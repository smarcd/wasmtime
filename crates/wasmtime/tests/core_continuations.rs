use wasmtime::*;

const LOCK_WAT: &str = r#"
(module
  (import "test" "poll" (func $poll (param i32)))
  (memory (export "memory") 1)
  (global $lock (export "lock") (mut i32) (i32.const 0))
  (global $marker (export "marker") (mut i32) (i32.const 0))
  (global $done (export "done") (mut i32) (i32.const 0))
  (global $cleanups (export "cleanups") (mut i32) (i32.const 0))
  (global $exhausted (export "exhausted") (mut i32) (i32.const 0))
  (func $unlock
    i32.const 202 global.set $marker
    i32.const 0 i32.const 202 i32.store
    i32.const 0 global.set $lock)
  (func (export "owner") (param $unlocked i32)
    i32.const 1 global.set $lock
    i32.const 101 global.set $marker
    i32.const 0 i32.const 101 i32.store
    local.get $unlocked if call $unlock end
    i32.const 0 call $poll
    call $unlock
    i32.const 1 global.set $done)
  (func (export "cleanup") (local $attempts i32)
    (loop $retry
      global.get $lock
      if
        local.get $attempts i32.const 1 i32.add local.tee $attempts
        i32.const 4 i32.ge_u
        if
          i32.const 1 global.set $exhausted
          unreachable
        end
        i32.const 1 call $poll
        br $retry
      end)
    global.get $marker i32.const 202 i32.ne if unreachable end
    i32.const 0 i32.load i32.const 202 i32.ne if unreachable end
    global.get $cleanups i32.const 1 i32.add global.set $cleanups))
"#;

struct Snapshot {
    result: Result<()>,
    lock: i32,
    marker: i32,
    done: i32,
    cleanups: i32,
    exhausted: i32,
    memory_word: i32,
}

fn snapshot(store: &mut Store<()>, instance: Instance, result: Result<()>) -> Snapshot {
    fn global(store: &mut Store<()>, instance: Instance, name: &str) -> i32 {
        instance
            .get_global(&mut *store, name)
            .unwrap()
            .get(store)
            .i32()
            .unwrap()
    }
    let memory = instance.get_memory(&mut *store, "memory").unwrap();
    let memory_word = i32::from_le_bytes(memory.data(&*store)[..4].try_into().unwrap());
    Snapshot {
        result,
        lock: global(store, instance, "lock"),
        marker: global(store, instance, "marker"),
        done: global(store, instance, "done"),
        cleanups: global(store, instance, "cleanups"),
        exhausted: global(store, instance, "exhausted"),
        memory_word,
    }
}

async fn run_nested(unlocked: bool) -> Result<Snapshot> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let module = Module::new(&engine, LOCK_WAT)?;
    let mut linker = Linker::new(&engine);
    linker.func_wrap_async(
        "test",
        "poll",
        |mut caller: Caller<'_, ()>, (phase,): (i32,)| {
            Box::new(async move {
                if phase == 0 {
                    let cleanup = caller.get_export("cleanup").unwrap().into_func().unwrap();
                    cleanup
                        .typed::<(), ()>(&caller)?
                        .call_async(&mut caller, ())
                        .await?;
                } else {
                    assert_eq!(phase, 1);
                    tokio::task::yield_now().await;
                }
                Ok(())
            })
        },
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let owner = instance.get_typed_func::<i32, ()>(&mut store, "owner")?;
    let result = owner.call_async(&mut store, i32::from(unlocked)).await;
    Ok(snapshot(&mut store, instance, result))
}

async fn run_scenario(unlocked: bool) -> Result<Snapshot> {
    run_owned(unlocked).await
}

fn assert_progress(s: Snapshot) -> Result<()> {
    assert_eq!(
        s.exhausted, 0,
        "cleanup exhausted the lock budget before owner resumed"
    );
    s.result?;
    assert_eq!(
        (s.lock, s.marker, s.done, s.cleanups, s.memory_word),
        (0, 202, 1, 1, 202)
    );
    Ok(())
}

#[tokio::test]
async fn core_continuations_unlocked_control() -> Result<()> {
    assert_progress(run_scenario(true).await?)
}

#[tokio::test]
async fn core_continuations_owner_lock_progress() -> Result<()> {
    assert_progress(run_scenario(false).await?)
}

fn finish_ok<T: Send + 'static>(store: &mut Store<T>, call: CoreCallId) -> Result<Vec<CoreValue>> {
    match store.take_core_call_outcome(call)? {
        CoreCallOutcome::Returned(values) => Ok(values),
        CoreCallOutcome::Trapped(error) => Err(error),
        #[cfg(feature = "gc")]
        CoreCallOutcome::Exception { error, .. } => Err(error),
        CoreCallOutcome::Cancelled => panic!("unexpected cancellation"),
    }
}

fn error<T>(result: Result<T>, expected: CoreCallError) {
    let Err(error) = result else {
        panic!("expected {expected:?}")
    };
    assert_eq!(
        error.downcast_ref::<CoreCallError>(),
        Some(&expected),
        "{error:?}"
    );
}

fn wait(event: CoreCallEvent, id: CoreCallId) -> CoreHostWaitId {
    let CoreCallEvent::Suspended { call, wait, .. } = event else {
        panic!("expected suspension")
    };
    assert_eq!(call, id);
    wait
}

fn poll_import<T: Send + 'static>(linker: &mut Linker<T>, engine: &Engine) -> Result<()> {
    linker.func_new_suspending(
        "test",
        "poll",
        FuncType::new(engine, [ValType::I32], []),
        |_, args| {
            let [CoreValue::I32(token)] = args else {
                unreachable!()
            };
            Ok(CoreHostStep::Suspend(*token as u64))
        },
    )?;
    Ok(())
}

async fn run_owned(unlocked: bool) -> Result<Snapshot> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    let module = Module::new(&engine, LOCK_WAT)?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let owner = instance.get_func(&mut store, "owner").unwrap();
    let cleanup = instance.get_func(&mut store, "cleanup").unwrap();
    let a = store.start_core_call(owner, vec![CoreValue::I32(i32::from(unlocked))])?;
    let a_wait = wait(store.resume_core_call(a)?.await?, a);
    let c = store.start_core_call(cleanup, vec![])?;
    let event = store.resume_core_call(c)?.await?;
    let c_wait = if unlocked {
        assert_eq!(event, CoreCallEvent::Finished { call: c });
        None
    } else {
        Some(wait(event, c))
    };
    store.complete_core_host_wait(a_wait, CoreHostResult::Return(vec![]))?;
    assert_eq!(
        store.resume_core_call(a)?.await?,
        CoreCallEvent::Finished { call: a }
    );
    if let Some(wait) = c_wait {
        store.complete_core_host_wait(wait, CoreHostResult::Return(vec![]))?;
        assert_eq!(
            store.resume_core_call(c)?.await?,
            CoreCallEvent::Finished { call: c }
        );
    }
    assert!(finish_ok(&mut store, a)?.is_empty());
    assert!(finish_ok(&mut store, c)?.is_empty());
    Ok(snapshot(&mut store, instance, Ok(())))
}

#[tokio::test]
async fn core_continuations_ordinary_nested_stays_exclusive() -> Result<()> {
    let s = run_nested(false).await?;
    assert_eq!((s.exhausted, s.done, s.cleanups), (1, 0, 0));
    assert!(s.result.is_err());
    Ok(())
}

#[tokio::test]
async fn core_continuations_host_wait_progress() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    linker.func_new_suspending(
        "test",
        "wait",
        FuncType::new(&engine, [], [ValType::I32]),
        |_, _| Ok(CoreHostStep::Suspend(4)),
    )?;
    let module = Module::new(
        &engine,
        r#"(module
      (import "test" "poll" (func $poll (param i32)))
      (import "test" "wait" (func $wait (result i32)))
      (global $lock (mut i32) (i32.const 0))
      (global $count (export "count") (mut i32) (i32.const 0))
      (func (export "a") i32.const 1 global.set $lock i32.const 0 call $poll i32.const 0 global.set $lock)
      (func (export "c") call $wait i32.const 7 i32.ne if unreachable end
        global.get $lock if unreachable end i32.const 1 global.set $count))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let a_func = instance.get_func(&mut store, "a").unwrap();
    let c_func = instance.get_func(&mut store, "c").unwrap();
    let a = store.start_core_call(a_func, vec![])?;
    let aw = wait(store.resume_core_call(a)?.await?, a);
    let c = store.start_core_call(c_func, vec![])?;
    let cw = wait(store.resume_core_call(c)?.await?, c);
    error(
        store.complete_core_host_wait(cw, CoreHostResult::Return(vec![])),
        CoreCallError::TypeMismatch,
    );
    store.complete_core_host_wait(aw, CoreHostResult::Return(vec![]))?;
    assert_eq!(
        store.resume_core_call(a)?.await?,
        CoreCallEvent::Finished { call: a }
    );
    store.complete_core_host_wait(cw, CoreHostResult::Return(vec![CoreValue::I32(7)]))?;
    assert_eq!(
        store.resume_core_call(c)?.await?,
        CoreCallEvent::Finished { call: c }
    );
    finish_ok(&mut store, a)?;
    finish_ok(&mut store, c)?;
    assert_eq!(
        instance
            .get_global(&mut store, "count")
            .unwrap()
            .get(&mut store)
            .i32(),
        Some(1)
    );
    Ok(())
}

const TWICE: &str = r#"(module (import "test" "poll" (func $poll (param i32)))
    (func (export "run") i32.const 1 call $poll i32.const 2 call $poll))"#;

