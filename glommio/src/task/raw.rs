//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
use alloc::alloc::Layout;
use core::{
    future::Future,
    mem::{self, ManuallyDrop},
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};
use std::sync::atomic::{fence, AtomicBool, Ordering};

#[cfg(feature = "debugging")]
use crate::task::debugging::TaskDebugger;
use crate::{
    sys,
    task::{
        header::{AtomicRefCount, Header, RefCount},
        registry::TaskRegistry,
        state::*,
        utils::{abort, abort_on_panic, extend},
        Task,
    },
};

/// The vtable for a task.
pub(crate) struct TaskVTable {
    /// Schedules an existing runnable, consuming its counted reference.
    pub(crate) schedule: unsafe fn(*const ()),

    /// Returns a pointer to the output stored after completion.
    pub(crate) get_output: unsafe fn(*const ()) -> *const (),

    /// Cancels and releases a runnable task.
    pub(crate) drop_task: unsafe fn(*const ()),

    /// Cancels the task through its join handle.
    pub(crate) cancel: unsafe fn(*const ()),

    /// Drops the join handle and any unread output.
    pub(crate) drop_handle: unsafe fn(*const ()),

    /// Destroys owner-thread resources during executor shutdown.
    pub(crate) shutdown: unsafe fn(*const ()),

    /// Runs the task.
    pub(crate) run: unsafe fn(*const ()) -> bool,
}

/// Memory layout of a task.
///
/// This struct contains the following information:
///
/// 1. How to allocate and deallocate the task.
/// 2. How to access the fields inside the task.
#[derive(Clone, Copy)]
pub(crate) struct TaskLayout {
    /// Memory layout of the whole task.
    pub(crate) layout: Layout,

    /// Offset into the task at which the schedule function is stored.
    pub(crate) offset_s: usize,

    /// Offset into the task at which the future is stored.
    pub(crate) offset_f: usize,

    /// Offset into the task at which the output is stored.
    pub(crate) offset_r: usize,
}

/// Raw pointers to the fields inside a task.
pub(crate) struct RawTask<F, R, S> {
    /// The task header.
    pub(crate) header: *const Header,

    /// The schedule function.
    pub(crate) schedule: *const S,

    /// The future.
    pub(crate) future: *mut F,

    /// The output of the future.
    pub(crate) output: *mut R,
}

impl<F, R, S> Copy for RawTask<F, R, S> {}

