//! Owned scalar core calls. Only the selected fiber may execute against a Store.
//!
//! Released fibers belong to this arena. An exclusive (`KeepStore`) fiber belongs
//! only to its resume future; forgetting that future leaks the stack and leaves
//! an inert tombstone, never a second path for resuming its borrowed Rust frames.

use super::{StoreId, StoreInner, StoreOpaque};
use crate::fiber::{self, FiberPoll, StoreFiber, StoreFiberYield};
use crate::prelude::*;
use crate::{AsContextMut, Error, Func, FuncType, Store, ValRaw, ValType};
use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};

/// An owned, reference-free core Wasm value. Floating-point values retain bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreValue {
    /// A 32-bit integer.
    I32(i32),
    /// A 64-bit integer.
    I64(i64),
    /// The bits of a 32-bit float.
    F32(u32),
    /// The bits of a 64-bit float.
    F64(u64),
    /// A 128-bit vector.
    V128(u128),
}

impl CoreValue {
    pub(crate) fn lower(self) -> ValRaw {
        match self {
            Self::I32(x) => ValRaw::i32(x),
            Self::I64(x) => ValRaw::i64(x),
            Self::F32(x) => ValRaw::f32(x),
            Self::F64(x) => ValRaw::f64(x),
            Self::V128(x) => ValRaw::v128(x),
        }
    }
    pub(crate) fn lift(raw: ValRaw, ty: &ValType) -> Self {
        match ty {
            ValType::I32 => Self::I32(raw.get_i32()),
            ValType::I64 => Self::I64(raw.get_i64()),
            ValType::F32 => Self::F32(raw.get_f32()),
            ValType::F64 => Self::F64(raw.get_f64()),
            ValType::V128 => Self::V128(raw.get_v128()),
            ValType::Ref(_) => unreachable!(),
        }
    }
    fn matches(self, ty: &ValType) -> bool {
        matches!(
            (self, ty),
            (Self::I32(_), ValType::I32)
                | (Self::I64(_), ValType::I64)
                | (Self::F32(_), ValType::F32)
                | (Self::F64(_), ValType::F64)
                | (Self::V128(_), ValType::V128)
        )
    }
}

pub(crate) fn scalar_type(ty: &FuncType) -> Result<()> {
    if ty
        .params()
        .chain(ty.results())
        .any(|t| matches!(t, ValType::Ref(_)))
    {
        return Err(CoreCallError::NonScalarType.into());
    }
    Ok(())
}

pub(crate) fn check_values(
    values: &[CoreValue],
    types: impl ExactSizeIterator<Item = ValType>,
) -> Result<()> {
    if values.len() != types.len() || !values.iter().zip(types).all(|(v, t)| v.matches(&t)) {
        return Err(CoreCallError::TypeMismatch.into());
    }
    Ok(())
}

