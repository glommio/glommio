//! Cleanup before shutdown and reentrant scheduling callbacks.

use std::{
    cell::{Cell, RefCell},
    future::{pending, Future},
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    rc::Rc,
    sync::{mpsc, Arc, Barrier},
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
            &ex.tasks,
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

fn owned_schedule_after_shutdown<const N: usize>() {
    let ex = executor();
    let future_drops = DropProbe::new();
    let schedule_drops = DropProbe::new();
    let future = CleanupFuture::<N>::new(true, future_drops.guard());
    let polls = future.polls.clone();
    let schedule_guard = schedule_drops.guard();
    let scheduled = Rc::new(RefCell::new(None));
    let schedule_slot = scheduled.clone();
    let scheduled_once = Cell::new(false);
    let (task, handle) = ex.run(async {
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            &ex.tasks,
            future,
            move |task| {
                let _ = &schedule_guard;
                assert!(
                    !scheduled_once.replace(true),
                    "schedule closure invoked after executor shutdown"
                );
                *schedule_slot.borrow_mut() = Some(task);
            },
            false,
        );
        task.schedule();
        let task = scheduled
            .borrow_mut()
            .take()
            .expect("initial runnable was not scheduled");
        (task, handle)
    });
    let allocation = AllocationProbe::track_handle(&handle);
    future_drops.assert_not_dropped();
    schedule_drops.assert_not_dropped();

    drop(ex);
    assert_eq!(polls.get(), 0, "shutdown polled an unscheduled future");
    future_drops.assert_dropped_once();
    schedule_drops.assert_dropped_once();
    allocation.assert_live();

    drop(handle);
    allocation.assert_live();
    task.schedule();
    assert_eq!(
        polls.get(),
        0,
        "scheduling after shutdown polled the future"
    );
    future_drops.assert_dropped_once();
    schedule_drops.assert_dropped_once();
    allocation.assert_freed();
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
            &ex.tasks,
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
        let (task, handle) = task_impl::spawn_local(ex.id(), &ex.tasks, future, drop, false);
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
        let (task, handle) = task_impl::spawn_local(ex.id(), &ex.tasks, future, drop, false);
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
fn foreign_cleanup_drops_connected_channel<const N: usize>() {
    let ex = executor();
    let future_drops = DropProbe::new();
    let future = CleanupFuture::<N>::new(true, future_drops.guard());
    let saved_waker = future.waker.clone();
    let polls = future.polls.clone();

    ex.run(async {
        let (sender, receiver) = crate::channels::shared_channel::new_bounded::<u8>(1);
        let (sender, receiver) = futures::future::join(sender.connect(), receiver.connect()).await;
        let handle = crate::spawn_local(async move {
            future.await;
            drop(sender);
        })
        .detach();
        let allocation = AllocationProbe::track_handle(&handle);
        drop(handle);
        let waker = saved_waker
            .borrow_mut()
            .take()
            .expect("task was not polled");
        thread::spawn(move || drop(waker))
            .join()
            .expect("foreign waker drop panicked");

        crate::executor()
            .reactor()
            .spin_poll_io()
            .expect("failed to process the cleanup notification through the reactor");
        futures_lite::future::yield_now().await;

        assert_eq!(polls.get(), 1, "abandoned future was polled again");
        future_drops.assert_dropped_once();
        allocation.assert_freed();
        drop(receiver);
    });
}

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

/// Shutdown destructors must be able to spawn tasks that submit I/O.
#[test]
fn shutdown_destructor_can_spawn_io() {
    shutdown_destructor_io(false);
}

/// Shutdown still needs a usable queue when the default queue was removed.
#[test]
fn shutdown_destructor_can_spawn_io_without_default_queue() {
    shutdown_destructor_io(true);
}

fn shutdown_destructor_io(remove_default_queue: bool) {
    /// Records submission failures without unwinding through task destruction.
    struct SpawnIoOnDrop(Rc<Cell<Option<bool>>>);

    impl Drop for SpawnIoOnDrop {
        fn drop(&mut self) {
            let succeeded = catch_unwind(|| {
                let handle = crate::spawn_local(async {
                    let file = crate::io::BufferedFile::open("/dev/null")
                        .await
                        .expect("failed to open the cleanup test file");
                    file.close()
                        .await
                        .expect("failed to close the cleanup test file");
                })
                .detach();
                drop(handle);
            })
            .is_ok();
            self.0.set(Some(succeeded));
        }
    }

    let owner = executor();
    let result = Rc::new(Cell::new(None));
    let guard = SpawnIoOnDrop(result.clone());
    let handle = owner
        .spawn(async move {
            pending::<()>().await;
            drop(guard);
        })
        .detach();
    let allocation = AllocationProbe::track_handle(&handle);
    if remove_default_queue {
        owner
            .remove_task_queue(crate::TaskQueueHandle::default())
            .expect("idle default queue could not be removed");
    }
    drop(owner);
    drop(handle);

    assert_eq!(
        result.get(),
        Some(true),
        "shutdown cleanup lacked a task queue context",
    );
    allocation.assert_freed();
}

/// Thread-local executors can outlive the debugger's thread-local state.
#[cfg(feature = "debugging")]
#[test]
fn tls_executor_shutdown_with_debugger() {
    thread_local! {
        static OWNER: crate::LocalExecutor = executor();
    }

    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        OWNER.with(|ex| {
            ex.run(async move {
                let handle = crate::spawn_local(futures_lite::future::poll_fn(move |cx| {
                    sender
                        .send(cx.waker().clone())
                        .expect("test stopped waiting for the task's waker");
                    Poll::<()>::Pending
                }))
                .detach();
                drop(handle);
            });
        });
    });
    let waker = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("task was not polled");
    worker
        .join()
        .expect("thread-local executor shutdown panicked");
    drop(waker);
}