#[tokio::test]
async fn core_continuations_wait_identity() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut other = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    let module = Module::new(&engine, TWICE)?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    error(
        other.start_core_call(func, vec![]),
        CoreCallError::WrongStore,
    );
    let id = store.start_core_call(func, vec![])?;
    error(store.take_core_call_outcome(id), CoreCallError::NotFinished);
    let w1 = wait(store.resume_core_call(id)?.await?, id);
    error(
        other.complete_core_host_wait(w1, CoreHostResult::Return(vec![])),
        CoreCallError::WrongStore,
    );
    store.complete_core_host_wait(w1, CoreHostResult::Return(vec![]))?;
    error(
        store.complete_core_host_wait(w1, CoreHostResult::Return(vec![])),
        CoreCallError::StaleWait,
    );
    let w2 = wait(store.resume_core_call(id)?.await?, id);
    assert_ne!(w1, w2);
    error(
        store.complete_core_host_wait(w1, CoreHostResult::Return(vec![])),
        CoreCallError::StaleWait,
    );
    error(store.resume_core_call(id), CoreCallError::WaitingForHost);
    error(
        store.complete_core_host_wait(w2, CoreHostResult::Trap(ThrownException.into())),
        CoreCallError::UnrootedException,
    );
    store.complete_core_host_wait(w2, CoreHostResult::Return(vec![]))?;
    assert_eq!(
        store.resume_core_call(id)?.await?,
        CoreCallEvent::Finished { call: id }
    );
    error(store.resume_core_call(id), CoreCallError::AlreadyFinished);
    finish_ok(&mut store, id)?;
    let next = store.start_core_call(func, vec![])?;
    assert_ne!(next, id);
    error(store.resume_core_call(id), CoreCallError::StaleCall);
    error(
        store.complete_core_host_wait(w2, CoreHostResult::Return(vec![])),
        CoreCallError::StaleCall,
    );
    store.cancel_core_call(next)?;
    Ok(())
}

#[tokio::test]
async fn core_continuations_scalar_validation() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    let values = vec![
        CoreValue::I32(-1),
        CoreValue::I64(i64::MIN),
        CoreValue::F32(0x7fc01234),
        CoreValue::F64(0x7ff8000000001234),
        CoreValue::V128(u128::MAX),
    ];
    let expected = values.clone();
    let types = [
        ValType::I32,
        ValType::I64,
        ValType::F32,
        ValType::F64,
        ValType::V128,
    ];
    linker.func_new_suspending(
        "test",
        "poll",
        FuncType::new(&engine, types.clone(), types),
        move |_, args| {
            assert_eq!(args, expected);
            Ok(CoreHostStep::Suspend(9))
        },
    )?;
    error(
        linker.func_new_suspending(
            "test",
            "bad",
            FuncType::new(&engine, [ValType::EXTERNREF], []),
            |_, _| panic!("must reject signature"),
        ),
        CoreCallError::NonScalarType,
    );
    let module = Module::new(
        &engine,
        r#"(module
      (import "test" "poll" (func $poll (param i32 i64 f32 f64 v128) (result i32 i64 f32 f64 v128)))
      (func (export "run") (param i32 i64 f32 f64 v128) (result i32 i64 f32 f64 v128)
        local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 call $poll)
      (func (export "bad") (param funcref)))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let bad = instance.get_func(&mut store, "bad").unwrap();
    error(
        store.start_core_call(bad, vec![]),
        CoreCallError::NonScalarType,
    );
    let func = instance.get_func(&mut store, "run").unwrap();
    error(
        store.start_core_call(func, vec![]),
        CoreCallError::TypeMismatch,
    );
    let id = store.start_core_call(func, values.clone())?;
    let w = wait(store.resume_core_call(id)?.await?, id);
    error(
        store.complete_core_host_wait(w, CoreHostResult::Return(vec![])),
        CoreCallError::TypeMismatch,
    );
    let mut wrong = values.clone();
    wrong[0] = CoreValue::I64(-1);
    error(
        store.complete_core_host_wait(w, CoreHostResult::Return(wrong)),
        CoreCallError::TypeMismatch,
    );
    store.complete_core_host_wait(w, CoreHostResult::Return(values.clone()))?;
    assert_eq!(
        store.resume_core_call(id)?.await?,
        CoreCallEvent::Finished { call: id }
    );
    assert_eq!(finish_ok(&mut store, id)?, values);
    Ok(())
}

use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};
struct Probe(Arc<AtomicUsize>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn core_continuations_exclusive_ancestor() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    let d = drops.clone();
    linker.func_wrap_async(
        "test",
        "outer",
        move |mut caller: Caller<'_, ()>, (): ()| {
            let probe = Probe(d.clone());
            Box::new(async move {
                let _probe = probe;
                let child = caller.get_export("child").unwrap().into_func().unwrap();
                child
                    .typed::<(), ()>(&caller)?
                    .call_async(&mut caller, ())
                    .await
            })
        },
    )?;
    let module = Module::new(
        &engine,
        r#"(module
      (import "test" "poll" (func $poll (param i32)))
      (import "test" "outer" (func $outer))
      (func (export "child") i32.const 0 call $poll)
      (func (export "run") call $outer))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    let id = store.start_core_call(func, vec![])?;
    assert_eq!(
        store.resume_core_call(id)?.await?,
        CoreCallEvent::Finished { call: id }
    );
    error(finish_ok(&mut store, id), CoreCallError::ExclusiveHostFrame);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let child = instance.get_func(&mut store, "child").unwrap();
    let id = store.start_core_call(child, vec![])?;
    let w = wait(store.resume_core_call(id)?.await?, id);
    store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
    store.resume_core_call(id)?.await?;
    finish_ok(&mut store, id)?;
    // A synchronous user frame cannot enter async-required Wasm, even if it
    // holds a non-Send local. No future or forbidden Store alias is involved.
    linker.func_wrap("test", "sync", |mut caller: Caller<'_, ()>| -> Result<()> {
        let local = std::rc::Rc::new(1);
        let func = caller.get_export("child").unwrap().into_func().unwrap();
        assert!(
            func.typed::<(), ()>(&caller)?
                .call(&mut caller, ())
                .is_err()
        );
        assert_eq!(*local, 1);
        Ok(())
    })?;
    let module = Module::new(
        &engine,
        r#"(module
      (import "test" "poll" (func $poll (param i32))) (import "test" "sync" (func $sync))
      (func (export "child") i32.const 0 call $poll) (func (export "run") call $sync))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    let id = store.start_core_call(func, vec![])?;
    store.resume_core_call(id)?.await?;
    finish_ok(&mut store, id)?;
    Ok(())
}

#[tokio::test]
async fn core_continuations_suspend_outside_root() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    linker.func_new_suspending(
        "test",
        "poll",
        FuncType::new(&engine, [ValType::I32], []),
        |_, args| {
            Ok(if args == [CoreValue::I32(0)] {
                CoreHostStep::Return(vec![])
            } else {
                CoreHostStep::Suspend(1)
            })
        },
    )?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "poll" (func $poll (param i32))) (func (export "run") (param i32) local.get 0 call $poll))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_typed_func::<i32, ()>(&mut store, "run")?;
    func.call_async(&mut store, 0).await?;
    error(
        func.call_async(&mut store, 1).await,
        CoreCallError::NoActiveCoreCall,
    );
    Ok(())
}

#[tokio::test]
async fn core_continuations_keep_store_pending() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut linker = Linker::new(&engine);
    let d = drops.clone();
    linker.func_wrap_async("test", "pending", move |_: Caller<'_, ()>, (): ()| {
        let probe = Probe(d.clone());
        Box::new(async move {
            let _probe = probe;
            std::future::pending::<()>().await;
            Ok(())
        })
    })?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "pending" (func $pending)) (func (export "run") call $pending) (func (export "ok")))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    let id = store.start_core_call(func, vec![])?;
    let mut future = Box::pin(store.resume_core_call(id)?);
    assert!(matches!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    drop(future);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(matches!(
        store.take_core_call_outcome(id)?,
        CoreCallOutcome::Cancelled
    ));
    let func = instance.get_func(&mut store, "ok").unwrap();
    let id = store.start_core_call(func, vec![])?;
    store.resume_core_call(id)?.await?;
    finish_ok(&mut store, id)?;
    Ok(())
}

#[tokio::test]
async fn core_continuations_cancel_released() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, 0);
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    linker.func_wrap("test", "effect", |mut caller: Caller<'_, usize>| {
        *caller.data_mut() += 1
    })?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "poll" (func $poll (param i32)))
      (import "test" "effect" (func $effect)) (func (export "run") i32.const 0 call $poll call $effect))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    for _ in 0..8 {
        for phase in 0..3 {
            let id = store.start_core_call(func, vec![])?;
            if phase > 0 {
                let w = wait(store.resume_core_call(id)?.await?, id);
                if phase == 2 {
                    store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
                }
            }
            store.cancel_core_call(id)?;
            error(store.cancel_core_call(id), CoreCallError::AlreadyFinished);
            assert!(matches!(
                store.take_core_call_outcome(id)?,
                CoreCallOutcome::Cancelled
            ));
            error(store.take_core_call_outcome(id), CoreCallError::StaleCall);
        }
    }
    assert_eq!(*store.data(), 0);
    Ok(())
}