/// An opaque call identity, valid only in its originating Store and generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CoreCallId {
    store: StoreId,
    slot: u32,
    generation: u64,
}
/// An opaque, single-use reply address for one suspension of one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CoreHostWaitId {
    call: CoreCallId,
    suspension: u64,
}
/// The owned action returned by a suspending host callback.
pub enum CoreHostStep {
    /// Return these scalar results to the guest immediately.
    Return(Vec<CoreValue>),
    /// Release the Store and publish an embedder-defined token.
    Suspend(u64),
}
/// A reply to a suspended host call. Completing a wait does not execute Wasm.
pub enum CoreHostResult {
    /// Return these scalar results to the suspended import.
    Return(Vec<CoreValue>),
    /// Trap the call. A bare `ThrownException` is rejected.
    Trap(Error),
}
/// A driver event; both variants return the exclusive Store borrow.
#[derive(Debug, PartialEq, Eq)]
pub enum CoreCallEvent {
    /// A host callback has returned and the runtime has safely parked its stack.
    Suspended {
        /// The suspended call.
        call: CoreCallId,
        /// The single-use reply address.
        wait: CoreHostWaitId,
        /// The callback's owned token.
        token: u64,
    },
    /// An outcome is available through [`Store::take_core_call_outcome`].
    Finished {
        /// The completed call.
        call: CoreCallId,
    },
}
/// A completed call's owned outcome.
pub enum CoreCallOutcome {
    /// Scalar results.
    Returned(Vec<CoreValue>),
    /// A runtime or host trap.
    Trapped(Error),
    /// A Store-bound exception and diagnostic error without pending-slot authority.
    #[cfg(feature = "gc")]
    Exception {
        /// Independently rooted exception payload, bound to the original Store.
        exception: crate::OwnedRooted<crate::ExnRef>,
        /// Original diagnostic text and Wasm backtrace, excluding `ThrownException`.
        error: Error,
    },
    /// The call was cancelled without executing further guest instructions.
    Cancelled,
}
/// A rejected driver operation. Errors never consume a wait or another call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreCallError {
    /// Identity belongs to a different Store.
    WrongStore,
    /// Call generation no longer exists.
    StaleCall,
    /// Wait was already completed or belongs to an earlier suspension.
    StaleWait,
    /// Complete the outstanding host wait before resuming.
    WaitingForHost,
    /// An exclusive resume future owns a fiber (possibly intentionally forgotten).
    CallRunning,
    /// The call already has an outcome.
    AlreadyFinished,
    /// The call has not produced an outcome.
    NotFinished,
    /// Scalar arity or type does not match the Wasm signature.
    TypeMismatch,
    /// References are not supported at this API's boundary.
    NonScalarType,
    /// Suspension was requested outside an owned core root.
    NoActiveCoreCall,
    /// Suspension would cross a live ordinary host frame or another fiber.
    ExclusiveHostFrame,
    /// Cancellation is in progress.
    Cancelling,
    /// An unrelated exception is pending in the Store.
    PendingException,
    /// A bare exception marker has no owned payload.
    UnrootedException,
    /// A call or suspension identity cannot advance without wrapping.
    IdExhausted,
}
impl fmt::Display for CoreCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "core call: {self:?}")
    }
}
impl core::error::Error for CoreCallError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Created,
    Runnable,
    Running,
    Waiting,
    Cancelling,
    Finished,
}
struct CallData {
    func: Func,
    args: Vec<CoreValue>,
    results: Vec<ValType>,
}
struct Wait {
    id: CoreHostWaitId,
    token: u64,
    results: Vec<ValType>,
}
enum Outcome {
    Public(CoreCallOutcome),
    #[cfg(feature = "gc")]
    Exception {
        exception: crate::vm::VMGcRef,
        error: Error,
    },
}
struct Entry {
    phase: Phase,
    data: Option<CallData>,
    fiber: Option<StoreFiber<'static>>,
    sequence: u64,
    wait: Option<Wait>,
    reply: Option<CoreHostResult>,
    outcome: Option<Outcome>,
    depth: usize,
    exclusive: usize,
    suspend: Option<usize>,
}
struct Slot {
    generation: u64,
    entry: Option<Entry>,
    retired: bool,
}
#[derive(Default)]
pub(crate) struct CoreCalls {
    slots: Vec<Slot>,
    selected: Option<CoreCallId>,
    #[cfg(all(test, feature = "gc"))]
    fail_outcome_root: bool,
}
impl CoreCalls {
    fn entry(&self, id: CoreCallId) -> Result<&Entry> {
        let slot = self
            .slots
            .get(id.slot as usize)
            .ok_or(CoreCallError::StaleCall)?;
        if slot.generation != id.generation {
            return Err(CoreCallError::StaleCall.into());
        }
        slot.entry
            .as_ref()
            .ok_or_else(|| CoreCallError::StaleCall.into())
    }
    fn entry_mut(&mut self, id: CoreCallId) -> Result<&mut Entry> {
        self.entry(id)?;
        Ok(self.slots[id.slot as usize].entry.as_mut().unwrap())
    }
    fn available(&self) -> Result<()> {
        if self
            .slots
            .iter()
            .filter_map(|s| s.entry.as_ref())
            .any(|e| matches!(e.phase, Phase::Running | Phase::Cancelling))
        {
            return Err(CoreCallError::CallRunning.into());
        }
        Ok(())
    }
    fn remove(&mut self, id: CoreCallId) -> Entry {
        let slot = &mut self.slots[id.slot as usize];
        assert_eq!(slot.generation, id.generation);
        let entry = slot.entry.take().unwrap();
        if let Some(next) = slot.generation.checked_add(1) {
            slot.generation = next;
        } else {
            slot.retired = true;
        }
        entry
    }
    #[cfg(feature = "gc")]
    pub(super) fn trace_roots(
        &mut self,
        modules: &crate::module::ModuleRegistry,
        unwind: &dyn wasmtime_unwinder::Unwind,
        roots: &mut crate::vm::GcRootsList,
    ) {
        for entry in self.slots.iter_mut().filter_map(|s| s.entry.as_mut()) {
            if let Some(fiber) = &mut entry.fiber {
                fiber.trace_gc_roots(modules, unwind, roots);
            }
            if let Some(Outcome::Exception { exception, .. }) = &mut entry.outcome {
                // SAFETY: entries and fibers cannot move while collection visits
                // these pointers; all mutations require exclusive Store access.
                unsafe {
                    roots.add_vmgcref_root(exception.into(), "Core call outcome");
                }
            }
        }
    }
    pub(super) fn assert_drained(&self) {
        assert!(self.slots.iter().all(|s| s.entry.is_none()));
        assert!(self.selected.is_none());
    }
}