impl<F, R, S> Clone for RawTask<F, R, S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<F, R, S> RawTask<F, R, S>
where
    F: Future<Output = R>,
    S: Fn(Task),
{
    const RAW_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_waker,
        Self::wake,
        Self::wake_by_ref,
        Self::drop_waker,
    );

    // Cleanup notifications must not be confused with genuine wakes: consuming
    // the sole external waker of a detached task must still poll its future.
    const CLEANUP_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_cleanup_waker,
        Self::wake_cleanup,
        Self::do_cleanup,
        Self::drop_waker,
    );

    /// Allocates a task with the given `future` and `schedule` function.
    ///
    /// The initial runnable, join handle, and executor registry each own one
    /// counted reference.
    /// The registry comes from the owning executor, which need not be running.
    pub(crate) fn allocate(
        future: F,
        schedule: S,
        executor_id: usize,
        registry: &TaskRegistry,
        latency_matters: bool,
    ) -> NonNull<()> {
        // Compute the layout of the task for allocation. Abort if the computation
        // fails.
        let task_layout = abort_on_panic(Self::task_layout);

        unsafe {
            // Allocate enough space for the entire task.
            let raw_task = match NonNull::new(alloc::alloc::alloc(task_layout.layout) as *mut ()) {
                None => abort(),
                Some(p) => p,
            };

            let raw = Self::from_ptr(raw_task.as_ptr());

            // Write the header as the first field of the task.
            (raw.header as *mut Header).write(Header {
                notifier: sys::get_sleep_notifier_for(executor_id).unwrap(),
                state: SCHEDULED | HANDLE,
                latency_matters,
                references: AtomicRefCount::new(3),
                active: AtomicBool::new(true),
                prev_link: core::ptr::null_mut(),
                next: core::ptr::null_mut(),
                scheduling: false,
                awaiter: None,
                vtable: &TaskVTable {
                    schedule: Self::schedule_owned,
                    get_output: Self::get_output,
                    drop_task: Self::drop_task,
                    cancel: Self::cancel,
                    drop_handle: Self::drop_handle,
                    shutdown: Self::shutdown,
                    run: Self::run,
                },
                #[cfg(feature = "debugging")]
                owner_thread: std::thread::current().id(),
                #[cfg(feature = "debugging")]
                debugger_count: TaskDebugger::counter(),
            });

            // Write the schedule function as the third field of the task.
            (raw.schedule as *mut S).write(schedule);

            // Write the future as the fourth field of the task.
            raw.future.write(future);

            registry.insert(raw.header as *mut Header);

            #[cfg(feature = "debugging")]
            TaskDebugger::register(raw_task.as_ptr());

            raw_task
        }
    }

    unsafe fn my_id(&self) -> usize {
        self.notifier().id()
    }

    unsafe fn notifier(&self) -> &sys::SleepNotifier {
        &(*self.header).notifier
    }

    fn thread_id() -> Option<usize> {
        crate::executor::executor_id()
    }

    /// Creates a `RawTask` from a raw task pointer.
    #[inline]
    pub(crate) fn from_ptr(ptr: *const ()) -> Self {
        let task_layout = Self::task_layout();
        let p = ptr as *const u8;

        unsafe {
            Self {
                header: p as *const Header,
                schedule: p.add(task_layout.offset_s) as *const S,
                future: p.add(task_layout.offset_f) as *mut F,
                output: p.add(task_layout.offset_r) as *mut R,
            }
        }
    }

    /// Returns the memory layout for a task.
    #[inline]
    fn task_layout() -> TaskLayout {
        // Compute the layouts for `Header`, `T`, `S`, `F`, and `R`.
        let layout_header = Layout::new::<Header>();
        let layout_s = Layout::new::<S>();
        let layout_f = Layout::new::<F>();
        let layout_r = Layout::new::<R>();

        // Compute the layout for `union { F, R }`.
        let size_union = layout_f.size().max(layout_r.size());
        let align_union = layout_f.align().max(layout_r.align());
        let layout_union = unsafe { Layout::from_size_align_unchecked(size_union, align_union) };

        // Compute the layout for `Header` followed by `T`, then `S`, and finally `union
        // { F, R }`.
        let layout = layout_header;
        let (layout, offset_s) = extend(layout, layout_s);
        let (layout, offset_union) = extend(layout, layout_union);
        let offset_f = offset_union;
        let offset_r = offset_union;

        TaskLayout {
            layout,
            offset_s,
            offset_f,
            offset_r,
        }
    }

    /// Wakes a task. Only the owning executor may access its local state.
    unsafe fn do_wake(ptr: *const ()) {
        let raw = Self::from_ptr(ptr);
        if !(*raw.header).active.load(Ordering::Acquire) {
            return;
        }
        if Self::thread_id() != Some(raw.my_id()) {
            // Closing the queue can synchronously drop the supplied waker.
            let notifier = (*raw.header).notifier.clone();
            notifier.queue_waker(
                Waker::from_raw(Self::clone_waker(ptr)),
                (*raw.header).latency_matters,
            );
            return;
        }

        let header = raw.header as *mut Header;
        let state = (*header).state;
        if state & (COMPLETED | CLOSED | SCHEDULED) == 0 {
            (*header).state |= SCHEDULED;
            if state & RUNNING == 0 {
                Self::schedule(ptr);
            }
        }
    }

    unsafe fn wake(ptr: *const ()) {
        // Preserve ownership even if a synchronous scheduling callback panics.
        let _waker = Waker::from_raw(RawWaker::new(ptr, &Self::RAW_WAKER_VTABLE));
        Self::do_wake(ptr);
    }

    unsafe fn wake_by_ref(ptr: *const ()) {
        Self::do_wake(ptr);
    }

    unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
        Self::increment_references(ptr as *const Header);
        RawWaker::new(ptr, &Self::RAW_WAKER_VTABLE)
    }

    #[inline]
    unsafe fn increment_references(header: *const Header) {
        let refs = (*header).references.fetch_add(1, Ordering::Relaxed);
        if refs <= 0 || refs == RefCount::MAX {
            abort();
        }
    }

    /// Releases a reference after publishing any owner-thread cleanup. The last
    /// release may run anywhere: no future, output, or schedule closure remains.
    #[inline]
    unsafe fn release(ptr: *const ()) {
        Self::release_references(ptr, 1);
    }

    #[inline]
    unsafe fn release_references(ptr: *const (), count: RefCount) {
        let header = ptr as *const Header;
        let refs = (*header).references.fetch_sub(count, Ordering::Release);
        if refs < count {
            abort();
        }
        if refs == count {
            fence(Ordering::Acquire);
            Self::destroy(ptr);
        }
        // A concurrent final release may free the allocation immediately after
        // our decrement. Do not access the header on the nonfinal path.
    }

    /// Keep our reference while arranging owner cleanup of an abandoned future.
    /// A CAS is necessary: concurrent drops must not both miss the transition to
    /// just the registry and the last waker, or touch memory after releasing it.
    /// Foreign cleanup transfers this reference to the notification so the owner
    /// can finish cleanup even before the submitting thread returns.
    unsafe fn drop_waker(ptr: *const ()) {
        let header = ptr as *const Header;
        let mut refs = (*header).references.load(Ordering::Acquire);
        loop {
            if refs <= 0 {
                abort();
            }
            if refs == 1 {
                // There are no weak task references: ownership cannot be
                // recreated without an existing counted reference. This acquire
                // load proves exclusive ownership and observes prior cleanup.
                Self::destroy(ptr);
                return;
            }
            if refs == 2
                && (*header).active.load(Ordering::Acquire)
                && (*header).notifier.accepts_foreign_wakes()
            {
                if Self::thread_id() != Some((*header).notifier.id()) {
                    let notifier = (*header).notifier.clone();
                    notifier.queue_waker(
                        Waker::from_raw(RawWaker::new(ptr, &Self::CLEANUP_WAKER_VTABLE)),
                        (*header).latency_matters,
                    );
                } else {
                    defer!(Self::release(ptr));
                    Self::do_cleanup(ptr);
                }
                return;
            }
            match (*header).references.compare_exchange_weak(
                refs,
                refs - 1,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => refs = current,
            }
        }
    }

    unsafe fn clone_cleanup_waker(ptr: *const ()) -> RawWaker {
        Self::increment_references(ptr as *const Header);
        RawWaker::new(ptr, &Self::CLEANUP_WAKER_VTABLE)
    }

    unsafe fn wake_cleanup(ptr: *const ()) {
        let _waker = Waker::from_raw(RawWaker::new(ptr, &Self::CLEANUP_WAKER_VTABLE));
        Self::do_cleanup(ptr);
    }

    /// A last-drop notification schedules cancellation without polling again.
    /// Destruction runs through the scheduler so it has a task queue context and
    /// does not reenter reactor resources borrowed by the waker's caller.
    unsafe fn do_cleanup(ptr: *const ()) {
        let raw = Self::from_ptr(ptr);
        let header = raw.header as *mut Header;
        if !(*header).active.load(Ordering::Acquire) {
            return;
        }
        if Self::thread_id() != Some(raw.my_id()) {
            let notifier = (*header).notifier.clone();
            notifier.queue_waker(
                Waker::from_raw(Self::clone_cleanup_waker(ptr)),
                (*header).latency_matters,
            );
        } else if (*header).state & (HANDLE | SCHEDULED | RUNNING) == 0
            && (*header).references.load(Ordering::Acquire) == 2
        {
            Self::cancel(ptr);
        }
    }

    /// Creates a new runnable when a wake or cancellation needs to schedule it.
    unsafe fn schedule(ptr: *const ()) {
        Self::increment_references(ptr as *const Header);
        Self::schedule_owned(ptr);
    }

    /// Transfers a counted runnable into the schedule closure. The registry's
    /// reference protects the closure until the guard permits owner cleanup,
    /// including when the callback runs or drops the task synchronously.
    unsafe fn schedule_owned(ptr: *const ()) {
        let raw = Self::from_ptr(ptr);
        let header = raw.header as *mut Header;
        if (*header).state & SCHEDULE_DROPPED != 0 {
            Self::release(ptr);
            return;
        }
        let was_scheduling = mem::replace(&mut (*header).scheduling, true);
        let _guard = ScheduleGuard {
            raw,
            was_scheduling,
        };
        (*raw.schedule)(Task {
            raw_task: NonNull::new_unchecked(ptr as *mut ()),
        });
    }

    /// Marks the union empty before calling user code in the destructor.
    unsafe fn drop_future(ptr: *const ()) {
        let raw = Self::from_ptr(ptr);
        let header = raw.header as *mut Header;
        if (*header).state & FUTURE_DROPPED == 0 {
            (*header).state |= FUTURE_DROPPED;
            abort_on_panic(|| raw.future.drop_in_place());
        }
    }

    /// Moves the output out so its destructor can unwind after task cleanup.
    unsafe fn take_output(ptr: *const ()) -> Option<R> {
        let raw = Self::from_ptr(ptr);
        let header = raw.header as *mut Header;
        if (*header).state & OUTPUT_PRESENT != 0 {
            (*header).state = ((*header).state & !OUTPUT_PRESENT) | CLOSED;
            Some(raw.output.read())
        } else {
            None
        }
    }

    unsafe fn get_output(ptr: *const ()) -> *const () {
        Self::from_ptr(ptr).output as *const ()
    }

    /// Releases owner-only resources and then the registry's reference. The
    /// registry protects all destructor callbacks; its release may free the task.
    unsafe fn finish(ptr: *const ()) {
        if Self::cleanup_owner(ptr) {
            Self::release(ptr);
        }
    }

    /// Cleans and unlinks the task, returning whether the caller must release
    /// the registry reference. Keeping it counted lets run combine both releases.
    unsafe fn cleanup_owner(ptr: *const ()) -> bool {
        let raw = Self::from_ptr(ptr);
        let header = raw.header as *mut Header;
        if (*header).state & SCHEDULE_DROPPED != 0
            || (*header).state & FUTURE_DROPPED == 0
            || (*header).state & RUNNING != 0
            || (*header).scheduling
        {
            return false;
        }
        (*header).active.store(false, Ordering::Release);
        (*header).scheduling = true;
        (*header).state |= SCHEDULE_DROPPED;
        abort_on_panic(|| (raw.schedule as *mut S).drop_in_place());
        Header::notify(header, None);
        (*header).scheduling = false;

        #[cfg(feature = "debugging")]
        TaskDebugger::detach(ptr);
        if !(*header).prev_link.is_null() {
            TaskRegistry::remove(header);
        }
        true
    }

    /// Cancels without polling. RUNNING also protects the future while its
    /// destructor is executing, so reentrant handle/waker operations defer work.
    unsafe fn close(ptr: *const ()) {
        let header = ptr as *mut Header;
        (*header).state |= CLOSED;
        (*header).active.store(false, Ordering::Release);
        if (*header).state & RUNNING == 0 {
            (*header).state = ((*header).state & !SCHEDULED) | RUNNING;
            Self::drop_future(ptr);
            (*header).state &= !RUNNING;
            Header::notify(header, None);
        }
    }

    unsafe fn cancel(ptr: *const ()) {
        let header = ptr as *mut Header;
        let state = (*header).state;
        if state & (COMPLETED | CLOSED) != 0 {
            return;
        }
        (*header).state |= CLOSED;
        (*header).active.store(false, Ordering::Release);
        if state & (SCHEDULED | RUNNING) == 0 {
            (*header).state |= SCHEDULED;
            Self::schedule(ptr);
        }
        Header::notify(header, None);
    }

    /// Drops outputs locally and routes abandoned futures through owner cleanup.
    /// An acquire load of one reference proves exclusive ownership of a cleaned task.
    unsafe fn drop_handle(ptr: *const ()) {
        let header = ptr as *mut Header;
        (*header).state &= !HANDLE;
        let output = Self::take_output(ptr);
        if (*header).state & SCHEDULE_DROPPED != 0
            && (*header).references.load(Ordering::Acquire) == 1
        {
            Self::destroy(ptr);
        } else {
            defer!(Self::drop_waker(ptr));
            Self::finish(ptr);
            if (*header).state & (SCHEDULED | RUNNING | CLOSED | COMPLETED) == 0
                && (*header).references.load(Ordering::Acquire) == 2
            {
                Self::cancel(ptr);
            }
        }
        drop(output);
    }

    unsafe fn drop_task(ptr: *const ()) {
        Self::close(ptr);
        Self::finish(ptr);
        Self::release(ptr);
    }

    /// Unlinks before invoking destructors, which can reenter executor shutdown.
    /// A running poll or schedule callback retains the registry reference until
    /// its guard can finish cleanup, even if the registry itself is destroyed.
    unsafe fn shutdown(ptr: *const ()) {
        // The registry may own the only reference at shutdown entry.
        Self::increment_references(ptr as *const Header);
        TaskRegistry::remove(ptr as *mut Header);
        Self::close(ptr);
        Self::finish(ptr);
        Self::release(ptr);
    }

    /// Only the inert header is left when the count reaches zero. In particular,
    /// the !Send output was taken or destroyed before releasing the handle.
    unsafe fn destroy(ptr: *const ()) {
        let header = ptr as *mut Header;
        // The acquire load or fence in the final release observes owner cleanup.
        debug_assert_eq!(
            (*header).state & (FUTURE_DROPPED | SCHEDULE_DROPPED | OUTPUT_PRESENT | HANDLE),
            FUTURE_DROPPED | SCHEDULE_DROPPED,
        );
        debug_assert!((*header).prev_link.is_null());
        debug_assert!((*header).awaiter.is_none());
        #[cfg(feature = "debugging")]
        (*header).debugger_count.fetch_sub(1, Ordering::Relaxed);
        abort_on_panic(|| header.drop_in_place());
        let layout = Self::task_layout().layout;
        #[cfg(test)]
        crate::task::test_support::deallocate(ptr as *mut u8, layout);
        #[cfg(not(test))]
        alloc::alloc::dealloc(ptr as *mut u8, layout);
    }

    /// Polls once while holding the runnable reference, transferring it when a
    /// wake received during polling schedules the task again.
    unsafe fn run(ptr: *const ()) -> bool {
        let raw = Self::from_ptr(ptr);
        let header = raw.header as *mut Header;
        if (*header).state & (CLOSED | COMPLETED) != 0 {
            Self::drop_task(ptr);
            return false;
        }
        (*header).state = ((*header).state & !SCHEDULED) | RUNNING;
        let waker = ManuallyDrop::new(Waker::from_raw(RawWaker::new(ptr, &Self::RAW_WAKER_VTABLE)));
        let guard = PollGuard(raw);
        let poll = Pin::new_unchecked(&mut *raw.future).poll(&mut Context::from_waker(&waker));

        match poll {
            Poll::Ready(output) => {
                (*header).active.store(false, Ordering::Release);
                Self::drop_future(ptr);
                raw.output.write(output);
                (*header).state = ((*header).state & !SCHEDULED) | COMPLETED | OUTPUT_PRESENT;
                if (*header).state & HANDLE == 0 || (*header).state & CLOSED != 0 {
                    drop(Self::take_output(ptr));
                }
                (*header).state &= !RUNNING;
                Header::notify(header, None);
            }
            Poll::Pending => {
                if (*header).state & CLOSED != 0 {
                    Self::drop_future(ptr);
                    (*header).state &= !(RUNNING | SCHEDULED);
                    Header::notify(header, None);
                } else {
                    (*header).state &= !RUNNING;
                    if (*header).state & SCHEDULED != 0 {
                        // The callback owns the runnable from here, including
                        // if it runs, drops, or panics synchronously.
                        mem::forget(guard);
                        Self::schedule_owned(ptr);
                        return true;
                    } else if (*header).state & HANDLE == 0
                        && (*header).references.load(Ordering::Acquire) == 2
                    {
                        Self::close(ptr);
                    }
                }
            }
        }
        let release_registry = Self::cleanup_owner(ptr);
        mem::forget(guard);
        if release_registry {
            Self::release_references(ptr, 2);
        } else {
            Self::drop_waker(ptr);
        }
        false
    }
}

