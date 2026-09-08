//! Cleanup before shutdown and reentrant scheduling callbacks.

use std::{
    cell::{Cell, RefCell},
    future::{pending, Future},
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    rc::Rc,
    sync::mpsc,
    task::{Context, Poll, Waker},
    thread,
    time::Duration,
};

use futures::channel::oneshot;

use super::{
    task_impl,
    test_support::{executor, AllocationProbe, DropGuard, DropProbe},
};

struct CleanupFuture<const N: usize> {
    ready: Rc<Cell<bool>>,
    wake_on_poll: bool,
    waker: Rc<RefCell<Option<Waker>>>,
    polls: Rc<Cell<usize>>,
    _guard: DropGuard,
    _padding: [u8; N],
}

impl<const N: usize> CleanupFuture<N> {
    fn new(pending: bool, guard: DropGuard) -> Self {
        let future = Self {
            ready: Rc::new(Cell::new(!pending)),
            wake_on_poll: false,
            waker: Rc::default(),
            polls: Rc::default(),
            _guard: guard,
            _padding: [0; N],
        };
        assert_eq!(std::mem::size_of_val(&future) >= 2048, N >= 2048);
        future
    }
}

impl<const N: usize> Future for CleanupFuture<N> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.polls.set(self.polls.get() + 1);
        if !self.ready.get() {
            if self.wake_on_poll {
                cx.waker().wake_by_ref();
            } else {
                *self.waker.borrow_mut() = Some(cx.waker().clone());
            }
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

fn synchronous_schedule<const N: usize>(run: bool, panic: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let schedule_drops = DropProbe::new();
    let future = CleanupFuture::<N>::new(false, future_drops.guard());
    let polls = future.polls.clone();
    let schedule_guard = schedule_drops.guard();
    let callback_drops = schedule_drops.clone();

    ex.run(async {
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            future,
            move |task| {
                let _ = &schedule_guard;
                if run {
                    task.run();
                } else {
                    drop(task);
                }
                // Completing the task cannot destroy the closure while it is borrowed.
                callback_drops.assert_not_dropped();
                assert!(!panic, "intentional scheduling test panic");
            },
            false,
        );
        let allocation = AllocationProbe::track_handle(&handle);
        drop(handle);
        let outcome = catch_unwind(AssertUnwindSafe(|| task.schedule()));
        assert_eq!(outcome.is_err(), panic);
        assert_eq!(polls.get(), usize::from(run));
        future_drops.assert_dropped_once();
        schedule_drops.assert_dropped_once();
        allocation.assert_freed();
    });
}

struct NestedScheduleFuture<const N: usize> {
    polls: Rc<Cell<usize>>,
    _guard: DropGuard,
    _padding: [u8; N],
}

impl<const N: usize> Future for NestedScheduleFuture<N> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let polls = self.polls.get() + 1;
        self.polls.set(polls);
        if polls == 1 {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            assert_eq!(polls, 2, "completed future was polled again");
            Poll::Ready(())
        }
    }
}

fn nested_synchronous_schedule<const N: usize>(panic: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let schedule_drops = DropProbe::new();
    let polls = Rc::new(Cell::new(0));
    let future = NestedScheduleFuture::<N> {
        polls: polls.clone(),
        _guard: future_drops.guard(),
        _padding: [0; N],
    };
    assert_eq!(std::mem::size_of_val(&future) >= 2048, N >= 2048);
    let schedule_guard = schedule_drops.guard();
    let callback_drops = schedule_drops.clone();
    let depth = Rc::new(Cell::new(0usize));
    let callback_depth = depth.clone();
    let max_depth = Rc::new(Cell::new(0usize));
    let callback_max_depth = max_depth.clone();

    ex.run(async {
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            future,
            move |task| {
                let _ = &schedule_guard;
                let current_depth = callback_depth.get() + 1;
                callback_depth.set(current_depth);
                callback_max_depth.set(callback_max_depth.get().max(current_depth));
                task.run();
                // Inner completion must preserve the closure for both callbacks.
                callback_drops.assert_not_dropped();
                assert_eq!(callback_depth.get(), current_depth);
                callback_depth.set(current_depth - 1);
                if current_depth == 1 {
                    assert!(!panic, "intentional outer scheduling test panic");
                }
            },
            false,
        );
        let allocation = AllocationProbe::track_handle(&handle);
        drop(handle);
        let outcome = catch_unwind(AssertUnwindSafe(|| task.schedule()));
        if panic {
            let panic = outcome.expect_err("outer scheduler did not panic");
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"intentional outer scheduling test panic"),
                "task panicked for an unrelated reason",
            );
        } else {
            outcome.expect("nested scheduler panicked");
        }
        assert_eq!(polls.get(), 2);
        assert_eq!(max_depth.get(), 2, "scheduler did not run recursively");
        assert_eq!(depth.get(), 0);
        future_drops.assert_dropped_once();
        schedule_drops.assert_dropped_once();
        allocation.assert_freed();
    });
}