/// A selected call's exclusive Store loan. `Pending` retains the Store; it is
/// **not** a scheduling event. Only `Ready(Suspended)` permits sibling driving.
/// Dropping a polled future cancels its call; dropping an unpolled one does nothing.
///
/// Forgetting a polled pending future intentionally leaks its inaccessible stack,
/// Engine reference and any pooling stack slot. Its inert arena tombstone blocks
/// further core driving. Ordinary Store APIs and Store destruction remain safe.
/// No waker or call id can resume the leaked fiber.
///
/// ```compile_fail
/// # use wasmtime::*;
/// # fn example(store: &mut Store<()>, id: CoreCallId) -> Result<()> {
/// let future = store.resume_core_call(id)?;
/// store.data_mut();
/// drop(future);
/// # Ok(()) }
/// ```
pub struct CoreCallFuture<'a, T: 'static> {
    store: &'a mut Store<T>,
    id: CoreCallId,
    fiber: Option<StoreFiber<'static>>,
    done: bool,
}

impl<T: Send + 'static> Store<T> {
    /// Create a scalar root without executing guest code or allocating a stack.
    /// Driving is restricted to `Store`, so no public LIFO root scope can overlap
    /// released calls. Reference values inside Wasm remain fully GC traced.
    ///
    /// ```compile_fail
    /// # use wasmtime::*;
    /// # fn example(store: &mut Store<()>, func: Func) -> Result<()> {
    /// let mut scope = RootScope::new(&mut *store);
    /// store.start_core_call(func, vec![])?;
    /// scope.as_context_mut();
    /// # Ok(()) }
    /// ```
    /// ```compile_fail
    /// # use wasmtime::*;
    /// # fn example(engine: &Engine, func: Func) -> Result<()> {
    /// let mut store = Store::new(engine, std::rc::Rc::new(()));
    /// store.start_core_call(func, vec![])?;
    /// # Ok(()) }
    /// ```
    pub fn start_core_call(&mut self, func: Func, args: Vec<CoreValue>) -> Result<CoreCallId> {
        self.inner.core_calls.available()?;
        if !func.comes_from_same_store(&self.inner) {
            return Err(CoreCallError::WrongStore.into());
        }
        if self.inner.has_pending_exception() {
            return Err(CoreCallError::PendingException.into());
        }
        let ty = func.ty(&*self);
        scalar_type(&ty)?;
        check_values(&args, ty.params())?;
        let entry = Entry {
            phase: Phase::Created,
            data: Some(CallData {
                func,
                args,
                results: ty.results().try_collect::<TryVec<_>, _>()?.into(),
            }),
            fiber: None,
            sequence: 0,
            wait: None,
            reply: None,
            outcome: None,
            depth: 0,
            exclusive: 0,
            suspend: None,
        };
        let store = self.inner.id();
        let arena = &mut self.inner.core_calls;
        let slot = match arena
            .slots
            .iter()
            .position(|s| s.entry.is_none() && !s.retired)
        {
            Some(i) => i,
            None => {
                if arena.slots.len() > u32::MAX as usize {
                    return Err(CoreCallError::IdExhausted.into());
                }
                arena.slots.try_reserve(1)?;
                arena.slots.push(Slot {
                    generation: 0,
                    entry: None,
                    retired: false,
                });
                arena.slots.len() - 1
            }
        };
        let id = CoreCallId {
            store,
            slot: u32::try_from(slot).expect("core slot limit checked before insertion"),
            generation: arena.slots[slot].generation,
        };
        arena.slots[slot].entry = Some(entry);
        Ok(id)
    }
    /// Resume a Created or Runnable call. No ownership changes until first poll.
    pub fn resume_core_call(&mut self, call: CoreCallId) -> Result<CoreCallFuture<'_, T>> {
        self.inner.check_core_id(call)?;
        self.inner.core_calls.available()?;
        if self.inner.has_pending_exception() {
            return Err(CoreCallError::PendingException.into());
        }
        match self.inner.core_calls.entry(call)?.phase {
            Phase::Created | Phase::Runnable => {}
            Phase::Waiting => return Err(CoreCallError::WaitingForHost.into()),
            Phase::Finished => return Err(CoreCallError::AlreadyFinished.into()),
            Phase::Running => return Err(CoreCallError::CallRunning.into()),
            Phase::Cancelling => return Err(CoreCallError::Cancelling.into()),
        }
        Ok(CoreCallFuture {
            store: self,
            id: call,
            fiber: None,
            done: false,
        })
    }
    /// Validate and supply a single-use host reply, without executing guest code.
    pub fn complete_core_host_wait(
        &mut self,
        wait: CoreHostWaitId,
        result: CoreHostResult,
    ) -> Result<()> {
        self.inner.check_core_id(wait.call)?;
        self.inner.core_calls.available()?;
        let entry = self.inner.core_calls.entry_mut(wait.call)?;
        let current = entry.wait.as_ref().ok_or(CoreCallError::StaleWait)?;
        if entry.phase != Phase::Waiting || current.id != wait {
            return Err(CoreCallError::StaleWait.into());
        }
        match &result {
            CoreHostResult::Return(values) => {
                check_values(values, current.results.iter().cloned())?
            }
            CoreHostResult::Trap(error) if error.is::<crate::ThrownException>() => {
                return Err(CoreCallError::UnrootedException.into());
            }
            CoreHostResult::Trap(_) => {}
        }
        entry.wait = None;
        entry.reply = Some(result);
        entry.phase = Phase::Runnable;
        Ok(())
    }
    /// Take a finished outcome once, invalidating its call and wait identities.
    /// Exception extraction preserves the arena root if root allocation fails.
    pub fn take_core_call_outcome(&mut self, call: CoreCallId) -> Result<CoreCallOutcome> {
        self.inner.check_core_id(call)?;
        self.inner.core_calls.available()?;
        if self.inner.core_calls.entry(call)?.phase != Phase::Finished {
            return Err(CoreCallError::NotFinished.into());
        }
        #[cfg(feature = "gc")]
        let rooted = self.inner.root_core_exception(call)?;
        let outcome = self.inner.core_calls.remove(call).outcome.unwrap();
        match outcome {
            Outcome::Public(outcome) => Ok(outcome),
            #[cfg(feature = "gc")]
            Outcome::Exception { exception, error } => {
                self.inner.unwrap_gc_store_mut().drop_gc_ref(exception);
                // The marker is runtime authority over the current pending slot,
                // so it must not escape with this independently owned exception.
                let text = format!("{error:#}");
                let error = match error.downcast::<crate::WasmBacktrace>() {
                    Ok(trace) => Error::msg(text).context(trace),
                    Err(_) => Error::msg(text),
                };
                Ok(CoreCallOutcome::Exception {
                    exception: rooted.unwrap(),
                    error,
                })
            }
        }
    }
    /// Cancel an unstarted or released call. Its Cancelled outcome can be taken once.
    pub fn cancel_core_call(&mut self, call: CoreCallId) -> Result<()> {
        self.inner.check_core_id(call)?;
        self.inner.core_calls.available()?;
        if self.inner.has_pending_exception() {
            return Err(CoreCallError::PendingException.into());
        }
        if self.inner.core_calls.entry(call)?.phase == Phase::Finished {
            return Err(CoreCallError::AlreadyFinished.into());
        }
        self.inner.cancel_released_core_call(call);
        Ok(())
    }
}