/// Task destructors can spawn tasks after the debugger's thread-local teardown.
#[cfg(feature = "debugging")]
#[test]
fn tls_executor_shutdown_can_spawn_with_debugger() {
    struct SpawnTaskOnDrop(mpsc::Sender<()>);

    impl Drop for SpawnTaskOnDrop {
        fn drop(&mut self) {
            let sender = self.0.clone();
            let handle = crate::spawn_local(async move {
                sender.send(()).expect("test stopped waiting for cleanup");
            })
            .detach();
            drop(handle);
        }
    }

    thread_local! {
        static OWNER: crate::LocalExecutor = executor();
    }

    let (sender, receiver) = mpsc::channel();
    let (cleaned_tx, cleaned_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        OWNER.with(|ex| {
            ex.run(async move {
                let guard = SpawnTaskOnDrop(cleaned_tx);
                let handle = crate::spawn_local(futures_lite::future::poll_fn(move |cx| {
                    let _ = &guard;
                    sender
                        .send(cx.waker().clone())
                        .expect("test stopped waiting for the task's waker");
                    Poll::<()>::Pending
                }))
                .detach();
                drop(handle);
            });
        });
    });
    let waker = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("task was not polled");
    worker
        .join()
        .expect("thread-local executor shutdown panicked");
    cleaned_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("task destructor did not spawn during thread-local shutdown");
    drop(waker);
}

/// Dropping an idle handle must preserve the future destructor's owner context.
#[test]
#[expect(
    clippy::async_yields_async,
    reason = "The handle must leave run() so it can be dropped while its executor is idle."
)]
fn idle_handle_drop_preserves_executor_context() {
    struct RecordContext(Rc<RefCell<Vec<Option<usize>>>>);

    impl Drop for RecordContext {
        fn drop(&mut self) {
            self.0.borrow_mut().push(crate::executor::executor_id());
        }
    }

    let owner = executor();
    let observed = Rc::new(RefCell::new(Vec::new()));
    let guard = RecordContext(observed.clone());
    let handle = owner.run(async move {
        crate::spawn_local(async move {
            pending::<()>().await;
            drop(guard);
        })
        .detach()
    });
    let allocation = AllocationProbe::track_handle(&handle);

    drop(handle);

    assert_eq!(crate::executor::executor_id(), None);
    assert_eq!(
        observed.borrow().as_slice(),
        &[Some(owner.id())],
        "the future destructor must run exactly once in its owning executor",
    );
    allocation.assert_freed();
}