#[tokio::test]
async fn core_continuations_panic_restoration() -> Result<()> {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    let d = drops.clone();
    linker.func_new_suspending(
        "test",
        "panic",
        FuncType::new(&engine, [], []),
        move |_: Caller<'_, ()>, _| {
            let _probe = Probe(d.clone());
            panic!("suspending callback panic")
        },
    )?;
    let d = drops.clone();
    linker.func_wrap_async("test", "async_panic", move |_: Caller<'_, ()>, (): ()| {
        let probe = Probe(d.clone());
        Box::new(async move {
            let _probe = probe;
            panic!("async callback panic");
            #[allow(
                unreachable_code,
                reason = "panic fixture still needs the async Result type"
            )]
            Ok(())
        })
    })?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "poll" (func $poll (param i32)))
      (import "test" "panic" (func $panic)) (import "test" "async_panic" (func $async_panic))
      (func (export "sibling") i32.const 0 call $poll)
      (func (export "panic") call $panic) (func (export "async_panic") call $async_panic))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    for (n, name) in ["panic", "async_panic"].into_iter().enumerate() {
        let func = instance.get_func(&mut store, "sibling").unwrap();
        let sibling = store.start_core_call(func, vec![])?;
        let w = wait(store.resume_core_call(sibling)?.await?, sibling);
        let func = instance.get_func(&mut store, name).unwrap();
        let id = store.start_core_call(func, vec![])?;
        let mut future = Box::pin(store.resume_core_call(id)?);
        assert!(
            catch_unwind(AssertUnwindSafe(|| future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))))
            .is_err()
        );
        drop(future);
        assert_eq!(drops.load(Ordering::SeqCst), n + 1);
        assert!(matches!(
            store.take_core_call_outcome(id)?,
            CoreCallOutcome::Cancelled
        ));
        store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
        store.resume_core_call(sibling)?.await?;
        finish_ok(&mut store, sibling)?;
    }
    Ok(())
}

#[tokio::test]
async fn core_continuations_send_resume_and_drop() -> Result<()> {
    struct SendState {
        effects: usize,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for SendState {
        fn drop(&mut self) {
            assert_eq!(self.effects, 1, "cancelled sibling executed its effect");
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    let engine = Engine::default();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut store = Store::new(
        &engine,
        SendState {
            effects: 0,
            drops: drops.clone(),
        },
    );
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    linker.func_wrap("test", "effect", |mut caller: Caller<'_, SendState>| {
        caller.data_mut().effects += 1
    })?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "poll" (func $poll (param i32)))
      (import "test" "effect" (func $effect)) (func (export "run") i32.const 0 call $poll call $effect))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    let a = store.start_core_call(func, vec![])?;
    let aw = wait(store.resume_core_call(a)?.await?, a);
    let c = store.start_core_call(func, vec![])?;
    wait(store.resume_core_call(c)?.await?, c);
    std::thread::spawn(move || -> Result<()> {
        store.complete_core_host_wait(aw, CoreHostResult::Return(vec![]))?;
        let mut future = Box::pin(store.resume_core_call(a)?);
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(CoreCallEvent::Finished { .. }))
        ));
        drop(future);
        finish_ok(&mut store, a)?;
        assert_eq!(store.data().effects, 1);
        drop(store);
        Ok(())
    })
    .join()
    .unwrap()?;
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    Ok(())
}

#[cfg(feature = "gc-copying")]
mod gc_controls {
    use super::*;

    fn engine() -> Result<Engine> {
        let mut config = Config::new();
        config.collector(Collector::Copying).wasm_exceptions(true);
        Engine::new(&config)
    }