impl<T: Send + 'static> Future for CoreCallFuture<'_, T> {
    type Output = Result<CoreCallEvent>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        assert!(!this.done, "polled completed core future");
        if this.fiber.is_none() {
            let entry = this.store.inner.core_calls.entry(this.id).unwrap();
            let fiber = if entry.phase == Phase::Created {
                let data = match entry.data.as_ref().unwrap().try_clone_owned() {
                    Ok(data) => data,
                    Err(error) => {
                        this.done = true;
                        return Poll::Ready(Err(error));
                    }
                };
                let id = this.id;
                // Capture the stable allocation, not the movable Store wrapper.
                match fiber::make_fiber(
                    &mut **this.store.inner,
                    move |store: &mut StoreInner<T>| {
                        let suspend = store.fiber_async_state_mut().suspend_identity();
                        store.core_calls.entry_mut(id).unwrap().suspend = suspend;
                        let result = data.run(store);
                        store.finish_core_execution(id, result);
                        Ok(())
                    },
                ) {
                    Ok(fiber) => fiber,
                    Err(error) => {
                        this.done = true;
                        return Poll::Ready(Err(error));
                    }
                }
            } else {
                assert_eq!(entry.phase, Phase::Runnable);
                this.store
                    .inner
                    .core_calls
                    .entry_mut(this.id)
                    .unwrap()
                    .fiber
                    .take()
                    .unwrap()
            };
            let entry = this.store.inner.core_calls.entry_mut(this.id).unwrap();
            entry.data = None;
            entry.phase = Phase::Running;
            this.fiber = Some(fiber);
        }
        let _selection = CoreGuard::select(&mut this.store.inner, this.id);
        match fiber::poll_store_fiber(&mut this.store.inner, this.fiber.as_mut().unwrap(), cx) {
            FiberPoll::KeepStore => Poll::Pending,
            FiberPoll::Released => {
                let entry = this.store.inner.core_calls.entry_mut(this.id).unwrap();
                assert_eq!(entry.phase, Phase::Running);
                let wait = entry.wait.as_ref().unwrap();
                let event = CoreCallEvent::Suspended {
                    call: this.id,
                    wait: wait.id,
                    token: wait.token,
                };
                entry.fiber = this.fiber.take();
                entry.phase = Phase::Waiting;
                this.done = true;
                Poll::Ready(Ok(event))
            }
            FiberPoll::Finished(result) => {
                let entry = this.store.inner.core_calls.entry_mut(this.id).unwrap();
                if let Err(error) = result {
                    entry.outcome = Some(Outcome::Public(CoreCallOutcome::Trapped(error)));
                }
                assert!(entry.outcome.is_some());
                entry.phase = Phase::Finished;
                this.fiber.take();
                this.done = true;
                Poll::Ready(Ok(CoreCallEvent::Finished { call: this.id }))
            }
        }
    }
}
impl<T> Drop for CoreCallFuture<'_, T> {
    fn drop(&mut self) {
        if let Some(mut fiber) = self.fiber.take() {
            let _selection = CoreGuard::select(&mut self.store.inner, self.id);
            self.store
                .inner
                .core_calls
                .entry_mut(self.id)
                .unwrap()
                .phase = Phase::Cancelling;
            self.store.inner.dispose_core_fiber(self.id, &mut fiber);
        }
    }
}

