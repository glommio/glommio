// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//
//! Task lifecycle tests, which run under the ordinary test runner and also
//! under Miri.
//!
//! Every test in `tests.rs` builds a `LocalExecutor`, which means io_uring,
//! which Miri cannot execute, so the most `unsafe`-dense code in the crate,
//! `task::raw`, had no undefined-behaviour coverage at all. These need no
//! reactor, which is what lets Miri run them; it is also why they cost nothing
//! to run normally, and they do, on every `cargo test`.
//!
//! These drive the task machinery directly instead: allocate, run, schedule,
//! cancel and drop, with a schedule function that just collects runnables.
//! No reactor, no rings, no syscalls. That is enough to exercise the reference
//! counting, the state transitions, and the allocation and teardown paths,
//! which is where a use-after-free would live.
//!
//! Run under Miri with
//! `cargo +nightly miri test -p glommio --lib task::`, and under the normal
//! test runner alongside everything else.

/// Test-only override for the owning-executor identity, read by
/// `raw::RawTask::thread_id` and set by nothing but the tests below.
///
/// `do_wake`, `drop_waker` and `run` all branch on whether the current thread
/// owns the task, and with no executor installed that check always says "not
/// mine" and sends everything down the foreign path. That makes the local
/// teardown path, the reference counting and deallocation and the most
/// `unsafe`-dense code in the crate, untestable without a reactor.
#[cfg(test)]
pub(crate) mod test_executor_id {
    use std::cell::Cell;

    thread_local! {
        static ID: Cell<Option<usize>> = const { Cell::new(None) };
    }

    pub(crate) fn get() -> Option<usize> {
        ID.with(Cell::get)
    }

    /// Sets the override for the current thread, restoring it on drop.
    pub(crate) fn scoped(id: usize) -> Guard {
        let previous = ID.with(|c| c.replace(Some(id)));
        Guard(previous)
    }

    pub(crate) struct Guard(Option<usize>);

    impl Drop for Guard {
        fn drop(&mut self) {
            ID.with(|c| c.set(self.0));
        }
    }
}

#[cfg(test)]
mod test {
    use super::test_executor_id;
    use crate::task::{registry::TaskRegistry, task_impl, task_impl::Task, JoinHandle};
    use std::{
        cell::RefCell,
        future::Future,
        pin::Pin,
        rc::Rc,
        task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    };

    const EXECUTOR_ID: usize = 0;
    const QUEUE_INDEX: usize = 0;

    /// A waker that does nothing, for polling a `JoinHandle` whose task has
    /// already been run to completion.
    fn noop_waker() -> Waker {
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        // SAFETY: every function in the vtable is a no-op and ignores its data
        // pointer, so a null pointer is never dereferenced.
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    fn poll_once<R>(handle: &mut JoinHandle<R>) -> Poll<Option<R>> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        Pin::new(handle).poll(&mut cx)
    }

    /// Collects rescheduled runnables so a test can run them by hand.
    type Collected = Rc<RefCell<Vec<Task>>>;

    thread_local! {
        static REGISTRY: TaskRegistry = TaskRegistry::new(crate::executor::WeakExecutorContext::default());
    }

    struct OwnerGuard {
        _identity: test_executor_id::Guard,
    }

    impl Drop for OwnerGuard {
        fn drop(&mut self) {
            REGISTRY.with(|registry| {
                registry.request_shutdown();
                registry.cleanup_active.set(true);
                while registry.shutdown_next() {}
                registry.cleanup_active.set(false);
            });
        }
    }

    /// Claims `EXECUTOR_ID` for this thread so the task's drop and wake paths
    /// take the owning-thread branch. Must be held for as long as any task from
    /// `spawn_capturing` is alive.
    fn own_tasks() -> OwnerGuard {
        OwnerGuard {
            _identity: test_executor_id::scoped(EXECUTOR_ID),
        }
    }

    /// Spawns a counted runnable in the reactor-free test registry.
    fn spawn_capturing<F, R>(future: F, sink: Collected) -> (Task, JoinHandle<R>)
    where
        F: Future<Output = R>,
        R: 'static,
    {
        REGISTRY.with(|registry| {
            task_impl::spawn_local(
                EXECUTOR_ID,
                QUEUE_INDEX,
                registry,
                future,
                move |runnable: Task| sink.borrow_mut().push(runnable),
                false,
            )
        })
    }

    /// Registering a task's own waker clones it; cancellation wakes it inline.
    /// Both callbacks access the same header. Under Miri's Stacked Borrows,
    /// holding an exclusive header reference across either callback is UB,
    /// even though the notification never polls the future synchronously.
    #[test]
    fn self_waker_can_be_registered_and_notified_on_cancellation() {
        let _owned = own_tasks();
        let sink: Collected = Default::default();
        let saved_waker = Rc::new(RefCell::new(None));
        let saved = saved_waker.clone();
        let polls = Rc::new(std::cell::Cell::new(0));
        let poll_count = polls.clone();
        let (runnable, mut handle) = spawn_capturing(
            std::future::poll_fn(move |cx| {
                poll_count.set(poll_count.get() + 1);
                *saved.borrow_mut() = Some(cx.waker().clone());
                Poll::<()>::Pending
            }),
            sink.clone(),
        );
        runnable.run();
        let waker = saved_waker
            .borrow_mut()
            .take()
            .expect("task was not polled");
        let mut cx = Context::from_waker(&waker);

        assert_eq!(Pin::new(&mut handle).poll(&mut cx), Poll::Pending);
        handle.cancel();
        assert_eq!(polls.get(), 1, "notification must not poll the future");
        assert_eq!(sink.borrow().len(), 1, "cancellation must queue cleanup");

        let cleanup = sink.borrow_mut().pop().expect("missing cleanup runnable");
        cleanup.run();
        assert_eq!(poll_once(&mut handle), Poll::Ready(None));
        assert_eq!(polls.get(), 1, "cleanup must not poll the cancelled future");
        assert!(sink.borrow().is_empty());
    }