fn abandoned_future<const N: usize>(foreign: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let future = CleanupFuture::<N>::new(true, future_drops.guard());
    let saved_waker = future.waker.clone();
    let polls = future.polls.clone();

    ex.run(async {
        let (task, handle) = task_impl::spawn_local(ex.id(), future, drop, false);
        let allocation = AllocationProbe::track_handle(&handle);
        task.run_right_away();
        drop(handle);
        future_drops.assert_not_dropped();
        let waker = saved_waker
            .borrow_mut()
            .take()
            .expect("task was not polled");
        if foreign {
            thread::spawn(move || drop(waker))
                .join()
                .expect("foreign waker drop panicked");
            // Deterministically process the foreign release while the owner is active.
            crate::sys::get_sleep_notifier_for(ex.id())
                .unwrap()
                .process_foreign_wakes();
        } else {
            drop(waker);
        }
        assert_eq!(polls.get(), 1, "abandoned future was polled again");
        future_drops.assert_dropped_once();
        allocation.assert_freed();
    });
}

/// Consumes the cleanup notification while the foreign drop still owns its reference.
fn cleanup_notification_before_foreign_release<const N: usize>() {
    let ex = executor();
    let future_drops = DropProbe::new();
    let future = CleanupFuture::<N>::new(true, future_drops.guard());
    let saved_waker = future.waker.clone();
    let polls = future.polls.clone();

    ex.run(async {
        let (task, handle) = task_impl::spawn_local(ex.id(), future, drop, false);
        let allocation = AllocationProbe::track_handle(&handle);
        task.run_right_away();
        drop(handle);
        future_drops.assert_not_dropped();
        let waker = saved_waker
            .borrow_mut()
            .take()
            .expect("task was not polled");
        let notifier = crate::sys::get_sleep_notifier_for(ex.id()).unwrap();
        let (queued_tx, queued_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            crate::sys::test_support::pause_next_foreign_wake(queued_tx, resume_rx);
            drop(waker);
        });

        let queued = queued_rx.recv_timeout(Duration::from_secs(10));
        let processed = notifier.process_foreign_wakes();
        let resumed = resume_tx.send(());
        let outcome = worker.join();
        queued.expect("foreign drop did not enqueue a cleanup notification");
        resumed.expect("foreign drop stopped waiting for the owner");
        outcome.expect("foreign waker drop panicked");
        assert_eq!(processed, 1, "owner did not consume the notification first");
        notifier.process_foreign_wakes();

        assert_eq!(polls.get(), 1, "abandoned future was polled again");
        future_drops.assert_dropped_once();
        allocation.assert_freed();
    });
}

/// Drives last-waker cleanup through the reactor while the task owns a channel.

/// A final foreign waker can arrive during the reactor's last check before sleep.
#[test]
fn foreign_cleanup_while_parking_closes_file() {
    let ex = executor();
    ex.run(async {
        let file = crate::io::BufferedFile::open("/dev/null")
            .await
            .expect("failed to open the cleanup test file");
        let saved_waker = Rc::new(RefCell::new(None));
        let slot = saved_waker.clone();
        let handle = crate::spawn_local(async move {
            futures_lite::future::poll_fn(|cx| {
                *slot.borrow_mut() = Some(cx.waker().clone());
                Poll::<()>::Pending
            })
            .await;
            drop(file);
        })
        .detach();
        drop(handle);

        let reactor = crate::executor().reactor();
        let checked_before_sleep = Cell::new(false);
        for _ in 0..100 {
            reactor
                .sys
                .wait(
                    || None,
                    None,
                    0,
                    || {
                        checked_before_sleep.set(true);
                        let waker = saved_waker
                            .borrow_mut()
                            .take()
                            .expect("task was not polled");
                        thread::spawn(move || drop(waker))
                            .join()
                            .expect("foreign waker drop panicked");
                        reactor.sys.process_foreign_wakes()
                    },
                )
                .expect("failed to prepare the reactor for sleep");
            if checked_before_sleep.get() {
                break;
            }
        }
        assert!(
            checked_before_sleep.get(),
            "reactor never reached its last check before sleep"
        );
    });
}