impl CallData {
    fn try_clone_owned(&self) -> Result<Self> {
        Ok(Self {
            func: self.func,
            args: self
                .args
                .iter()
                .copied()
                .try_collect::<TryVec<_>, _>()?
                .into(),
            results: self
                .results
                .iter()
                .cloned()
                .try_collect::<TryVec<_>, _>()?
                .into(),
        })
    }
    fn run<T>(&self, mut store: &mut StoreInner<T>) -> Result<Vec<CoreValue>> {
        let mut raw = TryVec::new();
        raw.resize_with(self.args.len().max(self.results.len()), || ValRaw::v128(0))?;
        for (slot, value) in raw.iter_mut().zip(&self.args) {
            *slot = value.lower();
        }
        let mut values = TryVec::new();
        values.reserve(self.results.len())?;
        // SAFETY: start_core_call checked Store identity and all scalar types.
        // The buffer is owned by this fiber, sized for args/results, and holds
        // no unrooted references or Store-wide scratch across a release.
        //
        // As in component/concurrent.rs::make_call, dormant runtime Store
        // contexts span the VM call. They point into heap-stable StoreInner /
        // PhantomPinned StoreOpaque and are never used by inactive fibers.
        // resume_fiber restores activation TLS and grants the sole running
        // fiber access before those contexts are used again. No reference to T,
        // mutable Store subfield, arena entry, or public RootScope spans release.
        unsafe {
            self.func
                .call_unchecked(store.as_context_mut(), &mut *raw as *mut [ValRaw])?;
        }
        for (raw, ty) in raw.iter().zip(&self.results) {
            values.push(CoreValue::lift(*raw, ty))?;
        }
        Ok(values.into())
    }
}