    struct Payload {
        id: i32,
        _probe: Probe,
    }
    fn gc_imports<T: Send + 'static>(
        linker: &mut Linker<T>,
        engine: &Engine,
        drops: &Arc<AtomicUsize>,
    ) -> Result<()> {
        poll_import(linker, engine)?;
        let drops = drops.clone();
        linker.func_wrap("test", "make", move |mut caller: Caller<'_, T>, id: i32| {
            ExternRef::new(
                &mut caller,
                Payload {
                    id,
                    _probe: Probe(drops.clone()),
                },
            )
            .map(Some)
        })?;
        linker.func_wrap(
            "test",
            "check",
            |mut caller: Caller<'_, T>, r: Option<Rooted<ExternRef>>, id: i32| -> Result<()> {
                assert_eq!(
                    r.unwrap()
                        .data(&mut caller)?
                        .unwrap()
                        .downcast_ref::<Payload>()
                        .unwrap()
                        .id,
                    id
                );
                Ok(())
            },
        )?;
        Ok(())
    }

    const ROOTS: &str = r#"(module
      (type $cell (struct (field i32))) (type $items (array (mut i32)))
      (import "test" "make" (func $make (param i32) (result externref)))
      (import "test" "check" (func $check (param externref i32)))
      (import "test" "poll" (func $poll (param i32)))
      (func (export "extern") (param i32)
        local.get 0 call $make local.get 0 call $poll local.get 0 call $check)
      (func (export "struct") (param i32)
        local.get 0 struct.new $cell local.get 0 call $poll
        struct.get $cell 0 local.get 0 i32.ne if unreachable end)
      (func (export "array") (param i32)
        local.get 0 i32.const 1 array.new $items local.get 0 call $poll
        i32.const 0 array.get $items local.get 0 i32.ne if unreachable end)
      (func (export "extern_local") (param i32) (local externref)
        local.get 0 call $make local.set 1 local.get 0 call $poll local.get 1 local.get 0 call $check)
      (func (export "struct_local") (param i32) (local (ref null $cell))
        local.get 0 struct.new $cell local.set 1 local.get 0 call $poll
        local.get 1 struct.get $cell 0 local.get 0 i32.ne if unreachable end)
      (func (export "array_local") (param i32) (local (ref null $items))
        local.get 0 i32.const 1 array.new $items local.set 1 local.get 0 call $poll
        local.get 1 i32.const 0 array.get $items local.get 0 i32.ne if unreachable end))"#;

    #[tokio::test]
    async fn core_continuations_gc_stacks() -> Result<()> {
        let engine = engine()?;
        let mut store = Store::new(&engine, ());
        let drops = Arc::new(AtomicUsize::new(0));
        let mut linker = Linker::new(&engine);
        gc_imports(&mut linker, &engine, &drops)?;
        let module = Module::new(&engine, ROOTS)?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        for (n, name) in [
            "extern",
            "struct",
            "array",
            "extern_local",
            "struct_local",
            "array_local",
        ]
        .into_iter()
        .enumerate()
        {
            let baseline = drops.load(Ordering::SeqCst);
            let func = instance.get_func(&mut store, name).unwrap();
            let a = store.start_core_call(func, vec![CoreValue::I32(111)])?;
            let aw = wait(store.resume_core_call(a)?.await?, a);
            let c = store.start_core_call(func, vec![CoreValue::I32(222)])?;
            let cw = wait(store.resume_core_call(c)?.await?, c);
            store.gc(None)?;
            // This check must precede resumption: a tracing mutation cannot be
            // allowed to dereference an intentionally collected guest root.
            let observed = drops.load(Ordering::SeqCst);
            if observed != baseline {
                // A missing-tracing mutation invalidates the saved guest stack.
                // Leak it before failing, so teardown cannot touch those roots.
                std::mem::forget(store);
                panic!("collected suspended operand/local: {observed} != {baseline}");
            }
            let order = if n % 2 == 0 {
                [(a, aw), (c, cw)]
            } else {
                [(c, cw), (a, aw)]
            };
            for (id, w) in order {
                store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
                assert_eq!(
                    store.resume_core_call(id)?.await?,
                    CoreCallEvent::Finished { call: id }
                );
                finish_ok(&mut store, id)?;
                store.gc(None)?;
            }
            assert_eq!(
                drops.load(Ordering::SeqCst),
                baseline + if name.starts_with("extern") { 2 } else { 0 }
            );
        }
        Ok(())
    }

    const EXCEPTIONS: &str = r#"(module
      (import "test" "make" (func $make (param i32) (result externref)))
      (import "test" "check" (func $check (param externref i32)))
      (import "test" "poll" (func $poll (param i32)))
      (tag $a (export "a_tag") (param i32)) (tag $c (export "c_tag") (param i64))
      (tag $r (export "ref_tag") (param externref))
      (func (export "a") i32.const 0 call $poll i32.const 11 throw $a)
      (func (export "c") i32.const 0 call $poll i64.const 22 throw $c)
      (func (export "refs") i32.const 33 call $make i32.const 0 call $poll throw $r)
      (func (export "caught")
        (block $caught (result exnref)
          (try_table (catch_all_ref $caught) i32.const 44 call $make throw $r)
          unreachable)
        i32.const 0 call $poll throw_ref)
      (func (export "ok")))"#;

    #[tokio::test]
    async fn core_continuations_exception_outcomes() -> Result<()> {
        let engine = engine()?;
        let mut store = Store::new(&engine, ());
        let drops = Arc::new(AtomicUsize::new(0));
        let mut linker = Linker::new(&engine);
        gc_imports(&mut linker, &engine, &drops)?;
        let module = Module::new(&engine, EXCEPTIONS)?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let mut ids = Vec::new();
        for name in ["a", "c", "refs"] {
            let func = instance.get_func(&mut store, name).unwrap();
            let id = store.start_core_call(func, vec![])?;
            let w = wait(store.resume_core_call(id)?.await?, id);
            store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
            assert_eq!(
                store.resume_core_call(id)?.await?,
                CoreCallEvent::Finished { call: id }
            );
            assert!(store.take_pending_exception().is_none());
            ids.push(id);
        }
        store.gc(None)?;
        let observed = drops.load(Ordering::SeqCst);
        if observed != 0 {
            // Do not extract or dispose an intentionally untraced VMGcRef.
            std::mem::forget(store);
            panic!("collected pending outcome payload: {observed} != 0");
        }
        let func = instance.get_func(&mut store, "ok").unwrap();
        let ok = store.start_core_call(func, vec![])?;
        store.resume_core_call(ok)?.await?;
        finish_ok(&mut store, ok)?;
        for (id, name) in ids.into_iter().zip(["a_tag", "c_tag", "ref_tag"]).rev() {
            let CoreCallOutcome::Exception { exception, error } =
                store.take_core_call_outcome(id)?
            else {
                panic!("missing exception outcome")
            };
            assert!(
                !error.is::<ThrownException>(),
                "owned outcome must not carry pending-slot authority"
            );
            #[cfg(feature = "backtrace")]
            assert!(
                error.is::<WasmBacktrace>(),
                "lost original wasm backtrace: {error:?}"
            );
            let mut scope = RootScope::new(&mut store);
            let exn = exception.to_rooted(&mut scope);
            let tag = instance.get_tag(&mut scope, name).unwrap();
            assert!(Tag::eq(&exn.tag(&mut scope)?, &tag, &scope));
            let field = exn.field(&mut scope, 0)?;
            match name {
                "a_tag" => assert_eq!(field.i32(), Some(11)),
                "c_tag" => assert_eq!(field.i64(), Some(22)),
                _ => assert_eq!(
                    field
                        .unwrap_externref()
                        .unwrap()
                        .data(&scope)?
                        .unwrap()
                        .downcast_ref::<Payload>()
                        .unwrap()
                        .id,
                    33
                ),
            }
        }
        store.gc(None)?;
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn core_continuations_caught_exception_stack() -> Result<()> {
        let engine = engine()?;
        let mut store = Store::new(&engine, ());
        let drops = Arc::new(AtomicUsize::new(0));
        let mut linker = Linker::new(&engine);
        gc_imports(&mut linker, &engine, &drops)?;
        linker.func_wrap("test", "gc", |mut caller: Caller<'_, ()>| caller.gc(None))?;
        let module = Module::new(&engine, EXCEPTIONS)?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let func = instance.get_func(&mut store, "caught").unwrap();
        let id = store.start_core_call(func, vec![])?;
        let w = wait(store.resume_core_call(id)?.await?, id);
        let gc_module = Module::new(
            &engine,
            r#"(module (import "test" "gc" (func $gc)) (func (export "run") call $gc))"#,
        )?;
        let gc_instance = linker.instantiate_async(&mut store, &gc_module).await?;
        let func = gc_instance.get_func(&mut store, "run").unwrap();
        let sibling = store.start_core_call(func, vec![])?;
        store.resume_core_call(sibling)?.await?;
        finish_ok(&mut store, sibling)?;
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "collected caught exception on stack"
        );
        store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
        store.resume_core_call(id)?.await?;
        assert!(store.take_pending_exception().is_none());
        let CoreCallOutcome::Exception { exception, .. } = store.take_core_call_outcome(id)? else {
            panic!("missing exception")
        };
        {
            let mut scope = RootScope::new(&mut store);
            let exn = exception.to_rooted(&mut scope);
            let tag = instance.get_tag(&mut scope, "ref_tag").unwrap();
            assert!(Tag::eq(&exn.tag(&mut scope)?, &tag, &scope));
            assert_eq!(
                exn.field(&mut scope, 0)?
                    .unwrap_externref()
                    .unwrap()
                    .data(&scope)?
                    .unwrap()
                    .downcast_ref::<Payload>()
                    .unwrap()
                    .id,
                44
            );
        }
        drop(exception);
        store.gc(None)?;
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn core_continuations_cancel_gc_roots() -> Result<()> {
        for names in [["refs", "caught"], ["caught", "refs"]] {
            let engine = engine()?;
            let mut store = Store::new(&engine, ());
            let drops = Arc::new(AtomicUsize::new(0));
            let mut linker = Linker::new(&engine);
            gc_imports(&mut linker, &engine, &drops)?;
            let module = Module::new(&engine, EXCEPTIONS)?;
            let instance = linker.instantiate_async(&mut store, &module).await?;
            let mut ids = Vec::new();
            for name in names {
                let func = instance.get_func(&mut store, name).unwrap();
                let id = store.start_core_call(func, vec![])?;
                let w = wait(store.resume_core_call(id)?.await?, id);
                ids.push((id, w));
            }
            store.cancel_core_call(ids[0].0)?;
            store.gc(None)?;
            assert_eq!(
                drops.load(Ordering::SeqCst),
                1,
                "cancelled root remains or live sibling lost"
            );
            store.complete_core_host_wait(ids[1].1, CoreHostResult::Return(vec![]))?;
            store.resume_core_call(ids[1].0)?.await?;
            assert!(matches!(
                store.take_core_call_outcome(ids[1].0)?,
                CoreCallOutcome::Exception { .. }
            ));
            store.gc(None)?;
            assert_eq!(drops.load(Ordering::SeqCst), 2);
        }
        Ok(())
    }

    #[tokio::test]
    async fn core_continuations_gc_during_cancellation() -> Result<()> {
        struct GcOnDrop<'a> {
            caller: Caller<'a, ()>,
            collections: Arc<AtomicUsize>,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for GcOnDrop<'_> {
            fn drop(&mut self) {
                self.caller.gc(None).unwrap();
                assert_eq!(
                    self.drops.load(Ordering::SeqCst),
                    0,
                    "GC during unwind lost active or sibling root"
                );
                self.collections.fetch_add(1, Ordering::SeqCst);
            }
        }
        let engine = engine()?;
        let mut store = Store::new(&engine, ());
        let drops = Arc::new(AtomicUsize::new(0));
        let collections = Arc::new(AtomicUsize::new(0));
        let mut linker = Linker::new(&engine);
        gc_imports(&mut linker, &engine, &drops)?;
        let d = drops.clone();
        let c = collections.clone();
        linker.func_wrap_async("test", "pending", move |caller: Caller<'_, ()>, (): ()| {
            let guard = GcOnDrop {
                caller,
                collections: c.clone(),
                drops: d.clone(),
            };
            Box::new(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                Ok(())
            })
        })?;
        let module = Module::new(
            &engine,
            r#"(module
          (import "test" "make" (func $make (param i32) (result externref)))
          (import "test" "check" (func $check (param externref i32)))
          (import "test" "poll" (func $poll (param i32)))
          (import "test" "pending" (func $pending))
          (func (export "run") (param $id i32) (param $pending i32)
            local.get $id call $make
            local.get $pending if call $pending else i32.const 0 call $poll end
            local.get $id call $check))"#,
        )?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let func = instance.get_func(&mut store, "run").unwrap();
        let sibling = store.start_core_call(func, vec![CoreValue::I32(111), CoreValue::I32(0)])?;
        let w = wait(store.resume_core_call(sibling)?.await?, sibling);
        let id = store.start_core_call(func, vec![CoreValue::I32(222), CoreValue::I32(1)])?;
        let mut future = Box::pin(store.resume_core_call(id)?);
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        drop(future);
        assert_eq!(collections.load(Ordering::SeqCst), 1);
        store.gc(None)?;
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
        store.resume_core_call(sibling)?.await?;
        finish_ok(&mut store, sibling)?;
        store.gc(None)?;
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        Ok(())
    }
    #[tokio::test]
    async fn core_continuations_exception_diagnostic_has_no_authority() -> Result<()> {
        let engine = engine()?;
        let mut store = Store::new(&engine, ());
        let drops = Arc::new(AtomicUsize::new(0));
        let mut linker = Linker::new(&engine);
        gc_imports(&mut linker, &engine, &drops)?;
        let module = Module::new(&engine, EXCEPTIONS)?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let func = instance.get_func(&mut store, "a").unwrap();
        let id = store.start_core_call(func, vec![])?;
        let w = wait(store.resume_core_call(id)?.await?, id);
        store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
        store.resume_core_call(id)?.await?;
        let CoreCallOutcome::Exception {
            error: diagnostic, ..
        } = store.take_core_call_outcome(id)?
        else {
            panic!("missing exception")
        };
        assert!(!diagnostic.is::<ThrownException>());
        let saved = std::sync::Mutex::new(Some(diagnostic));
        linker.func_wrap("test", "relay", move || -> Result<()> {
            Err(saved.lock().unwrap().take().unwrap())
        })?;
        let module = Module::new(
            &engine,
            r#"(module
          (import "test" "relay" (func $relay)) (tag $tag (export "tag") (param i32))
          (global $caught (export "caught") (mut i32) (i32.const 0))
          (func (export "run")
            (block $handler (result i32)
              (try_table (catch $tag $handler) call $relay)
              i32.const 0)
            global.set $caught))"#,
        )?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let tag = instance.get_tag(&mut store, "tag").unwrap();
        let ty = ExnType::from_tag_type(&tag.ty(&store))?;
        let pre = ExnRefPre::new(&mut store, ty);
        {
            let mut scope = RootScope::new(&mut store);
            let exn = ExnRef::new(&mut scope, &pre, &tag, &[Val::I32(77)])?;
            assert!(
                scope
                    .as_context_mut()
                    .throw::<()>(exn)
                    .unwrap_err()
                    .is::<ThrownException>()
            );
        }
        let func = instance.get_typed_func::<(), ()>(&mut store, "run")?;
        let err = func.call_async(&mut store, ()).await.unwrap_err();
        assert!(!err.is::<ThrownException>());
        assert_eq!(
            instance
                .get_global(&mut store, "caught")
                .unwrap()
                .get(&mut store)
                .i32(),
            Some(0)
        );
        let pending = store
            .take_pending_exception()
            .expect("diagnostic consumed unrelated exception");
        assert_eq!(pending.field(&mut store, 0)?.i32(), Some(77));
        Ok(())
    }

    #[tokio::test]
    async fn core_continuations_pending_exception() -> Result<()> {
        let engine = engine()?;
        let mut store = Store::new(&engine, ());
        let tag = Tag::new(
            &mut store,
            &TagType::new(FuncType::new(&engine, [ValType::I32], [])),
        )?;
        let exn_ty = ExnType::from_tag_type(&tag.ty(&store))?;
        let pre = Arc::new(ExnRefPre::new(&mut store, exn_ty));
        let mut linker = Linker::new(&engine);
        let pre2 = pre.clone();
        linker.func_new_suspending(
            "test",
            "poll",
            FuncType::new(&engine, [], []),
            move |mut caller: Caller<'_, ()>, _| {
                let exn = ExnRef::new(&mut caller, &pre2, &tag, &[Val::I32(42)])?;
                let _ = caller.as_context_mut().throw::<()>(exn);
                Ok(CoreHostStep::Suspend(1))
            },
        )?;
        let module = Module::new(
            &engine,
            r#"(module (import "test" "poll" (func $poll)) (func (export "run") call $poll) (func (export "ok")))"#,
        )?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let func = instance.get_func(&mut store, "run").unwrap();
        let id = store.start_core_call(func, vec![])?;
        assert_eq!(
            store.resume_core_call(id)?.await?,
            CoreCallEvent::Finished { call: id }
        );
        assert!(store.take_pending_exception().is_none());
        let CoreCallOutcome::Exception { exception, .. } = store.take_core_call_outcome(id)? else {
            panic!("pending exception did not prevent suspension")
        };
        assert_eq!(exception.field(&mut store, 0)?.i32(), Some(42));
        let ok = instance.get_func(&mut store, "ok").unwrap();
        let id = store.start_core_call(ok, vec![])?;
        let exn = ExnRef::new(&mut store, &pre, &tag, &[Val::I32(77)])?;
        let _ = store.throw::<()>(exn);
        error(
            store.start_core_call(ok, vec![]),
            CoreCallError::PendingException,
        );
        error(store.resume_core_call(id), CoreCallError::PendingException);
        let pending = store.take_pending_exception().unwrap();
        assert_eq!(pending.field(&mut store, 0)?.i32(), Some(77));
        store.resume_core_call(id)?.await?;
        finish_ok(&mut store, id)?;
        Ok(())
    }

    #[tokio::test]
    async fn core_continuations_cancel_preserves_unrelated_exception() -> Result<()> {
        let engine = engine()?;
        let mut store = Store::new(&engine, ());
        let module = Module::new(
            &engine,
            r#"(module (tag $tag (export "tag") (param i32)) (func (export "ok")))"#,
        )?;
        let instance = Instance::new(&mut store, &module, &[])?;
        let func = instance.get_func(&mut store, "ok").unwrap();
        let id = store.start_core_call(func, vec![])?;
        let tag = instance.get_tag(&mut store, "tag").unwrap();
        let ty = ExnType::from_tag_type(&tag.ty(&store))?;
        let pre = ExnRefPre::new(&mut store, ty);
        let exn = ExnRef::new(&mut store, &pre, &tag, &[Val::I32(81)])?;
        let _ = store.throw::<()>(exn);
        error(store.cancel_core_call(id), CoreCallError::PendingException);
        let pending = store
            .take_pending_exception()
            .expect("cancel consumed unrelated pending exception");
        assert_eq!(pending.field(&mut store, 0)?.i32(), Some(81));
        store.cancel_core_call(id)?;
        assert!(matches!(
            store.take_core_call_outcome(id)?,
            CoreCallOutcome::Cancelled
        ));
        Ok(())
    }
    #[tokio::test]
    async fn core_continuations_nonexecuting_apis_preserve_pending_exception() -> Result<()> {
        let engine = engine()?;
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        linker.func_new_suspending(
            "test",
            "poll",
            FuncType::new(&engine, [], []),
            |_: Caller<'_, ()>, _| Ok(CoreHostStep::Suspend(1)),
        )?;
        let module = Module::new(
            &engine,
            r#"(module
                (import "test" "poll" (func $poll))
                (tag $tag (export "tag") (param i32))
                (func (export "wait") call $poll)
                (func (export "throw") i32.const 42 throw $tag))"#,
        )?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let func = instance.get_func(&mut store, "wait").unwrap();
        let waiting = store.start_core_call(func, vec![])?;
        let w = wait(store.resume_core_call(waiting)?.await?, waiting);
        let func = instance.get_func(&mut store, "throw").unwrap();
        let finished = store.start_core_call(func, vec![])?;
        store.resume_core_call(finished)?.await?;
        let tag = instance.get_tag(&mut store, "tag").unwrap();
        let ty = ExnType::from_tag_type(&tag.ty(&store))?;
        let pre = ExnRefPre::new(&mut store, ty);
        let exn = ExnRef::new(&mut store, &pre, &tag, &[Val::I32(81)])?;
        let _ = store.throw::<()>(exn);
        store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
        let CoreCallOutcome::Exception { exception, .. } =
            store.take_core_call_outcome(finished)?
        else {
            panic!("taking an outcome lost its own exception")
        };
        assert_eq!(exception.field(&mut store, 0)?.i32(), Some(42));
        let pending = store
            .take_pending_exception()
            .expect("nonexecuting API consumed unrelated pending exception");
        assert_eq!(pending.field(&mut store, 0)?.i32(), Some(81));
        store.resume_core_call(waiting)?.await?;
        finish_ok(&mut store, waiting)?;
        Ok(())
    }

    #[tokio::test]
    async fn core_continuations_drop_and_into_data() -> Result<()> {
        let engine = engine()?;
        for into_data in [false, true] {
            let drops = Arc::new(AtomicUsize::new(0));
            let mut store = Store::new(&engine, 123usize);
            let mut linker = Linker::new(&engine);
            gc_imports(&mut linker, &engine, &drops)?;
            let module = Module::new(&engine, EXCEPTIONS)?;
            let instance = linker.instantiate_async(&mut store, &module).await?;
            for n in 0..3 {
                let func = instance.get_func(&mut store, "refs").unwrap();
                let id = store.start_core_call(func, vec![])?;
                let w = wait(store.resume_core_call(id)?.await?, id);
                if n == 2 {
                    store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
                    store.resume_core_call(id)?.await?;
                    assert!(store.take_pending_exception().is_none());
                }
            }
            store.gc(None)?;
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            if into_data {
                assert_eq!(store.into_data(), 123);
            } else {
                drop(store);
            }
            assert_eq!(
                drops.load(Ordering::SeqCst),
                3,
                "teardown lost released/outcome payloads"
            );
        }
        Ok(())
    }
}