/// Protects the borrowed scheduling closure until the callback returns.
struct ScheduleGuard<F, R, S>
where
    F: Future<Output = R>,
    S: Fn(Task),
{
    raw: RawTask<F, R, S>,
    // Recursive callbacks restore true until the outermost invocation exits.
    was_scheduling: bool,
}

impl<F, R, S> Drop for ScheduleGuard<F, R, S>
where
    F: Future<Output = R>,
    S: Fn(Task),
{
    fn drop(&mut self) {
        unsafe {
            let header = self.raw.header as *mut Header;
            (*header).scheduling = self.was_scheduling;
            // This may release the last reference. Do not access the task again.
            RawTask::<F, R, S>::finish(header as *const ());
        }
    }
}

/// Cancels a task and releases its runnable reference if polling unwinds.
struct PollGuard<F, R, S>(RawTask<F, R, S>)
where
    F: Future<Output = R>,
    S: Fn(Task);

impl<F, R, S> Drop for PollGuard<F, R, S>
where
    F: Future<Output = R>,
    S: Fn(Task),
{
    fn drop(&mut self) {
        unsafe {
            let header = self.0.header as *mut Header;
            (*header).state |= CLOSED | RUNNING;
            (*header).active.store(false, Ordering::Release);
            RawTask::<F, R, S>::drop_future(header as *const ());
            (*header).state &= !(RUNNING | SCHEDULED);
            Header::notify(header, None);
            RawTask::<F, R, S>::finish(header as *const ());
            RawTask::<F, R, S>::release(header as *const ());
        }
    }
}
