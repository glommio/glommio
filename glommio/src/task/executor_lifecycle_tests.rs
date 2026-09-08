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

fn drop_while_running<const N: usize>() {
    let executor = executor();
    let (waker, allocation, future_drop) = completed_waker::<N>(&executor);
    executor.run(async move { drop(waker) });
    drop(executor);
    future_drop.assert_dropped_once();
    allocation.assert_freed();
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

macro_rules! test_sizes {
    ($inline:ident, $boxed:ident, $body:ident $(, $arg:expr)*) => {
        #[test]
        fn $inline() { $body::<0>($($arg),*); }
        #[test]
        fn $boxed() { $body::<4096>($($arg),*); }
    };
}

test_sizes!(
    completed_task_is_destroyed_while_executor_is_running_inline,
    completed_task_is_destroyed_while_executor_is_running_boxed,
    drop_while_running
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