#[test]
fn core_continuations_forgotten_pending() -> Result<()> {
    const CHILD: &str = "WASMTIME_CORE_FORGET_CHILD";
    if std::env::var_os(CHILD).is_none() {
        for mode in ["drop", "into_data"] {
            let mut child = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "core_continuations_forgotten_pending",
                    "--nocapture",
                ])
                .env(CHILD, mode)
                .spawn()?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                if let Some(status) = child.try_wait()? {
                    assert!(status.success());
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill()?;
                    panic!("forgotten future child hung");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        return Ok(());
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        let engine = Engine::default();
        let mut store = Store::new(&engine, 1usize);
        let drops = Arc::new(AtomicUsize::new(0));
        let saved = Arc::new(std::sync::Mutex::new(None::<Waker>));
        let mut linker = Linker::new(&engine);
        poll_import(&mut linker, &engine)?;
        let d = drops.clone();
        let s = saved.clone();
        linker.func_wrap_async("test", "pending", move |_: Caller<'_, usize>, (): ()| {
            let probe = Probe(d.clone());
            let saved = s.clone();
            Box::new(async move {
                let _probe = probe;
                std::future::poll_fn(move |cx| {
                    *saved.lock().unwrap() = Some(cx.waker().clone());
                    Poll::<()>::Pending
                })
                .await;
                Ok(())
            })
        })?;
        let module = Module::new(
            &engine,
            r#"(module
          (import "test" "pending" (func $pending)) (import "test" "poll" (func $poll (param i32)))
          (memory (export "memory") 1) (global $g (export "g") (mut i32) (i32.const 0))
          (func (export "run") call $pending i32.const 99 global.set $g)
          (func (export "released") i32.const 0 call $poll)
          (func (export "ok") (result i32) global.get $g))"#,
        )?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let func = instance.get_func(&mut store, "released").unwrap();
        let released = store.start_core_call(func, vec![])?;
        let w = wait(store.resume_core_call(released)?.await?, released);
        let func = instance.get_func(&mut store, "run").unwrap();
        let id = store.start_core_call(func, vec![])?;
        let mut future = Box::pin(store.resume_core_call(id)?);
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        std::mem::forget(future);
        *store.data_mut() = 7;
        instance
            .get_memory(&mut store, "memory")
            .unwrap()
            .write(&mut store, 0, &[17])?;
        instance
            .get_global(&mut store, "g")
            .unwrap()
            .set(&mut store, Val::I32(23))?;
        error(store.resume_core_call(id), CoreCallError::CallRunning);
        error(store.cancel_core_call(id), CoreCallError::CallRunning);
        error(
            store.complete_core_host_wait(w, CoreHostResult::Return(vec![])),
            CoreCallError::CallRunning,
        );
        error(
            store.start_core_call(func, vec![]),
            CoreCallError::CallRunning,
        );
        saved.lock().unwrap().take().unwrap().wake();
        let ok = instance.get_typed_func::<(), i32>(&mut store, "ok")?;
        assert_eq!(ok.call_async(&mut store, ()).await?, 23);
        assert_eq!(
            instance
                .get_memory(&mut store, "memory")
                .unwrap()
                .data(&store)[0],
            17
        );
        if std::env::var(CHILD).unwrap() == "into_data" {
            assert_eq!(store.into_data(), 7);
        } else {
            drop(store);
        }
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "forgotten exclusive stack was recovered"
        );
        Ok(())
    })
}

