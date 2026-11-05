//! Executor-local registration handles and idle callback integration.

use std::{
    fmt,
    rc::{Rc, Weak},
};

pub use glommio_idle::BeforeParkCapacityError;
pub(crate) use glommio_idle::{park_if_idle, IdleWorkers};

use crate::task::registry::TaskRegistry;

/// Keeps a callback registered with [`crate::ExecutorProxy::register_before_park`].
///
/// Dropping the handle unregisters the callback. If a batch is running, the
/// callback remains in that batch and is removed afterward. The handle is
/// local to its executor's thread and may safely outlive the executor.
/// Captured values are destroyed in the owning executor's cleanup context,
/// including when this handle is dropped between runs or in another executor.
#[must_use = "dropping the handle unregisters the callback"]
pub struct BeforeParkHandle {
    registration: Option<glommio_idle::BeforeParkHandle>,
    owner: Weak<TaskRegistry>,
}

impl BeforeParkHandle {
    pub(crate) fn new(
        registration: glommio_idle::BeforeParkHandle,
        owner: &Rc<TaskRegistry>,
    ) -> Self {
        Self {
            registration: Some(registration),
            owner: Rc::downgrade(owner),
        }
    }
}

impl Drop for BeforeParkHandle {
    fn drop(&mut self) {
        let registration = self.registration.take();
        if let Some(context) = self.owner.upgrade().and_then(|owner| owner.context()) {
            context.with_cleanup(None, || drop(registration));
        } else {
            drop(registration);
        }
    }
}