    #[test]
    fn run_to_completion_yields_output() {
        let _owned = own_tasks();
        let sink: Collected = Default::default();
        let (runnable, mut handle) = spawn_capturing(async { 42u32 }, sink.clone());

        runnable.run();
        assert!(
            sink.borrow().is_empty(),
            "a ready task should not reschedule"
        );
        assert_eq!(poll_once(&mut handle), Poll::Ready(Some(42)));
    }

    #[test]
    fn dropping_the_runnable_cancels() {
        let _owned = own_tasks();
        let sink: Collected = Default::default();
        let (runnable, mut handle) = spawn_capturing(async { 7u32 }, sink.clone());

        // A runnable reaches a droppable place by being scheduled; dropping it
        // there instead of running it cancels the task and drops its future.
        // That is what the executor does when the task queue has gone away.
        runnable.schedule();
        drop(sink.borrow_mut().pop().expect("scheduled"));
        assert_eq!(poll_once(&mut handle), Poll::Ready(None));
    }

    #[test]
    fn dropping_the_handle_leaves_the_task_runnable() {
        let _owned = own_tasks();
        let sink: Collected = Default::default();
        let (runnable, handle) = spawn_capturing(async { 1u32 }, sink.clone());

        // Detaching: the task must still be safe to run and must tear itself
        // down afterwards, with no handle left to collect the output.
        drop(handle);
        runnable.run();
        assert!(sink.borrow().is_empty());
    }

    #[test]
    fn dropping_both_without_running_frees_the_task() {
        let _owned = own_tasks();
        let sink: Collected = Default::default();
        let (runnable, handle) = spawn_capturing(async { 1u32 }, sink.clone());
        drop(handle);
        runnable.schedule();
        drop(sink.borrow_mut().pop().expect("scheduled"));
        assert!(sink.borrow().is_empty());
    }

    #[test]
    fn scheduling_hands_the_runnable_to_the_schedule_function() {
        let _owned = own_tasks();
        let sink: Collected = Default::default();
        let (runnable, mut handle) = spawn_capturing(async { 9u32 }, sink.clone());

        // `schedule` takes a reference, passes it to the schedule function, and
        // relies on a guard to keep the closure's captured state alive if the
        // task is freed during the call.
        runnable.schedule();
        assert_eq!(sink.borrow().len(), 1);

        let rescheduled = sink.borrow_mut().pop().unwrap();
        rescheduled.run();
        assert_eq!(poll_once(&mut handle), Poll::Ready(Some(9)));
    }

    #[test]
    fn a_task_that_yields_is_rescheduled_and_completes() {
        struct YieldOnce(bool);
        impl Future for YieldOnce {
            type Output = u32;
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
                if self.0 {
                    Poll::Ready(5)
                } else {
                    self.0 = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }

        let _owned = own_tasks();
        let sink: Collected = Default::default();
        let (runnable, mut handle) = spawn_capturing(YieldOnce(false), sink.clone());

        // The first run leaves the task pending. Waking from inside the poll
        // marks it scheduled, and `run` hands it back through the schedule
        // function on the way out.
        runnable.run();
        let rescheduled = sink.borrow_mut().pop().expect("task should reschedule");
        rescheduled.run();
        assert_eq!(poll_once(&mut handle), Poll::Ready(Some(5)));
    }

    #[test]
    fn a_larger_future_is_boxed_and_still_torn_down() {
        // `spawn_local` boxes futures of 2KB or more, taking a different
        // allocation path that has to free correctly too.
        let _owned = own_tasks();
        let sink: Collected = Default::default();
        let buf = [7u8; 4096];
        let big = async move { buf[0] as u32 };
        assert!(std::mem::size_of_val(&big) >= 2048);

        let (runnable, mut handle) = spawn_capturing(big, sink.clone());
        runnable.run();
        assert_eq!(poll_once(&mut handle), Poll::Ready(Some(7)));
    }

    #[test]
    fn output_is_dropped_when_the_handle_goes_away_first() {
        // The task completes with an output nobody collects; closing the task
        // has to drop that output rather than leak it.
        let dropped = Rc::new(RefCell::new(false));
        struct NotifyOnDrop(Rc<RefCell<bool>>);
        impl Drop for NotifyOnDrop {
            fn drop(&mut self) {
                *self.0.borrow_mut() = true;
            }
        }

        let _owned = own_tasks();
        let sink: Collected = Default::default();
        let flag = dropped.clone();
        let (runnable, handle) = spawn_capturing(async move { NotifyOnDrop(flag) }, sink.clone());

        runnable.run();
        assert!(!*dropped.borrow(), "output held by the handle");
        drop(handle);
        assert!(*dropped.borrow(), "output dropped with the handle");
    }

    #[test]
    fn many_tasks_allocate_and_free() {
        // Repetition, so a refcount that is off by one shows up as a leak or a
        // double free rather than passing by luck.
        let _owned = own_tasks();
        let sink: Collected = Default::default();
        for i in 0..64u32 {
            let (runnable, mut handle) = spawn_capturing(async move { i }, sink.clone());
            runnable.run();
            assert_eq!(poll_once(&mut handle), Poll::Ready(Some(i)));
        }
        assert!(sink.borrow().is_empty());
    }
}