#[tokio::test]
async fn core_continuations_unpolled_future() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    let module = Module::new(&engine, TWICE)?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    let id = store.start_core_call(func, vec![])?;
    drop(store.resume_core_call(id)?);
    std::mem::forget(store.resume_core_call(id)?);
    let w = wait(store.resume_core_call(id)?.await?, id);
    store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
    drop(store.resume_core_call(id)?);
    std::mem::forget(store.resume_core_call(id)?);
    let w = wait(store.resume_core_call(id)?.await?, id);
    store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
    assert_eq!(
        store.resume_core_call(id)?.await?,
        CoreCallEvent::Finished { call: id }
    );
    finish_ok(&mut store, id)?;
    Ok(())
}

#[tokio::test]
async fn core_continuations_pending_drop_borrows_data() -> Result<()> {
    struct Data {
        value: usize,
        alive: Arc<std::sync::atomic::AtomicBool>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Data {
        fn drop(&mut self) {
            assert_eq!(self.drops.load(Ordering::SeqCst), 1);
            self.alive.store(false, Ordering::SeqCst);
        }
    }
    struct BorrowProbe<'a> {
        value: &'a usize,
        alive: Arc<std::sync::atomic::AtomicBool>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for BorrowProbe<'_> {
        fn drop(&mut self) {
            assert!(
                self.alive.load(Ordering::SeqCst),
                "user data died before pending future destructor"
            );
            assert_eq!(*self.value, 321);
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    let engine = Engine::default();
    for into_data in [false, true] {
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let drops = Arc::new(AtomicUsize::new(0));
        let mut store = Store::new(
            &engine,
            Data {
                value: 321,
                alive,
                drops,
            },
        );
        let mut linker = Linker::new(&engine);
        linker.func_wrap_async("test", "pending", |caller: Caller<'_, Data>, (): ()| {
            Box::new(async move {
                let _probe = BorrowProbe {
                    value: &caller.data().value,
                    alive: caller.data().alive.clone(),
                    drops: caller.data().drops.clone(),
                };
                std::future::pending::<()>().await;
                Ok(())
            })
        })?;
        let module = Module::new(
            &engine,
            r#"(module (import "test" "pending" (func $pending)) (func (export "run") call $pending))"#,
        )?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let func = instance.get_func(&mut store, "run").unwrap();
        let id = store.start_core_call(func, vec![])?;
        let mut future = Box::pin(store.resume_core_call(id)?);
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        drop(future);
        if into_data {
            let data = store.into_data();
            assert_eq!(data.value, 321);
            drop(data);
        } else {
            drop(store);
        }
    }
    Ok(())
}

#[tokio::test]
async fn core_continuations_start_reentry() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    let nested = Module::new(
        &engine,
        r#"(module (import "test" "poll" (func $poll (param i32))) (func $start i32.const 0 call $poll) (start $start))"#,
    )?;
    let nested_linker = linker.clone();
    linker.func_wrap_async(
        "test",
        "start",
        move |mut caller: Caller<'_, ()>, (): ()| {
            let nested = nested.clone();
            let linker = nested_linker.clone();
            Box::new(async move {
                linker.instantiate_async(&mut caller, &nested).await?;
                Ok(())
            })
        },
    )?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "start" (func $start)) (func (export "run") call $start))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    let id = store.start_core_call(func, vec![])?;
    assert_eq!(
        store.resume_core_call(id)?.await?,
        CoreCallEvent::Finished { call: id }
    );
    error(finish_ok(&mut store, id), CoreCallError::ExclusiveHostFrame);
    Ok(())
}

#[cfg(feature = "call-hook")]
#[tokio::test]
async fn core_continuations_hook_reentry() -> Result<()> {
    struct Hook {
        target: CallHook,
        func: TypedFunc<(), ()>,
        hits: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl CallHookHandler<()> for Hook {
        async fn handle_call_event(
            &self,
            mut store: StoreContextMut<'_, ()>,
            event: CallHook,
        ) -> Result<()> {
            if std::mem::discriminant(&event) == std::mem::discriminant(&self.target) {
                self.hits.fetch_add(1, Ordering::SeqCst);
                error(
                    self.func.call_async(&mut store, ()).await,
                    CoreCallError::ExclusiveHostFrame,
                );
            }
            Ok(())
        }
    }
    let engine = Engine::default();
    for event in [
        CallHook::CallingWasm,
        CallHook::ReturningFromWasm,
        CallHook::CallingHost,
        CallHook::ReturningFromHost,
    ] {
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        poll_import(&mut linker, &engine)?;
        linker.func_wrap("test", "host", || {})?;
        let module = Module::new(
            &engine,
            r#"(module (import "test" "poll" (func $poll (param i32)))
          (import "test" "host" (func $host)) (func (export "child") i32.const 0 call $poll)
          (func (export "run") call $host))"#,
        )?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let func = instance.get_typed_func::<(), ()>(&mut store, "child")?;
        let hits = Arc::new(AtomicUsize::new(0));
        store.call_hook_async(Hook {
            target: event,
            func,
            hits: hits.clone(),
        });
        let func = instance.get_func(&mut store, "run").unwrap();
        let id = store.start_core_call(func, vec![])?;
        store.resume_core_call(id)?.await?;
        finish_ok(&mut store, id)?;
        assert!(hits.load(Ordering::SeqCst) > 0);
    }
    Ok(())
}

#[cfg(feature = "call-hook")]
#[tokio::test]
async fn core_continuations_suspending_hook_reentry() -> Result<()> {
    struct Hook {
        target: CallHook,
        func: TypedFunc<(), ()>,
        hits: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl CallHookHandler<()> for Hook {
        async fn handle_call_event(
            &self,
            mut store: StoreContextMut<'_, ()>,
            event: CallHook,
        ) -> Result<()> {
            if std::mem::discriminant(&event) == std::mem::discriminant(&self.target) {
                self.hits.fetch_add(1, Ordering::SeqCst);
                error(
                    self.func.call_async(&mut store, ()).await,
                    CoreCallError::ExclusiveHostFrame,
                );
            }
            Ok(())
        }
    }
    let engine = Engine::default();
    for event in [
        CallHook::CallingWasm,
        CallHook::ReturningFromWasm,
        CallHook::CallingHost,
        CallHook::ReturningFromHost,
    ] {
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        poll_import(&mut linker, &engine)?;
        linker.func_new_suspending("test", "host", FuncType::new(&engine, [], []), |_, _| {
            Ok(CoreHostStep::Suspend(7))
        })?;
        let module = Module::new(
            &engine,
            r#"(module (import "test" "poll" (func $poll (param i32)))
          (import "test" "host" (func $host)) (func (export "child") i32.const 0 call $poll)
          (func (export "run") call $host))"#,
        )?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let func = instance.get_typed_func::<(), ()>(&mut store, "child")?;
        let hits = Arc::new(AtomicUsize::new(0));
        store.call_hook_async(Hook {
            target: event,
            func,
            hits: hits.clone(),
        });
        let func = instance.get_func(&mut store, "run").unwrap();
        let id = store.start_core_call(func, vec![])?;
        let w = wait(store.resume_core_call(id)?.await?, id);
        store.complete_core_host_wait(w, CoreHostResult::Return(vec![]))?;
        store.resume_core_call(id)?.await?;
        finish_ok(&mut store, id)?;
        assert!(hits.load(Ordering::SeqCst) > 0);
    }
    Ok(())
}

#[cfg(feature = "pooling-allocator")]
#[tokio::test]
async fn core_continuations_stack_pool_exhaustion() -> Result<()> {
    let mut config = Config::new();
    let mut pool = PoolingAllocationConfig::default();
    pool.total_stacks(1)
        .total_memories(0)
        .total_tables(0)
        .total_core_instances(4);
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    let module = Module::new(&engine, TWICE)?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    let a = store.start_core_call(func, vec![])?;
    wait(store.resume_core_call(a)?.await?, a);
    let b = store.start_core_call(func, vec![])?;
    // Merely creating or discarding the future must not allocate a stack.
    drop(store.resume_core_call(b)?);
    assert!(store.resume_core_call(b)?.await.is_err());
    store.cancel_core_call(b)?;
    assert!(matches!(
        store.take_core_call_outcome(b)?,
        CoreCallOutcome::Cancelled
    ));
    store.cancel_core_call(a)?;
    let c = store.start_core_call(func, vec![])?;
    wait(store.resume_core_call(c)?.await?, c);
    store.cancel_core_call(c)?;
    Ok(())
}

#[tokio::test]
async fn core_continuations_direct_host_reentry() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    linker.func_wrap_async("test", "outer", |mut caller: Caller<'_, ()>, (): ()| {
        Box::new(async move {
            let host = caller.get_export("poll").unwrap().into_func().unwrap();
            host.typed::<i32, ()>(&caller)?
                .call_async(&mut caller, 0)
                .await
        })
    })?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "poll" (func $poll (param i32)))
      (import "test" "outer" (func $outer)) (export "poll" (func $poll)) (func (export "run") call $outer))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    let id = store.start_core_call(func, vec![])?;
    store.resume_core_call(id)?.await?;
    error(finish_ok(&mut store, id), CoreCallError::ExclusiveHostFrame);
    Ok(())
}

#[cfg(target_has_atomic = "64")]
#[tokio::test]
async fn core_continuations_epoch_reentry() -> Result<()> {
    let mut config = Config::new();
    config.epoch_interruption(true);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    store.set_epoch_deadline(100);
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "poll" (func $poll (param i32)))
      (func (export "child") i32.const 0 call $poll) (func (export "run")))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let child = instance.get_typed_func::<(), ()>(&mut store, "child")?;
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    store.epoch_deadline_callback(move |mut store| {
        h.fetch_add(1, Ordering::SeqCst);
        store.set_epoch_deadline(100);
        let mut nested = Box::pin(child.call_async(&mut store, ()));
        let Poll::Ready(result) = nested
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("epoch descendant retained a nested fiber")
        };
        error(result, CoreCallError::ExclusiveHostFrame);
        Ok(UpdateDeadline::Continue(100))
    });
    store.set_epoch_deadline(0);
    let func = instance.get_func(&mut store, "run").unwrap();
    let id = store.start_core_call(func, vec![])?;
    store.resume_core_call(id)?.await?;
    finish_ok(&mut store, id)?;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    Ok(())
}