/// Synchronous cleanup must restore the executor and queue that it interrupted.
#[test]
fn idle_handle_cleanup_restores_other_executor_context() {
    struct RecordContext(Rc<Cell<Option<(usize, crate::TaskQueueHandle)>>>);

    impl Drop for RecordContext {
        fn drop(&mut self) {
            self.0.set(
                catch_unwind(|| {
                    let ex = crate::executor();
                    (ex.id(), ex.current_task_queue())
                })
                .ok(),
            );
        }
    }

    let owner = executor();
    let observed = Rc::new(Cell::new(None));
    let guard = RecordContext(observed.clone());
    let (handle, owner_queue, owner_reactor) = owner.run(async {
        let queue = crate::executor().create_task_queue(
            crate::Shares::default(),
            crate::Latency::Matters(Duration::from_millis(1)),
            "cleanup owner",
        );
        let (started_tx, started_rx) = oneshot::channel();
        let handle = crate::spawn_local_into(
            async move {
                started_tx.send(()).expect("start receiver disappeared");
                pending::<()>().await;
                drop(guard);
            },
            queue,
        )
        .expect("spawn failed")
        .detach();
        started_rx.await.expect("task did not start");
        (handle, queue, crate::executor().reactor())
    });
    let allocation = AllocationProbe::track_handle(&handle);
    let owner_requirements = owner_reactor.io_requirements();
    let other = executor();
    other.run(async {
        let ex = crate::executor();
        let queue = ex.current_task_queue();
        let requirements = ex.reactor().io_requirements();
        drop(handle);
        assert_eq!(observed.get(), Some((owner.id(), owner_queue)));
        assert_eq!(crate::executor::executor_id(), Some(other.id()));
        assert_eq!(ex.current_task_queue(), queue);
        assert_eq!(
            ex.reactor().io_requirements()._io_handle,
            requirements._io_handle
        );
    });
    assert_eq!(
        owner_reactor.io_requirements()._io_handle,
        owner_requirements._io_handle,
        "cleanup did not restore its owner's I/O requirements",
    );
    assert_eq!(crate::executor::executor_id(), None);
    allocation.assert_freed();
}

/// Last-handle cleanup must finish and reclaim both the task and its executor.
#[test]
fn last_handle_drop_with_captured_executor_finishes() {
    let (completed_tx, completed_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let owner = Rc::new(executor());
        let weak_owner = Rc::downgrade(&owner);
        let captured_owner = owner.clone();
        let future_drops = DropProbe::new();
        let guard = future_drops.guard();
        let handle = owner
            .spawn(async move {
                pending::<()>().await;
                drop((captured_owner, guard));
            })
            .detach();
        let allocation = AllocationProbe::track_handle(&handle);
        drop(owner);
        drop(handle);
        completed_tx
            .send((allocation, future_drops, weak_owner.strong_count()))
            .expect("executor-drop test stopped waiting for cleanup");
    });
    let (allocation, future_drops, remaining_owners) = completed_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("dropping the last handle did not finish while destroying its executor");
    worker.join().expect("executor-drop test worker panicked");
    future_drops.assert_dropped_once();
    allocation.assert_freed();
    assert_eq!(
        remaining_owners, 0,
        "dropping the last handle retained its captured executor",
    );
}

/// A destructor can keep using its context after releasing the public executor.
#[test]
fn last_handle_destructor_can_spawn_after_releasing_executor() {
    struct ReleaseThenSpawn {
        owner: Option<Rc<crate::LocalExecutor>>,
        child_drops: DropProbe,
        child_allocation: Rc<RefCell<Option<AllocationProbe>>>,
        succeeded: Rc<Cell<Option<bool>>>,
    }

    impl Drop for ReleaseThenSpawn {
        fn drop(&mut self) {
            let succeeded = catch_unwind(AssertUnwindSafe(|| {
                let owner = self.owner.take().expect("executor already released");
                let id = owner.id();
                drop(owner);
                assert_eq!(crate::executor::executor_id(), Some(id));
                let _queue = crate::executor().current_task_queue();
                let guard = self.child_drops.guard();
                let child = crate::spawn_local(async move {
                    crate::timer::Timer::new(Duration::from_secs(60)).await;
                    drop(guard);
                })
                .detach();
                *self.child_allocation.borrow_mut() = Some(AllocationProbe::track_handle(&child));
                drop(child);
            }))
            .is_ok();
            self.succeeded.set(Some(succeeded));
        }
    }

    let owner = Rc::new(executor());
    let weak_owner = Rc::downgrade(&owner);
    let weak_registry = Rc::downgrade(&owner.tasks);
    let child_drops = DropProbe::new();
    let child_allocation = Rc::new(RefCell::new(None));
    let succeeded = Rc::new(Cell::new(None));
    let guard = ReleaseThenSpawn {
        owner: Some(owner.clone()),
        child_drops: child_drops.clone(),
        child_allocation: child_allocation.clone(),
        succeeded: succeeded.clone(),
    };
    let handle = owner
        .spawn(async move {
            pending::<()>().await;
            drop(guard);
        })
        .detach();
    let allocation = AllocationProbe::track_handle(&handle);
    drop(owner);
    drop(handle);

    assert_eq!(
        succeeded.get(),
        Some(true),
        "cleanup lost its owner context"
    );
    assert_eq!(crate::executor::executor_id(), None);
    assert_eq!(weak_owner.strong_count(), 0);
    assert_eq!(weak_registry.strong_count(), 0);
    allocation.assert_freed();
    child_drops.assert_dropped_once();
    child_allocation
        .borrow()
        .as_ref()
        .expect("cleanup did not spawn a child task")
        .assert_freed();
}

