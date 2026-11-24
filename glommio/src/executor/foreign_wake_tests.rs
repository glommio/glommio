//! FIFO ordering, bounded preference, and cleanup of foreign-ready tasks.

use std::{
    cell::RefCell,
    panic::{catch_unwind, AssertUnwindSafe},
    rc::Rc,
};

use super::{with_foreign_wake, LocalExecutor, TaskQueue, TaskQueueHandle, LOCAL_EX};
use crate::{Latency, Shares};

fn queue(handle: TaskQueueHandle) -> Rc<RefCell<TaskQueue>> {
    LOCAL_EX.with(|context| context.get_queue(&handle).unwrap())
}

fn run_queued(queue: &Rc<RefCell<TaskQueue>>) {
    loop {
        let runnable = queue.borrow_mut().get_task();
        let Some(runnable) = runnable else { break };
        runnable.run();
    }
}

#[test]
fn foreign_ready_tasks_preserve_fifo_and_precede_local_tasks() {
    LocalExecutor::default().run(async {
        let handle = crate::executor().create_task_queue(
            Shares::default(),
            Latency::NotImportant,
            "priority",
        );
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut tasks = Vec::new();
        for id in 0..3 {
            let events = events.clone();
            tasks.push(
                crate::spawn_local_into(async move { events.borrow_mut().push(id) }, handle)
                    .unwrap(),
            );
        }
        with_foreign_wake(|| {
            for id in 10..13 {
                let events = events.clone();
                tasks.push(
                    crate::spawn_local_into(async move { events.borrow_mut().push(id) }, handle)
                        .unwrap(),
                );
            }
        });
        run_queued(&queue(handle));
        assert_eq!(*events.borrow(), [10, 11, 12, 0, 1, 2]);
        for task in tasks {
            task.await;
        }
    });
}

#[test]
fn local_work_gets_a_turn_after_thirty_two_foreign_ready_tasks() {
    LocalExecutor::default().run(async {
        let handle = crate::executor().create_task_queue(
            Shares::default(),
            Latency::NotImportant,
            "fairness",
        );
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut tasks = Vec::new();
        let local_events = events.clone();
        tasks.push(
            crate::spawn_local_into(async move { local_events.borrow_mut().push(100) }, handle)
                .unwrap(),
        );
        with_foreign_wake(|| {
            for id in 0..64 {
                let events = events.clone();
                tasks.push(
                    crate::spawn_local_into(async move { events.borrow_mut().push(id) }, handle)
                        .unwrap(),
                );
            }
        });
        run_queued(&queue(handle));
        assert_eq!(events.borrow()[32], 100);
        for task in tasks {
            task.await;
        }
    });
}

#[test]
fn unwinding_a_foreign_wake_restores_ordinary_scheduling() {
    let result = catch_unwind(AssertUnwindSafe(|| {
        with_foreign_wake(|| panic!("injected wake panic"))
    }));
    assert!(result.is_err());
    assert!(!super::is_foreign_wake());
}

#[test]
fn dropping_an_executor_releases_foreign_ready_futures() {
    struct DropCount(Rc<RefCell<usize>>);
    impl Drop for DropCount {
        fn drop(&mut self) {
            *self.0.borrow_mut() += 1;
        }
    }
    let dropped = Rc::new(RefCell::new(0));
    {
        let executor = LocalExecutor::default();
        executor.run(async {
            let handle = crate::executor().create_task_queue(
                Shares::default(),
                Latency::NotImportant,
                "cleanup",
            );
            let guard = DropCount(dropped.clone());
            with_foreign_wake(|| {
                crate::spawn_local_into(
                    async move {
                        let _guard = guard;
                        std::future::pending::<()>().await;
                    },
                    handle,
                )
                .unwrap()
                .detach()
            });
        });
    }
    assert_eq!(*dropped.borrow(), 1);
}