#[cfg(feature = "debug")]
#[tokio::test]
async fn core_continuations_debug_reentry() -> Result<()> {
    #[derive(Clone)]
    struct Handler {
        child: TypedFunc<(), ()>,
        hits: Arc<AtomicUsize>,
    }
    impl DebugHandler for Handler {
        type Data = ();
        async fn handle(&self, mut store: StoreContextMut<'_, ()>, _: DebugEvent<'_>) {
            if self.hits.fetch_add(1, Ordering::SeqCst) == 0 {
                error(
                    self.child.call_async(&mut store, ()).await,
                    CoreCallError::ExclusiveHostFrame,
                );
            }
        }
    }
    let mut config = Config::new();
    config.guest_debug(true);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    linker.func_wrap("test", "error", || -> Result<()> {
        bail!("trigger debugger")
    })?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "poll" (func $poll (param i32)))
      (import "test" "error" (func $error)) (func (export "child") i32.const 0 call $poll)
      (func (export "run") call $error))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let child = instance.get_typed_func::<(), ()>(&mut store, "child")?;
    let hits = Arc::new(AtomicUsize::new(0));
    store.set_debug_handler(Handler {
        child,
        hits: hits.clone(),
    });
    let func = instance.get_func(&mut store, "run").unwrap();
    let id = store.start_core_call(func, vec![])?;
    store.resume_core_call(id)?.await?;
    let diagnostic = finish_ok(&mut store, id).unwrap_err();
    assert!(
        format!("{diagnostic:#}").contains("trigger debugger"),
        "{diagnostic:?}"
    );
    assert!(hits.load(Ordering::SeqCst) >= 1);
    Ok(())
}

#[cfg(feature = "component-model")]
#[tokio::test]
async fn core_continuations_component_reentry() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    poll_import(&mut linker, &engine)?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "poll" (func $poll (param i32)))
      (func (export "child") i32.const 0 call $poll))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let child = instance.get_typed_func::<(), ()>(&mut store, "child")?;
    let component = component::Component::new(
        &engine,
        r#"(component
      (import "poll" (func $poll)) (core func $poll (canon lower (func $poll)))
      (core module $m (import "" "poll" (func $poll)) (func (export "run") call $poll))
      (core instance $i (instantiate $m (with "" (instance (export "poll" (func $poll))))))
      (func (export "run") (canon lift (core func $i "run"))))"#,
    )?;
    let mut components = component::Linker::new(&engine);
    components.root().func_wrap_async(
        "poll",
        move |mut store: StoreContextMut<'_, ()>, (): ()| {
            let child = child.clone();
            Box::new(async move { child.call_async(&mut store, ()).await })
        },
    )?;
    let instance = components.instantiate_async(&mut store, &component).await?;
    let component_func = instance.get_typed_func::<(), ()>(&mut store, "run")?;
    linker.func_wrap_async(
        "test",
        "outer",
        move |mut caller: Caller<'_, ()>, (): ()| {
            Box::new(async move { component_func.call_async(&mut caller, ()).await })
        },
    )?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "outer" (func $outer)) (func (export "run") call $outer))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    let id = store.start_core_call(func, vec![])?;
    store.resume_core_call(id)?.await?;
    error(finish_ok(&mut store, id), CoreCallError::ExclusiveHostFrame);
    Ok(())
}

#[tokio::test]
async fn core_continuations_cancellation_drop_panic() -> Result<()> {
    struct PanicDrop;
    impl Drop for PanicDrop {
        fn drop(&mut self) {
            panic!("future destructor panic");
        }
    }
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    linker.func_wrap_async("test", "pending", |_: Caller<'_, ()>, (): ()| {
        Box::new(async {
            let _probe = PanicDrop;
            std::future::pending::<()>().await;
            Ok(())
        })
    })?;
    let module = Module::new(
        &engine,
        r#"(module (import "test" "pending" (func $pending)) (func (export "run") call $pending) (func (export "ok")))"#,
    )?;
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let func = instance.get_func(&mut store, "run").unwrap();
    let id = store.start_core_call(func, vec![])?;
    let mut future = Box::pin(store.resume_core_call(id)?);
    assert!(matches!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future)))
        .expect_err("user future destructor panic was swallowed");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"future destructor panic")
    );
    let outcome = store.take_core_call_outcome(id);
    if outcome.is_err() {
        // Keep the failed assertion observable without a second destructor
        // panic obscuring it. This branch is only the regression's failure path.
        std::mem::forget(store);
        panic!("panicking future destructor did not terminalize cancellation");
    }
    assert!(matches!(outcome?, CoreCallOutcome::Cancelled));
    let ok = instance.get_func(&mut store, "ok").unwrap();
    let sibling = store.start_core_call(ok, vec![])?;
    store.resume_core_call(sibling)?.await?;
    finish_ok(&mut store, sibling)?;
    Ok(())
}

#[cfg(any(feature = "gc-copying", feature = "gc-drc"))]
mod outcome_classification {
    use super::*;

