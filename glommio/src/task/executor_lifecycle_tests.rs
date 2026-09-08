//! Completed-task cleanup across executor and OS-thread lifetimes.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll, Waker},
    thread,
};

use super::test_support::{completed_waker, executor, AllocationProbe, DropGuard, DropProbe};

fn drop_after_shutdown<const N: usize>(foreign: bool) {
    let executor = executor();
    let (waker, allocation, future_drop) = completed_waker::<N>(&executor);
    drop(executor);

    if foreign {
        thread::spawn(move || drop(waker))
            .join()
            .expect("foreign waker drop panicked");
    } else {
        // The owner is still this OS thread, although no executor is installed.
        drop(waker);
    }

    future_drop.assert_dropped_once();
    allocation.assert_freed();
}

fn drop_while_running<const N: usize>() {
    let executor = executor();
    let (waker, allocation, future_drop) = completed_waker::<N>(&executor);
    executor.run(async move { drop(waker) });
    drop(executor);
    future_drop.assert_dropped_once();
    allocation.assert_freed();
}

fn drop_while_idle<const N: usize>() {
    let executor = executor();
    let (waker, allocation, future_drop) = completed_waker::<N>(&executor);
    drop(waker);
    // A completed task must not need another run() to reclaim its allocation.
    allocation.assert_freed();
    future_drop.assert_dropped_once();
    drop(executor);
}

fn drop_while_another_executor_runs<const N: usize>() {
    let owner = executor();
    let (waker, allocation, future_drop) = completed_waker::<N>(&owner);
    let other = executor();
    assert_ne!(owner.id(), other.id());
    other.run(async move { drop(waker) });
    // Running a different executor on this thread must not redirect cleanup to it.
    allocation.assert_freed();
    future_drop.assert_dropped_once();
    drop(other);
    drop(owner);
}

fn exercise_late_waker(waker: Waker) {
    let first = waker.clone();
    let second = first.clone();
    for _ in 0..4 {
        second.wake_by_ref();
    }
    first.wake();
    let third = second.clone();
    drop(waker);
    third.wake();
    drop(second);
}

struct CountedWaker<const N: usize> {
    polls: Arc<AtomicUsize>,
    _guard: DropGuard,
    _padding: [u8; N],
}

impl<const N: usize> Future for CountedWaker<N> {
    type Output = Waker;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Waker> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(cx.waker().clone())
    }
}

fn late_wakes<const N: usize>(foreign: bool, shutdown: bool) {
    let executor = executor();
    let polls = Arc::new(AtomicUsize::new(0));
    let future_drop = DropProbe::new();
    let future = CountedWaker {
        polls: polls.clone(),
        _guard: future_drop.guard(),
        _padding: [0; N],
    };
    assert_eq!(std::mem::size_of_val(&future) >= 2048, N >= 2048);
    let waker = executor.run(future);
    let allocation = AllocationProbe::track_waker(&waker);
    let executor = if shutdown {
        drop(executor);
        None
    } else {
        Some(executor)
    };

    if foreign {
        thread::spawn(move || exercise_late_waker(waker))
            .join()
            .expect("late foreign waker operation panicked");
    } else if let Some(executor) = &executor {
        executor.run(async move { exercise_late_waker(waker) });
    } else {
        exercise_late_waker(waker);
    }

    // In the live-executor case, process notifications queued by the foreign worker.
    if let Some(executor) = &executor {
        executor.run(async {});
    }
    drop(executor);
    assert_eq!(
        polls.load(Ordering::SeqCst),
        1,
        "completed task was polled again"
    );
    future_drop.assert_dropped_once();
    allocation.assert_freed();
}

fn owner_thread_exits<const N: usize>() {
    let (waker, allocation, future_drop) = thread::spawn(|| {
        let executor = executor();
        let observed = completed_waker::<N>(&executor);
        drop(executor);
        observed
    })
    .join()
    .expect("owning executor thread panicked");

    allocation.assert_live();
    exercise_late_waker(waker);
    future_drop.assert_dropped_once();
    allocation.assert_freed();
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
    completed_task_is_destroyed_after_executor_shutdown_on_original_thread_inline,
    completed_task_is_destroyed_after_executor_shutdown_on_original_thread_boxed,
    drop_after_shutdown,
    false
);
test_sizes!(
    completed_task_is_destroyed_after_executor_shutdown_on_foreign_thread_inline,
    completed_task_is_destroyed_after_executor_shutdown_on_foreign_thread_boxed,
    drop_after_shutdown,
    true
);
test_sizes!(
    completed_task_is_destroyed_while_executor_is_running_inline,
    completed_task_is_destroyed_while_executor_is_running_boxed,
    drop_while_running
);
test_sizes!(
    completed_task_is_destroyed_while_executor_is_idle_inline,
    completed_task_is_destroyed_while_executor_is_idle_boxed,
    drop_while_idle
);
test_sizes!(
    completed_task_is_destroyed_with_another_executor_active_inline,
    completed_task_is_destroyed_with_another_executor_active_boxed,
    drop_while_another_executor_runs
);
test_sizes!(
    late_owner_wakes_after_completion_inline,
    late_owner_wakes_after_completion_boxed,
    late_wakes,
    false,
    false
);
test_sizes!(
    late_foreign_wakes_after_completion_inline,
    late_foreign_wakes_after_completion_boxed,
    late_wakes,
    true,
    false
);
test_sizes!(
    late_owner_wakes_after_shutdown_inline,
    late_owner_wakes_after_shutdown_boxed,
    late_wakes,
    false,
    true
);
test_sizes!(
    late_foreign_wakes_after_shutdown_inline,
    late_foreign_wakes_after_shutdown_boxed,
    late_wakes,
    true,
    true
);
test_sizes!(
    wakers_outlive_owner_os_thread_inline,
    wakers_outlive_owner_os_thread_boxed,
    owner_thread_exits
);