/// Future destructors must be able to spawn tasks that submit I/O.
#[test]
fn abandoned_future_destructor_can_spawn_io() {
    struct SpawnIoOnDrop(Option<oneshot::Sender<()>>);

    impl Drop for SpawnIoOnDrop {
        fn drop(&mut self) {
            let done = self.0.take().expect("I/O cleanup was already started");
            let handle = crate::spawn_local(async move {
                let file = crate::io::BufferedFile::open("/dev/null")
                    .await
                    .expect("failed to open the cleanup test file");
                file.close()
                    .await
                    .expect("failed to close the cleanup test file");
                done.send(()).expect("test stopped waiting for I/O cleanup");
            })
            .detach();
            drop(handle);
        }
    }

    let ex = executor();
    ex.run(async {
        let (done, completed) = oneshot::channel();
        let guard = SpawnIoOnDrop(Some(done));
        let saved_waker = Rc::new(RefCell::new(None));
        let slot = saved_waker.clone();
        let handle = crate::spawn_local(async move {
            let mut first_poll = true;
            futures_lite::future::poll_fn(|cx| {
                if std::mem::replace(&mut first_poll, false) {
                    *slot.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            drop(guard);
        })
        .detach();
        drop(handle);

        let waker = saved_waker
            .borrow_mut()
            .take()
            .expect("task was not polled");
        thread::spawn(move || drop(waker))
            .join()
            .expect("foreign waker drop panicked");
        completed.await.expect("I/O cleanup did not complete");
    });
}

/// Cleanup can release the last executor reference while destroying its task.
#[test]
fn last_handle_drop_with_captured_executor_finishes() {
    let (completed_tx, completed_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let owner = Rc::new(executor());
        let captured_owner = owner.clone();
        let handle = owner
            .spawn(async move {
                pending::<()>().await;
                drop(captured_owner);
            })
            .detach();
        drop(owner);
        drop(handle);
        completed_tx
            .send(())
            .expect("executor-drop test stopped waiting for cleanup");
    });
    completed_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("dropping the last handle did not finish while destroying its executor");
    worker.join().expect("executor-drop test worker panicked");
}

/// Replacing a timer's last task waker must permit timer use during cleanup.
#[test]
fn replacing_timer_waker_allows_timer_use_in_destructor() {
    use crate::timer::Timer;

    /// Records timer registration failures without unwinding through task destruction.
    struct CreateTimerOnDrop(Rc<Cell<Option<bool>>>);

    impl Drop for CreateTimerOnDrop {
        fn drop(&mut self) {
            let succeeded = catch_unwind(|| {
                drop(Timer::new(Duration::from_secs(60)));
            })
            .is_ok();
            self.0.set(Some(succeeded));
        }
    }

    let owner = executor();
    let result = Rc::new(Cell::new(None));
    let observed = result.clone();
    let allocation = owner.run(async move {
        let timer = Rc::new(RefCell::new(Timer::new(Duration::from_secs(60))));
        let shared_timer = timer.clone();
        let guard = CreateTimerOnDrop(observed);
        let (started_tx, started_rx) = futures::channel::oneshot::channel();
        let mut started_tx = Some(started_tx);
        let handle = crate::spawn_local(futures_lite::future::poll_fn(move |cx| {
            let _ = &guard;
            assert!(Pin::new(&mut *shared_timer.borrow_mut())
                .poll(cx)
                .is_pending());
            if let Some(tx) = started_tx.take() {
                tx.send(()).expect("start receiver disappeared");
            }
            Poll::<()>::Pending
        }))
        .detach();
        let allocation = AllocationProbe::track_handle(&handle);
        drop(handle);
        started_rx.await.expect("task did not start");

        futures_lite::future::poll_fn(|cx| {
            assert!(Pin::new(&mut *timer.borrow_mut()).poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        futures_lite::future::yield_now().await;
        allocation
    });
    assert_eq!(
        result.get(),
        Some(true),
        "task destructor ran with timer registry borrowed",
    );
    allocation.assert_freed();
}

fn sole_waker_completes_detached_future<const N: usize>(foreign: bool, by_ref: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let future = CleanupFuture::<N>::new(true, future_drops.guard());
    let saved_waker = future.waker.clone();
    let ready = future.ready.clone();
    let polls = future.polls.clone();

    ex.run(async {
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            future,
            |task| {
                task.run();
            },
            false,
        );
        let allocation = AllocationProbe::track_handle(&handle);
        task.run_right_away();
        drop(handle);
        let waker = saved_waker
            .borrow_mut()
            .take()
            .expect("task was not polled");
        ready.set(true);
        let wake = move || {
            if by_ref {
                waker.wake_by_ref();
                Some(waker)
            } else {
                waker.wake();
                None
            }
        };
        let retained_waker = if foreign {
            let retained = thread::spawn(wake)
                .join()
                .expect("foreign waker wake panicked");
            crate::sys::get_sleep_notifier_for(ex.id())
                .unwrap()
                .process_foreign_wakes();
            retained
        } else {
            wake()
        };
        assert_eq!(polls.get(), 2, "sole live waker canceled a runnable future");
        future_drops.assert_dropped_once();
        if retained_waker.is_some() {
            allocation.assert_live();
        }
        drop(retained_waker);
        allocation.assert_freed();
    });
}

fn panicking_scheduler<const N: usize>() {
    let ex = executor();
    let future_drops = DropProbe::new();
    let schedule_drops = DropProbe::new();
    let schedule_guard = schedule_drops.guard();
    let future = CleanupFuture::<N>::new(true, future_drops.guard());
    let saved_waker = future.waker.clone();
    let polls = future.polls.clone();

    ex.run(async {
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            future,
            move |_task| {
                let _ = &schedule_guard;
                panic!("intentional scheduling test panic");
            },
            false,
        );
        let allocation = AllocationProbe::track_handle(&handle);
        drop(handle);
        task.run_right_away();
        let waker = saved_waker
            .borrow_mut()
            .take()
            .expect("task was not polled");
        let outcome = catch_unwind(AssertUnwindSafe(|| drop(waker)));
        let panic = outcome.expect_err("scheduler did not panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"intentional scheduling test panic"),
            "task panicked for an unrelated reason",
        );
        assert_eq!(polls.get(), 1);
        future_drops.assert_dropped_once();
        schedule_drops.assert_dropped_once();
        allocation.assert_freed();
    });
}

macro_rules! test_sizes {
    ($inline:ident, $boxed:ident, $body:ident $(, $arg:expr)*) => {
        #[test]
        fn $inline() { $body::<0>($($arg),*); }
        #[test]
        fn $boxed() { $body::<4096>($($arg),*); }
    };
}

test_sizes!(
    synchronous_schedule_run_inline,
    synchronous_schedule_run_boxed,
    synchronous_schedule,
    true,
    false
);
test_sizes!(
    synchronous_schedule_drop_inline,
    synchronous_schedule_drop_boxed,
    synchronous_schedule,
    false,
    false
);
test_sizes!(
    synchronous_schedule_run_then_panic_inline,
    synchronous_schedule_run_then_panic_boxed,
    synchronous_schedule,
    true,
    true
);
test_sizes!(
    synchronous_schedule_drop_then_panic_inline,
    synchronous_schedule_drop_then_panic_boxed,
    synchronous_schedule,
    false,
    true
);
test_sizes!(
    nested_synchronous_schedule_inline,
    nested_synchronous_schedule_boxed,
    nested_synchronous_schedule,
    false
);
test_sizes!(
    nested_synchronous_schedule_panic_inline,
    nested_synchronous_schedule_panic_boxed,
    nested_synchronous_schedule,
    true
);
test_sizes!(
    last_suspended_waker_dropped_on_owner_inline,
    last_suspended_waker_dropped_on_owner_boxed,
    abandoned_future,
    false
);
test_sizes!(
    last_suspended_waker_dropped_on_foreign_thread_inline,
    last_suspended_waker_dropped_on_foreign_thread_boxed,
    abandoned_future,
    true
);
test_sizes!(
    cleanup_notification_before_foreign_release_inline,
    cleanup_notification_before_foreign_release_boxed,
    cleanup_notification_before_foreign_release
);
test_sizes!(
    sole_owner_waker_wakes_detached_future_inline,
    sole_owner_waker_wakes_detached_future_boxed,
    sole_waker_completes_detached_future,
    false,
    false
);
test_sizes!(
    sole_owner_waker_wakes_detached_future_by_ref_inline,
    sole_owner_waker_wakes_detached_future_by_ref_boxed,
    sole_waker_completes_detached_future,
    false,
    true
);
test_sizes!(
    sole_foreign_waker_wakes_detached_future_inline,
    sole_foreign_waker_wakes_detached_future_boxed,
    sole_waker_completes_detached_future,
    true,
    false
);
test_sizes!(
    sole_foreign_waker_wakes_detached_future_by_ref_inline,
    sole_foreign_waker_wakes_detached_future_by_ref_boxed,
    sole_waker_completes_detached_future,
    true,
    true
);
test_sizes!(
    cleanup_schedule_panic_releases_last_waker_inline,
    cleanup_schedule_panic_releases_last_waker_boxed,
    panicking_scheduler
);