    async fn orphaned_payload(return_error: bool) -> Result<()> {
        let collectors = [
            #[cfg(feature = "gc-copying")]
            Collector::Copying,
            #[cfg(feature = "gc-drc")]
            Collector::DeferredReferenceCounting,
        ];
        for collector in collectors {
            let mut config = Config::new();
            config.collector(collector).wasm_exceptions(true);
            let engine = Engine::new(&config)?;
            let mut store = Store::new(&engine, ());
            let tag = Tag::new(
                &mut store,
                &TagType::new(FuncType::new(&engine, [ValType::EXTERNREF], [])),
            )?;
            let ty = ExnType::from_tag_type(&tag.ty(&store))?;
            let pre = ExnRefPre::new(&mut store, ty);
            let drops = Arc::new(AtomicUsize::new(0));
            let count = drops.clone();
            let mut linker = Linker::new(&engine);
            linker.func_wrap(
                "test",
                "orphan",
                move |mut caller: Caller<'_, ()>| -> Result<()> {
                    let mut scope = RootScope::new(&mut caller);
                    let payload = ExternRef::new(&mut scope, Probe(count.clone()))?;
                    let exn =
                        ExnRef::new(&mut scope, &pre, &tag, &[Val::ExternRef(Some(payload))])?;
                    let marker = scope.as_context_mut().throw::<()>(exn).unwrap_err();
                    assert!(marker.is::<ThrownException>());
                    drop(marker);
                    if return_error {
                        bail!("ordinary host failure")
                    }
                    Ok(())
                },
            )?;
            let module = Module::new(
                &engine,
                r#"(module
              (import "test" "orphan" (func $orphan))
              (func (export "run") call $orphan))"#,
            )?;
            let instance = linker.instantiate_async(&mut store, &module).await?;
            let func = instance.get_func(&mut store, "run").unwrap();
            let id = store.start_core_call(func, vec![])?;
            assert_eq!(
                store.resume_core_call(id)?.await?,
                CoreCallEvent::Finished { call: id }
            );
            assert!(
                store.take_pending_exception().is_none(),
                "terminal call left orphan in pending slot"
            );
            match store.take_core_call_outcome(id)? {
                CoreCallOutcome::Trapped(error) if return_error => {
                    assert!(
                        format!("{error:#}").contains("ordinary host failure"),
                        "lost original host diagnostic: {error:?}"
                    );
                    assert!(!error.is::<ThrownException>());
                }
                CoreCallOutcome::Returned(values) if !return_error => assert!(values.is_empty()),
                _ => panic!("orphaned pending payload overrode the actual call result"),
            }
            store.gc(None)?;
            assert_eq!(
                drops.load(Ordering::SeqCst),
                1,
                "orphaned pending payload root leaked"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn core_continuations_orphan_does_not_override_trap() -> Result<()> {
        orphaned_payload(true).await
    }

    #[tokio::test]
    async fn core_continuations_orphan_does_not_override_return() -> Result<()> {
        orphaned_payload(false).await
    }
}

#[tokio::test]
async fn core_continuations_suspending_host_root() -> Result<()> {
    fn assert_send<T: Send>(_: &T) {}
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    linker.func_new_suspending(
        "test",
        "root",
        FuncType::new(&engine, [ValType::I32], [ValType::I64]),
        |_, args| {
            assert_eq!(args, [CoreValue::I32(42)]);
            Ok(CoreHostStep::Suspend(42))
        },
    )?;
    let func = linker
        .get(&mut store, "test", "root")
        .unwrap()
        .into_func()
        .unwrap();
    let id = store.start_core_call(func, vec![CoreValue::I32(42)])?;
    let future = store.resume_core_call(id)?;
    assert_send(&future);
    let w = wait(future.await?, id);
    store.complete_core_host_wait(w, CoreHostResult::Return(vec![CoreValue::I64(123)]))?;
    assert_eq!(
        store.resume_core_call(id)?.await?,
        CoreCallEvent::Finished { call: id }
    );
    let CoreCallOutcome::Returned(values) = store.take_core_call_outcome(id)? else {
        panic!("direct suspending host root did not return")
    };
    assert_eq!(values, [CoreValue::I64(123)]);
    Ok(())
}

#[cfg(feature = "call-hook")]
#[tokio::test]
async fn core_continuations_teardown_drains_before_user_data() -> Result<()> {
    use std::sync::atomic::AtomicBool;
    const CHILD: &str = "WASMTIME_CORE_TEARDOWN_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args([
                "core_continuations_teardown_drains_before_user_data",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .spawn()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait()? {
                assert!(status.success(), "teardown ordering child failed: {status}");
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                panic!("teardown ordering child exceeded 30 seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    struct Data(Arc<AtomicBool>);
    impl Drop for Data {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let engine = Engine::default();
    for into_data in [false, true] {
        let alive = Arc::new(AtomicBool::new(true));
        let drained = Arc::new(AtomicUsize::new(0));
        let mut store = Store::new(&engine, Data(alive.clone()));
        let mut linker = Linker::new(&engine);
        linker.func_new_suspending("test", "poll", FuncType::new(&engine, [], []), |_, _| {
            Ok(CoreHostStep::Suspend(1))
        })?;
        let module = Module::new(
            &engine,
            r#"(module
            (import "test" "poll" (func $poll)) (func (export "run") call $poll))"#,
        )?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        let func = instance.get_func(&mut store, "run").unwrap();
        for _ in 0..2 {
            let id = store.start_core_call(func, vec![])?;
            wait(store.resume_core_call(id)?.await?, id);
        }
        let live = alive.clone();
        let count = drained.clone();
        store.call_hook(move |store, event| {
            if matches!(event, CallHook::ReturningFromHost) {
                // Check external state BEFORE accessing T on the resumed fiber.
                assert!(
                    live.load(Ordering::SeqCst),
                    "released fiber disposed after user data dropped"
                );
                assert!(store.data().0.load(Ordering::SeqCst));
                count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        });
        if into_data {
            let data = store.into_data();
            assert!(alive.load(Ordering::SeqCst));
            assert_eq!(drained.load(Ordering::SeqCst), 2);
            drop(data);
        } else {
            drop(store);
        }
        assert!(!alive.load(Ordering::SeqCst));
        assert_eq!(drained.load(Ordering::SeqCst), 2);
    }
    Ok(())
}

#[cfg(any(feature = "gc-copying", feature = "gc-drc"))]
mod callback_entry {
    use super::*;

    fn collectors() -> Vec<Collector> {
        vec![
            #[cfg(feature = "gc-copying")]
            Collector::Copying,
            #[cfg(feature = "gc-drc")]
            Collector::DeferredReferenceCounting,
        ]
    }

    // A prior ordinary import's discarded marker cannot authorize the next
    // callback, its side effects, a wait, or selection of a guest EH handler.
    async fn rejects_preexisting(suspend: bool, owned: bool) -> Result<()> {
        for collector in collectors() {
            let mut config = Config::new();
            config.collector(collector).wasm_exceptions(true);
            let engine = Engine::new(&config)?;
            let mut store = Store::new(&engine, ());
            let tag = Tag::new(
                &mut store,
                &TagType::new(FuncType::new(
                    &engine,
                    [ValType::I32, ValType::EXTERNREF],
                    [],
                )),
            )?;
            let ty = ExnType::from_tag_type(&tag.ty(&store))?;
            let pre = ExnRefPre::new(&mut store, ty);
            let drops = Arc::new(AtomicUsize::new(0));
            let count = drops.clone();
            let mut linker = Linker::new(&engine);
            linker.func_wrap(
                "test",
                "orphan",
                move |mut caller: Caller<'_, ()>| -> Result<()> {
                    let mut scope = RootScope::new(&mut caller);
                    let payload = ExternRef::new(&mut scope, Probe(count.clone()))?;
                    let exn = ExnRef::new(
                        &mut scope,
                        &pre,
                        &tag,
                        &[Val::I32(77), Val::ExternRef(Some(payload))],
                    )?;
                    let marker = scope.as_context_mut().throw::<()>(exn).unwrap_err();
                    assert!(marker.is::<ThrownException>());
                    drop(marker);
                    Ok(())
                },
            )?;
            let invoked = Arc::new(AtomicUsize::new(0));
            let calls = invoked.clone();
            linker.func_new_suspending(
                "test",
                "next",
                FuncType::new(&engine, [], []),
                move |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(if suspend {
                        CoreHostStep::Suspend(9)
                    } else {
                        CoreHostStep::Return(vec![])
                    })
                },
            )?;
            let module = Module::new(
                &engine,
                r#"(module
                (import "test" "orphan" (func $orphan))
                (import "test" "next" (func $next))
                (global $handled (export "handled") (mut i32) (i32.const 0))
                (func (export "run")
                    (block $caught
                        (try_table (catch_all $caught) call $orphan call $next return))
                    i32.const 1 global.set $handled)
                (func (export "ok") (result i32) i32.const 123))"#,
            )?;
            let instance = linker.instantiate_async(&mut store, &module).await?;
            let func = instance.get_func(&mut store, "run").unwrap();
            let result = if owned {
                let id = store.start_core_call(func, vec![])?;
                assert_eq!(
                    store.resume_core_call(id)?.await?,
                    CoreCallEvent::Finished { call: id }
                );
                finish_ok(&mut store, id).map(|_| ())
            } else {
                func.typed::<(), ()>(&store)?
                    .call_async(&mut store, ())
                    .await
            };
            assert_eq!(
                invoked.load(Ordering::SeqCst),
                0,
                "entered callback with an earlier import's pending payload"
            );
            assert_eq!(
                instance
                    .get_global(&mut store, "handled")
                    .unwrap()
                    .get(&mut store)
                    .i32(),
                Some(0),
                "selected an EH handler for an unmarked orphan"
            );
            error(result, CoreCallError::PendingException);
            if owned {
                assert!(
                    store.take_pending_exception().is_none(),
                    "owned terminal left the orphan active"
                );
            } else {
                store.gc(None)?;
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    0,
                    "ordinary call consumed the caller's pending payload"
                );
                let mut scope = RootScope::new(&mut store);
                let pending = scope
                    .as_context_mut()
                    .take_pending_exception()
                    .expect("ordinary call lost the pending slot");
                assert!(Tag::eq(&pending.tag(&mut scope)?, &tag, &scope));
                assert_eq!(pending.field(&mut scope, 0)?.i32(), Some(77));
                let field = pending.field(&mut scope, 1)?;
                let payload = field.externref().unwrap().unwrap();
                let probe = payload
                    .data(&scope)?
                    .unwrap()
                    .downcast_ref::<Probe>()
                    .unwrap();
                assert!(Arc::ptr_eq(&probe.0, &drops));
            }
            store.gc(None)?;
            assert_eq!(
                drops.load(Ordering::SeqCst),
                1,
                "rejected entry leaked the orphan payload"
            );
            assert!(store.take_pending_exception().is_none());
            let ok = instance.get_func(&mut store, "ok").unwrap();
            let later = store.start_core_call(ok, vec![])?;
            assert_eq!(
                store.resume_core_call(later)?.await?,
                CoreCallEvent::Finished { call: later }
            );
            assert_eq!(finish_ok(&mut store, later)?, [CoreValue::I32(123)]);
        }
        Ok(())
    }

    #[tokio::test]
    async fn preexisting_return_ordinary() -> Result<()> {
        rejects_preexisting(false, false).await
    }
    #[tokio::test]
    async fn preexisting_suspend_ordinary() -> Result<()> {
        rejects_preexisting(true, false).await
    }
    #[tokio::test]
    async fn preexisting_return_owned() -> Result<()> {
        rejects_preexisting(false, true).await
    }
    #[tokio::test]
    async fn preexisting_suspend_owned() -> Result<()> {
        rejects_preexisting(true, true).await
    }

    // Empty-slot admission permits a payload produced by this callback to
    // propagate for either successful action. Neither action may publish a wait.
    async fn fresh_pending(suspend: bool) -> Result<()> {
        for collector in collectors() {
            let mut config = Config::new();
            config.collector(collector).wasm_exceptions(true);
            let engine = Engine::new(&config)?;
            let mut store = Store::new(&engine, ());
            let tag = Tag::new(
                &mut store,
                &TagType::new(FuncType::new(&engine, [ValType::I32], [])),
            )?;
            let ty = ExnType::from_tag_type(&tag.ty(&store))?;
            let pre = ExnRefPre::new(&mut store, ty);
            let invoked = Arc::new(AtomicUsize::new(0));
            let calls = invoked.clone();
            let mut linker = Linker::new(&engine);
            linker.func_new_suspending(
                "test",
                "fresh",
                FuncType::new(&engine, [], []),
                move |mut caller, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let exn = ExnRef::new(&mut caller, &pre, &tag, &[Val::I32(42)])?;
                    let marker = caller.as_context_mut().throw::<()>(exn).unwrap_err();
                    assert!(marker.is::<ThrownException>());
                    drop(marker);
                    Ok(if suspend {
                        CoreHostStep::Suspend(9)
                    } else {
                        CoreHostStep::Return(vec![])
                    })
                },
            )?;
            let module = Module::new(
                &engine,
                r#"(module
                (import "test" "fresh" (func $fresh)) (func (export "run") call $fresh))"#,
            )?;
            let instance = linker.instantiate_async(&mut store, &module).await?;
            let func = instance.get_func(&mut store, "run").unwrap();
            let id = store.start_core_call(func, vec![])?;
            assert_eq!(
                store.resume_core_call(id)?.await?,
                CoreCallEvent::Finished { call: id }
            );
            assert_eq!(invoked.load(Ordering::SeqCst), 1);
            assert!(store.take_pending_exception().is_none());
            let CoreCallOutcome::Exception { exception, .. } = store.take_core_call_outcome(id)?
            else {
                panic!("fresh callback payload did not propagate as Exception");
            };
            assert!(Tag::eq(&exception.tag(&mut store)?, &tag, &store));
            assert_eq!(exception.field(&mut store, 0)?.i32(), Some(42));
        }
        Ok(())
    }
    #[tokio::test]
    async fn fresh_return_exception() -> Result<()> {
        fresh_pending(false).await
    }
    #[tokio::test]
    async fn fresh_suspend_exception() -> Result<()> {
        fresh_pending(true).await
    }
}