/// Runtime-only restoration guard. The pointer names the stable StoreOpaque
/// allocation, never an arena element; checked ids are reacquired after reentry.
pub(crate) struct CoreGuard {
    store: *mut StoreOpaque,
    call: Option<CoreCallId>,
    kind: GuardKind,
}
enum GuardKind {
    Selection,
    Entry,
    Exclusive,
}
impl CoreGuard {
    fn select(store: &mut StoreOpaque, call: CoreCallId) -> Self {
        assert!(store.core_calls.selected.replace(call).is_none());
        Self {
            store,
            call: Some(call),
            kind: GuardKind::Selection,
        }
    }
    pub(crate) fn entry(store: &mut StoreOpaque) -> Self {
        Self::count(store, GuardKind::Entry)
    }
    pub(crate) fn exclusive(store: &mut StoreOpaque) -> Self {
        Self::count(store, GuardKind::Exclusive)
    }
    fn count(store: &mut StoreOpaque, kind: GuardKind) -> Self {
        let call = store.core_calls.selected;
        if let Some(id) = call {
            let entry = store.core_calls.entry_mut(id).unwrap();
            match kind {
                GuardKind::Entry => entry.depth += 1,
                GuardKind::Exclusive => entry.exclusive += 1,
                GuardKind::Selection => unreachable!(),
            }
        }
        Self { store, call, kind }
    }
}
impl Drop for CoreGuard {
    fn drop(&mut self) {
        let Some(id) = self.call else {
            return;
        };
        // SAFETY: guards exist only on active runtime frames. StoreInner is
        // heap-stable and every resumption/disposal owns exclusive access. A
        // forgotten fiber never runs this destructor after Store destruction.
        let store = unsafe { &mut *self.store };
        assert_eq!(store.core_calls.selected, Some(id));
        match self.kind {
            GuardKind::Selection => store.core_calls.selected = None,
            GuardKind::Entry => store.core_calls.entry_mut(id).unwrap().depth -= 1,
            GuardKind::Exclusive => store.core_calls.entry_mut(id).unwrap().exclusive -= 1,
        }
    }
}