impl fmt::Debug for BeforeParkHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BeforeParkHandle").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    //! Idle-batch integration with spawning, wakeups, and executor lifetime.

    use std::{
        cell::{Cell, RefCell},
        future::{pending, poll_fn},
        panic::{catch_unwind, AssertUnwindSafe},
        rc::Rc,
        task::{Poll, Waker},
    };

    use crate::{
        channels::local_channel, executor, spawn_local, spawn_local_into, BeforeParkCapacityError,
        BeforeParkHandle, LocalExecutor, LocalExecutorBuilder, PoolPlacement, TaskQueueHandle,
    };

    use super::IdleWorkers;

    #[test]
    fn registration_is_limited_to_256_callbacks_and_reuses_slots() {
        let workers = IdleWorkers::default();
        let calls = Rc::new(Cell::new(0));
        let mut handles = (0..256)
            .map(|_| {
                let calls = calls.clone();
                workers
                    .register(move || calls.set(calls.get() + 1))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let error = workers
            .register(|| {})
            .expect_err("the 257th callback must be rejected");
        assert_eq!(error, BeforeParkCapacityError);
        workers.run();
        assert_eq!(calls.get(), 256);

        drop(handles.pop());
        let replacement_calls = Rc::new(Cell::new(0));
        let replacement = workers
            .register({
                let calls = replacement_calls.clone();
                move || calls.set(calls.get() + 1)
            })
            .unwrap();
        workers.run();
        assert_eq!(calls.get(), 511);
        assert_eq!(replacement_calls.get(), 1);
        drop(replacement);
    }

    fn local_executor() -> LocalExecutor {
        LocalExecutorBuilder::default()
            .io_memory(0)
            .blocking_thread_pool_placement(PoolPlacement::Unbound(1))
            .make()
            .expect("failed to create idle-worker test executor")
    }

    #[test]
    fn entire_batch_precedes_spawned_work_and_prevents_parking() {
        let ex = local_executor();
        ex.run(async {
            let (sender, receiver) = local_channel::new_unbounded();
            let sender = Rc::new(sender);
            let visited = Rc::new(Cell::new(0));
            let workers = (0..4)
                .map(|index| {
                    let visited = visited.clone();
                    let sender = sender.clone();
                    executor()
                        .register_before_park(move || {
                            visited.set(visited.get() + 1);
                            let visited = visited.clone();
                            let sender = sender.clone();
                            let future = async move {
                                assert_eq!(
                                    visited.get(),
                                    4,
                                    "spawned work interrupted the idle batch"
                                );
                                sender.try_send(index).unwrap();
                            };
                            if index % 2 == 0 {
                                spawn_local(future).detach();
                            } else {
                                spawn_local_into(future, TaskQueueHandle::default())
                                    .unwrap()
                                    .detach();
                            }
                        })
                        .unwrap()
                })
                .collect::<Vec<_>>();
            let mut completed = Vec::new();
            for _ in 0..4 {
                completed.push(receiver.recv().await.unwrap());
            }
            completed.sort_unstable();
            assert_eq!(completed, vec![0, 1, 2, 3]);
            drop(workers);
        });
    }

    #[test]
    fn worker_mutations_take_effect_in_the_next_batch() {
        let ex = local_executor();
        ex.run(async {
            let (sender, receiver) = local_channel::new_unbounded();
            let sender = Rc::new(sender);
            let visited = Rc::new(Cell::new(0));
            let handles: Rc<RefCell<Vec<BeforeParkHandle>>> = Rc::default();
            let replacement = Rc::new(RefCell::new(None));
            let changed = Rc::new(Cell::new(false));
            for _ in 0..4 {
                let handles_for_worker = handles.clone();
                let replacement = replacement.clone();
                let changed = changed.clone();
                let visited = visited.clone();
                let sender = sender.clone();
                let worker = executor()
                    .register_before_park(move || {
                        visited.set(visited.get() + 1);
                        if !changed.replace(true) {
                            handles_for_worker.borrow_mut().clear();
                            let visited = visited.clone();
                            let sender_for_replacement = sender.clone();
                            *replacement.borrow_mut() =
                                Some(executor().register_before_park(move || {
                                    assert_eq!(visited.get(), 4);
                                    sender_for_replacement.try_send("replacement").unwrap();
                                }));
                            sender.try_send("original").unwrap();
                        }
                    })
                    .unwrap();
                handles.borrow_mut().push(worker);
            }
            assert_eq!(receiver.recv().await, Some("original"));
            assert_eq!(
                visited.get(),
                4,
                "removal skipped members of the current batch"
            );
            assert_eq!(receiver.recv().await, Some("replacement"));
            drop(replacement.borrow_mut().take());
        });
    }

    #[test]
    fn each_batch_moves_its_first_worker_and_preserves_fn_mut_state() {
        let ex = local_executor();
        ex.run(async {
            let visited = Rc::new(RefCell::new(Vec::new()));
            let waker: Rc<RefCell<Option<Waker>>> = Rc::default();
            let workers = (0..4)
                .map(|index| {
                    let visited = visited.clone();
                    let waker = waker.clone();
                    let mut count = 0;
                    executor()
                        .register_before_park(move || {
                            count += 1;
                            visited.borrow_mut().push((index, count));
                            if let Some(waker) = waker.borrow_mut().take() {
                                waker.wake();
                            }
                        })
                        .unwrap()
                })
                .collect::<Vec<_>>();
            for batch in 1..=5 {
                poll_fn(|cx| {
                    if visited.borrow().len() >= batch * 4 {
                        Poll::Ready(())
                    } else {
                        *waker.borrow_mut() = Some(cx.waker().clone());
                        Poll::Pending
                    }
                })
                .await;
                let visited = visited.borrow();
                assert_eq!(visited.len(), batch * 4);
                assert!(visited[(batch - 1) * 4..]
                    .iter()
                    .all(|(_, count)| *count == batch));
                if batch > 1 {
                    assert_ne!(visited[(batch - 1) * 4].0, visited[(batch - 2) * 4].0);
                }
            }
            drop(workers);
        });
    }

    #[test]
    fn runnable_tasks_do_not_trigger_idle_workers() {
        let ex = local_executor();
        let calls = Rc::new(Cell::new(0));
        let worker = ex.run(async {
            let calls = calls.clone();
            let worker = executor()
                .register_before_park(move || calls.set(calls.get() + 1))
                .unwrap();
            for _ in 0..10 {
                executor().yield_now().await;
            }
            worker
        });
        assert_eq!(calls.get(), 0);
        drop(worker);
        ex.run(async {
            let (sender, receiver) = local_channel::new_bounded(1);
            let _worker = executor()
                .register_before_park(move || sender.try_send(()).unwrap())
                .unwrap();
            receiver.recv().await.unwrap();
        });
        assert_eq!(calls.get(), 0, "dropped worker was invoked");
    }

    struct OnDrop(Option<Box<dyn FnOnce()>>);

    impl Drop for OnDrop {
        fn drop(&mut self) {
            if let Some(callback) = self.0.take() {
                callback();
            }
        }
    }

    #[test]
    fn dropping_a_handle_between_runs_enters_its_owner_context() {
        let owner = local_executor();
        let dropped = Rc::new(Cell::new(false));
        let worker = owner.run(async {
            let owner_id = executor().id();
            let guard = OnDrop(Some(Box::new({
                let dropped = dropped.clone();
                move || {
                    assert_eq!(executor().id(), owner_id);
                    assert_eq!(executor().current_task_queue(), TaskQueueHandle::default());
                    let nested = executor().register_before_park(|| {}).unwrap();
                    drop(nested);
                    spawn_local(async move { dropped.set(true) }).detach();
                }
            })));
            executor()
                .register_before_park(move || {
                    let _ = &guard;
                })
                .unwrap()
        });
        drop(worker);
        assert!(
            dropped.get(),
            "callback destruction did not poll its spawned task"
        );
        crate::executor::with_executor_context(|context| assert!(context.is_none()));
    }

    #[test]
    fn dropping_a_handle_in_another_executor_restores_foreign_context() {
        let owner = local_executor();
        let other = local_executor();
        let dropped = Rc::new(Cell::new(false));
        let worker = owner.run(async {
            let owner_id = executor().id();
            let guard = OnDrop(Some(Box::new({
                let dropped = dropped.clone();
                move || {
                    assert_eq!(executor().id(), owner_id);
                    dropped.set(true);
                }
            })));
            executor()
                .register_before_park(move || {
                    let _ = &guard;
                })
                .unwrap()
        });
        other.run(async {
            let other_id = executor().id();
            let other_queue = executor().current_task_queue();
            let reactor = executor().reactor();
            drop(worker);
            assert!(dropped.get());
            assert_eq!(executor().id(), other_id);
            assert_eq!(executor().current_task_queue(), other_queue);
            assert!(Rc::ptr_eq(&executor().reactor(), &reactor));
        });
    }

    #[test]
    fn callback_destructor_panic_restores_foreign_context() {
        let owner = local_executor();
        let other = local_executor();
        let worker = owner.run(async {
            let owner_id = executor().id();
            let guard = OnDrop(Some(Box::new(move || {
                assert_eq!(executor().id(), owner_id);
                panic!("callback destructor failed");
            })));
            executor()
                .register_before_park(move || {
                    let _ = &guard;
                })
                .unwrap()
        });
        other.run(async {
            let other_id = executor().id();
            let other_queue = executor().current_task_queue();
            let error = catch_unwind(AssertUnwindSafe(|| drop(worker)))
                .expect_err("callback destructor should unwind");
            assert_eq!(
                error.downcast_ref::<&str>(),
                Some(&"callback destructor failed")
            );
            assert_eq!(executor().id(), other_id);
            assert_eq!(executor().current_task_queue(), other_queue);
        });
        owner.run(async {
            let _worker = executor().register_before_park(|| {}).unwrap();
        });
    }

    #[test]
    fn fallible_registration_reports_capacity_and_releases_rejected_captures() {
        let ex = local_executor();
        ex.run(async {
            let mut handles = (0..256)
                .map(|_| executor().register_before_park(|| {}).unwrap())
                .collect::<Vec<_>>();
            let dropped = Rc::new(Cell::new(false));
            let owner_id = executor().id();
            let guard = OnDrop(Some(Box::new({
                let dropped = dropped.clone();
                move || {
                    assert_eq!(executor().id(), owner_id);
                    dropped.set(true);
                }
            })));
            let error = executor()
                .register_before_park(move || {
                    let _ = &guard;
                })
                .unwrap_err();
            assert_eq!(error, BeforeParkCapacityError);
            assert!(dropped.get());
            drop(handles.pop());
            let replacement = executor().register_before_park(|| {}).unwrap();
            drop(replacement);
        });
    }

    #[test]
    fn executor_shutdown_releases_workers_and_restores_foreign_context() {
        let owner = local_executor();
        let other = local_executor();
        let dropped = Rc::new(Cell::new(false));
        let (worker, weak_reactor) = owner.run(async {
            let reactor = executor().reactor();
            let weak = Rc::downgrade(&reactor);
            let owner_id = owner.id();
            let guard = OnDrop(Some(Box::new({
                let dropped = dropped.clone();
                move || {
                    assert_eq!(executor().id(), owner_id);
                    assert_eq!(executor().current_task_queue(), TaskQueueHandle::default());
                    let worker = executor().register_before_park(|| {}).unwrap();
                    drop(worker);
                    dropped.set(true);
                }
            })));
            let worker = executor()
                .register_before_park(move || {
                    let _ = (&guard, &reactor);
                })
                .unwrap();
            (worker, weak)
        });
        other.run(async {
            let other_id = executor().id();
            drop(owner);
            assert_eq!(executor().id(), other_id);
        });
        assert!(dropped.get());
        assert!(
            weak_reactor.upgrade().is_none(),
            "callback kept the reactor alive"
        );
        drop(worker);
    }

    #[test]
    fn callback_panic_restores_eager_spawning() {
        let ex = local_executor();
        let worker = ex.run(async {
            executor()
                .register_before_park(|| panic!("idle worker failed"))
                .unwrap()
        });
        assert!(catch_unwind(AssertUnwindSafe(|| ex.run(pending::<()>()))).is_err());
        drop(worker);
        ex.run(async {
            let polled = Rc::new(Cell::new(false));
            let task = spawn_local({
                let polled = polled.clone();
                async move { polled.set(true) }
            });
            assert!(polled.get(), "idle-batch spawn deferral survived unwinding");
            task.await;
        });
    }

    #[test]
    #[should_panic(expected = "this thread doesn't have a LocalExecutor running")]
    fn registration_requires_a_running_executor() {
        let _worker = executor().register_before_park(|| {}).unwrap();
    }
}
