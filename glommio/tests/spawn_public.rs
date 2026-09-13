// LocalExecutor::spawn, the public non-panicking spawn (issue #695).

use glommio::LocalExecutor;
use std::{cell::Cell, rc::Rc};

#[test]
fn test_spawn_before_run() {
    let executor = LocalExecutor::default();
    let task = executor.spawn(async { 42 });
    assert_eq!(executor.run(task), 42);
}

#[test]
fn test_spawn_multiple_tasks() {
    let executor = LocalExecutor::default();
    let task1 = executor.spawn(async { 1 });
    let task2 = executor.spawn(async { 2 });
    let task3 = executor.spawn(async { 3 });
    let result = executor.run(async move { task1.await + task2.await + task3.await });
    assert_eq!(result, 6);
}

#[test]
fn test_spawn_before_run_with_pending_future() {
    // pending on the first poll, so the reschedule happens with no executor
    // installed on this thread
    let executor = LocalExecutor::default();
    let task = executor.spawn(async {
        futures_lite::future::yield_now().await;
        42
    });
    assert_eq!(executor.run(task), 42);
}

#[test]
fn test_spawn_owned_data_outlives_the_spawning_scope() {
    let executor = LocalExecutor::default();
    let task;
    {
        let local = String::from("moved into the task");
        task = executor.spawn(async move { local });
    } // local is gone, the task owns it
    assert_eq!(executor.run(task), "moved into the task");
}

#[test]
fn test_spawn_detached_before_run_runs_after_its_scope_ends() {
    let executor = LocalExecutor::default();
    let seen = Rc::new(Cell::new(0u64));
    {
        let seen = seen.clone();
        let payload = Box::new([0xDEAD_BEEF_CAFE_1234u64; 8]);
        executor
            .spawn(async move {
                futures_lite::future::yield_now().await;
                seen.set(payload[0]);
            })
            .detach();
    }
    executor.run(async {
        for _ in 0..8 {
            futures_lite::future::yield_now().await;
        }
    });
    assert_eq!(seen.get(), 0xDEAD_BEEF_CAFE_1234);
}
