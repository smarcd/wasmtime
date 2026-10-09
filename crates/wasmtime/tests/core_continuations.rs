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
    run_nested(unlocked).await
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