/// A removed queue drops canceled runnables synchronously, including their executor.
#[test]
fn cancel_after_queue_removal_can_release_executor() {
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let owner = Rc::new(executor());
        let weak_owner = Rc::downgrade(&owner);
        let captured_owner = owner.clone();
        let (handle, queue) = owner.run(async move {
            let queue = crate::executor().create_task_queue(
                crate::Shares::default(),
                crate::Latency::NotImportant,
                "removed",
            );
            let (started_tx, started_rx) = futures::channel::oneshot::channel();
            let handle = crate::spawn_local_into(
                async move {
                    started_tx.send(()).expect("start receiver disappeared");
                    pending::<()>().await;
                    drop(captured_owner);
                },
                queue,
            )
            .expect("spawn failed")
            .detach();
            started_rx.await.expect("task did not start");
            (handle, queue)
        });
        let allocation = AllocationProbe::track_handle(&handle);
        owner
            .remove_task_queue(queue)
            .expect("queue removal failed");
        drop(owner);

        handle.cancel();
        drop(handle);

        assert!(weak_owner.upgrade().is_none(), "task retained its executor");
        allocation.assert_freed();
        finished_tx.send(()).expect("test stopped waiting");
    });
    finished_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("cancellation reentered executor shutdown and hung");
    worker.join().expect("cancellation test worker panicked");
}

/// Replacing a timer's last task waker must permit timer use during cleanup.
#[test]
fn replacing_timer_waker_allows_timer_use_in_destructor() {
    timer_waker_replacement(false);
}

/// A removed queue must not force timer cleanup under the registry's borrow.
#[test]
fn replacing_timer_waker_after_queue_removal_allows_timer_use_in_destructor() {
    timer_waker_replacement(true);
}