impl StoreOpaque {
    fn check_core_id(&self, call: CoreCallId) -> Result<()> {
        if call.store != self.id() {
            return Err(CoreCallError::WrongStore.into());
        }
        self.core_calls.entry(call)?;
        Ok(())
    }
    /// The only operation that may publish a releasable core host wait.
    pub(crate) fn suspend_core_call(
        &mut self,
        token: u64,
        results: Vec<ValType>,
    ) -> Result<CoreHostResult> {
        let id = self
            .core_calls
            .selected
            .ok_or(CoreCallError::NoActiveCoreCall)?;
        let suspend = self.fiber_async_state_mut().suspend_identity();
        let entry = self.core_calls.entry_mut(id).unwrap();
        if entry.phase == Phase::Cancelling {
            return Err(CoreCallError::Cancelling.into());
        }
        if entry.depth != 1
            || entry.exclusive != 0
            || entry.suspend.is_none()
            || entry.suspend != suspend
        {
            return Err(CoreCallError::ExclusiveHostFrame.into());
        }
        assert_eq!(entry.phase, Phase::Running);
        assert!(entry.wait.is_none() && entry.reply.is_none());
        entry.sequence = entry
            .sequence
            .checked_add(1)
            .ok_or(CoreCallError::IdExhausted)?;
        entry.wait = Some(Wait {
            id: CoreHostWaitId {
                call: id,
                suspension: entry.sequence,
            },
            token,
            results,
        });
        // Callback, Caller, and its LIFO root scope have already returned. Only
        // runtime Send-compatible frames and guest frames span this suspension.
        self.with_blocking(|_, cx| cx.suspend(StoreFiberYield::ReleaseStore))?;
        let entry = self.core_calls.entry_mut(id).unwrap();
        assert_eq!(entry.phase, Phase::Running);
        Ok(entry.reply.take().unwrap())
    }
    fn finish_core_execution(&mut self, id: CoreCallId, result: Result<Vec<CoreValue>>) {
        #[cfg(feature = "gc")]
        let exception = self.take_core_pending_exception();
        let outcome = match result {
            Ok(values) => Outcome::Public(CoreCallOutcome::Returned(values)),
            Err(error) => Outcome::Public(CoreCallOutcome::Trapped(error)),
        };
        #[cfg(feature = "gc")]
        let outcome = if let Some(exception) = exception {
            let error = match outcome {
                Outcome::Public(CoreCallOutcome::Trapped(error)) => error,
                _ => crate::ThrownException.into(),
            };
            Outcome::Exception { exception, error }
        } else {
            outcome
        };
        let outcome = match outcome {
            Outcome::Public(CoreCallOutcome::Trapped(error))
                if error.is::<crate::ThrownException>() =>
            {
                Outcome::Public(CoreCallOutcome::Trapped(
                    CoreCallError::UnrootedException.into(),
                ))
            }
            outcome => outcome,
        };
        self.core_calls.entry_mut(id).unwrap().outcome = Some(outcome);
    }
    fn finish_core_cancellation(&mut self, id: CoreCallId) {
        let entry = self.core_calls.entry_mut(id).unwrap();
        let outcome = entry.outcome.take();
        entry.wait = None;
        entry.reply = None;
        entry.data = None;
        entry.outcome = Some(Outcome::Public(CoreCallOutcome::Cancelled));
        entry.phase = Phase::Finished;
        self.drop_core_outcome(outcome);
        #[cfg(feature = "gc")]
        if let Some(exception) = self.take_core_pending_exception() {
            self.unwrap_gc_store_mut().drop_gc_ref(exception);
        }
    }
    fn cancel_released_core_call(&mut self, id: CoreCallId) {
        let entry = self.core_calls.entry_mut(id).unwrap();
        assert!(matches!(
            entry.phase,
            Phase::Created | Phase::Waiting | Phase::Runnable
        ));
        entry.phase = Phase::Cancelling;
        let fiber = entry.fiber.take();
        let _selection = CoreGuard::select(self, id);
        if let Some(mut fiber) = fiber {
            self.dispose_core_fiber(id, &mut fiber);
        } else {
            self.finish_core_cancellation(id);
        }
    }
    fn dispose_core_fiber(&mut self, id: CoreCallId, fiber: &mut StoreFiber<'_>) {
        struct Finish<'a, 'b> {
            store: &'a mut StoreOpaque,
            fiber: &'a mut StoreFiber<'b>,
            id: CoreCallId,
        }
        impl Drop for Finish<'_, '_> {
            fn drop(&mut self) {
                // A user future destructor may panic during cancellation. The
                // fiber crate finishes that stack before rethrowing; terminalize
                // on this unwind as well, never leave a Cancelling tombstone.
                self.fiber.assert_done();
                self.store.finish_core_cancellation(self.id);
            }
        }
        let finish = Finish {
            store: self,
            fiber,
            id,
        };
        finish.fiber.dispose(finish.store);
    }
    fn drop_core_outcome(&mut self, outcome: Option<Outcome>) {
        #[cfg(feature = "gc")]
        if let Some(Outcome::Exception { exception, .. }) = outcome {
            self.unwrap_gc_store_mut().drop_gc_ref(exception);
        }
        #[cfg(not(feature = "gc"))]
        drop(outcome);
    }
    /// No bound on T: Store<T>::Drop and into_data must drain before T dies.
    pub(super) fn drain_core_calls(&mut self) {
        for index in 0..self.core_calls.slots.len() {
            let slot = &self.core_calls.slots[index];
            let Some(entry) = &slot.entry else {
                continue;
            };
            let id = CoreCallId {
                store: self.id(),
                slot: u32::try_from(index).expect("core slot limit checked before insertion"),
                generation: slot.generation,
            };
            match entry.phase {
                Phase::Running => {
                    assert!(entry.fiber.is_none());
                } // Intentionally forgotten exclusive future.
                Phase::Finished => {}
                Phase::Cancelling => panic!("core cancellation reentered Store destruction"),
                _ => self.cancel_released_core_call(id),
            }
            // No entry borrow or iterator spans disposal: it may call user code
            // and collect roots in all the still-registered sibling fibers.
            let entry = self.core_calls.remove(id);
            assert!(entry.fiber.is_none());
            self.drop_core_outcome(entry.outcome);
        }
    }
    #[cfg(feature = "gc")]
    fn root_core_exception(
        &mut self,
        id: CoreCallId,
    ) -> Result<Option<crate::OwnedRooted<crate::ExnRef>>> {
        let Some(Outcome::Exception { exception, .. }) = &self.core_calls.entry(id)?.outcome else {
            return Ok(None);
        };
        let exception = exception.unchecked_copy();
        #[cfg(test)]
        if core::mem::take(&mut self.core_calls.fail_outcome_root) {
            return Err(crate::OutOfMemory::new(1).into());
        }
        let owned = self.unwrap_gc_store_mut().clone_gc_ref(&exception);
        let backup = owned.unchecked_copy();
        let mut nogc = super::AutoAssertNoGc::new(self);
        match crate::OwnedRooted::new(&mut nogc, owned) {
            Ok(root) => Ok(Some(root)),
            Err(error) => {
                nogc.unwrap_gc_store_mut().drop_gc_ref(backup);
                Err(error.into())
            }
        }
    }
}