fn timer_waker_replacement(remove_queue: bool) {
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
    let owner_ref = &owner;
    let allocation = owner.run(async move {
        let timer = Rc::new(RefCell::new(Timer::new(Duration::from_secs(60))));
        let shared_timer = timer.clone();
        let guard = CreateTimerOnDrop(observed);
        let (started_tx, started_rx) = futures::channel::oneshot::channel();
        let mut started_tx = Some(started_tx);
        let future = futures_lite::future::poll_fn(move |cx| {
            let _ = &guard;
            assert!(Pin::new(&mut *shared_timer.borrow_mut())
                .poll(cx)
                .is_pending());
            if let Some(tx) = started_tx.take() {
                tx.send(()).expect("start receiver disappeared");
            }
            Poll::<()>::Pending
        });
        let (handle, queue) = if remove_queue {
            let queue = crate::executor().create_task_queue(
                crate::Shares::default(),
                crate::Latency::NotImportant,
                "removed",
            );
            let handle = crate::spawn_local_into(future, queue)
                .expect("spawn failed")
                .detach();
            (handle, Some(queue))
        } else {
            (crate::spawn_local(future).detach(), None)
        };
        let allocation = AllocationProbe::track_handle(&handle);
        drop(handle);
        started_rx.await.expect("task did not start");
        if let Some(queue) = queue {
            owner_ref
                .remove_task_queue(queue)
                .expect("queue removal failed");
        }

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

fn handle_and_waker_release_race<const N: usize>() {
    let ex = executor();
    ex.run(async {
        for _ in 0..64 {
            let future_drops = DropProbe::new();
            let future = CleanupFuture::<N>::new(true, future_drops.guard());
            let saved_waker = future.waker.clone();
            let polls = future.polls.clone();
            let (task, handle) = task_impl::spawn_local(ex.id(), &ex.tasks, future, drop, false);
            let allocation = AllocationProbe::track_handle(&handle);
            task.run_right_away();
            let waker = saved_waker
                .borrow_mut()
                .take()
                .expect("task was not polled");
            let barrier = Arc::new(Barrier::new(2));
            let worker_barrier = barrier.clone();
            let worker = thread::spawn(move || {
                worker_barrier.wait();
                drop(waker);
            });
            barrier.wait();
            drop(handle);
            worker.join().expect("foreign waker drop panicked");
            crate::sys::get_sleep_notifier_for(ex.id())
                .unwrap()
                .process_foreign_wakes();
            assert_eq!(polls.get(), 1, "abandoned future was polled again");
            future_drops.assert_dropped_once();
            allocation.assert_freed();
        }
    });
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
            &ex.tasks,
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

struct RacePending<const N: usize> {
    sender: Option<mpsc::Sender<Waker>>,
    barrier: Arc<Barrier>,
    _guard: DropGuard,
    _padding: [u8; N],
}

impl<const N: usize> Future for RacePending<N> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        this.sender
            .take()
            .expect("abandoned future was polled again")
            .send(cx.waker().clone())
            .expect("foreign waker worker exited");
        this.barrier.wait();
        Poll::Pending
    }
}

fn runnable_and_waker_release_race<const N: usize>() {
    let ex = executor();
    ex.run(async {
        for _ in 0..64 {
            let future_drops = DropProbe::new();
            let (sender, receiver) = mpsc::channel::<Waker>();
            let barrier = Arc::new(Barrier::new(2));
            let worker_barrier = barrier.clone();
            let worker = thread::spawn(move || {
                let waker = receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("owner did not poll the future");
                worker_barrier.wait();
                drop(waker);
            });
            let future = RacePending::<N> {
                sender: Some(sender),
                barrier,
                _guard: future_drops.guard(),
                _padding: [0; N],
            };
            assert_eq!(std::mem::size_of_val(&future) >= 2048, N >= 2048);
            let (task, handle) = task_impl::spawn_local(ex.id(), &ex.tasks, future, drop, false);
            let allocation = AllocationProbe::track_handle(&handle);
            drop(handle);
            // The foreign waker release races the runnable release after Pending.
            task.run_right_away();
            worker.join().expect("foreign waker drop panicked");
            crate::sys::get_sleep_notifier_for(ex.id())
                .unwrap()
                .process_foreign_wakes();
            future_drops.assert_dropped_once();
            allocation.assert_freed();
        }
    });
}

enum SchedulePanicTrigger {
    Poll,
    Wake,
    Drop,
}

fn panicking_scheduler<const N: usize>(trigger: SchedulePanicTrigger) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let schedule_drops = DropProbe::new();
    let schedule_guard = schedule_drops.guard();
    let mut future = CleanupFuture::<N>::new(true, future_drops.guard());
    future.wake_on_poll = matches!(trigger, SchedulePanicTrigger::Poll);
    let saved_waker = future.waker.clone();
    let polls = future.polls.clone();

    ex.run(async {
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            &ex.tasks,
            future,
            move |_task| {
                let _ = &schedule_guard;
                panic!("intentional scheduling test panic");
            },
            false,
        );
        let allocation = AllocationProbe::track_handle(&handle);
        drop(handle);
        let outcome = if matches!(trigger, SchedulePanicTrigger::Poll) {
            catch_unwind(AssertUnwindSafe(|| {
                task.run_right_away();
            }))
        } else {
            task.run_right_away();
            let waker = saved_waker
                .borrow_mut()
                .take()
                .expect("task was not polled");
            catch_unwind(AssertUnwindSafe(|| match trigger {
                SchedulePanicTrigger::Drop => drop(waker),
                SchedulePanicTrigger::Wake => waker.wake(),
                SchedulePanicTrigger::Poll => unreachable!(),
            }))
        };
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
    foreign_cleanup_drops_connected_channel_inline,
    foreign_cleanup_drops_connected_channel_boxed,
    foreign_cleanup_drops_connected_channel
);
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
    owned_schedule_after_shutdown_inline,
    owned_schedule_after_shutdown_boxed,
    owned_schedule_after_shutdown
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
    last_suspended_handle_and_waker_race_inline,
    last_suspended_handle_and_waker_race_boxed,
    handle_and_waker_release_race
);
test_sizes!(
    last_suspended_runnable_and_waker_race_inline,
    last_suspended_runnable_and_waker_race_boxed,
    runnable_and_waker_release_race
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
    pending_reschedule_panic_releases_runnable_inline,
    pending_reschedule_panic_releases_runnable_boxed,
    panicking_scheduler,
    SchedulePanicTrigger::Poll
);
test_sizes!(
    wake_schedule_panic_releases_consumed_waker_inline,
    wake_schedule_panic_releases_consumed_waker_boxed,
    panicking_scheduler,
    SchedulePanicTrigger::Wake
);
test_sizes!(
    cleanup_schedule_panic_releases_last_waker_inline,
    cleanup_schedule_panic_releases_last_waker_boxed,
    panicking_scheduler,
    SchedulePanicTrigger::Drop
);