#[cfg(all(test, feature = "gc-copying", feature = "cranelift", feature = "wat"))]
mod tests {
    use super::*;
    use crate::{Collector, Config, Engine, Instance, Module};
    use core::task::Waker;

    #[test]
    fn core_calls_outcome_root_failure_preserves_generation_and_payload() -> Result<()> {
        let mut config = Config::new();
        config.collector(Collector::Copying).wasm_exceptions(true);
        let engine = Engine::new(&config)?;
        let mut store = Store::new(&engine, ());
        let module = Module::new(
            &engine,
            r#"(module (tag $tag (export "tag") (param i32))
            (func (export "run") i32.const 42 throw $tag))"#,
        )?;
        let instance = Instance::new(&mut store, &module, &[])?;
        let func = instance.get_func(&mut store, "run").unwrap();
        let id = store.start_core_call(func, vec![])?;
        let mut future = Box::pin(store.resume_core_call(id)?);
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(CoreCallEvent::Finished { .. }))
        ));
        drop(future);
        store.inner.core_calls.fail_outcome_root = true;
        let Err(error) = store.take_core_call_outcome(id) else {
            panic!("failure hook did not fire")
        };
        assert!(error.is::<crate::OutOfMemory>());
        assert_eq!(store.inner.core_calls.entry(id)?.phase, Phase::Finished);
        assert!(store.take_pending_exception().is_none());
        store.gc(None)?;
        let CoreCallOutcome::Exception { exception, error } = store.take_core_call_outcome(id)?
        else {
            panic!("outcome lost on failed extraction")
        };
        assert!(!error.is::<crate::ThrownException>());
        assert_eq!(exception.field(&mut store, 0)?.i32(), Some(42));
        let tag = instance.get_tag(&mut store, "tag").unwrap();
        assert!(crate::Tag::eq(&exception.tag(&mut store)?, &tag, &store));
        assert!(store.inner.core_calls.entry(id).is_err());
        Ok(())
    }
}
