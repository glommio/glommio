// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience

//! Lifecycle coverage for thread-local task payloads, results, and schedules.

use std::{
    cell::{Cell, RefCell},
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
    thread,
};

use futures_lite::future::yield_now;

use super::{
    task_impl,
    test_support::{completed_waker, executor, AllocationProbe, DropGuard, DropProbe},
    waker_fn::dummy_waker,
};

#[derive(Clone, Copy)]
enum Behavior {
    Complete,
    Pending,
    Panic,
}

type WakerSlot = Rc<RefCell<Option<Waker>>>;
type PollCount = Rc<Cell<usize>>;

struct PayloadFuture<R, const N: usize> {
    output: Option<R>,
    waker: WakerSlot,
    polls: PollCount,
    behavior: Behavior,
    _guard: DropGuard,
    // Exercise both branches of spawn_local's future-size threshold.
    _padding: [u8; N],
}

impl<R: Unpin, const N: usize> Future for PayloadFuture<R, N> {
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        let this = self.get_mut();
        this.polls.set(this.polls.get() + 1);
        if this.waker.borrow().is_none() {
            *this.waker.borrow_mut() = Some(cx.waker().clone());
        }
        match this.behavior {
            Behavior::Complete => Poll::Ready(this.output.take().expect("task repolled")),
            Behavior::Pending => Poll::Pending,
            Behavior::Panic => panic!("intentional lifecycle test panic"),
        }
    }
}

fn payload_future<R, const N: usize>(
    output: R,
    behavior: Behavior,
    guard: DropGuard,
) -> (PayloadFuture<R, N>, WakerSlot, PollCount) {
    let waker = Rc::new(RefCell::new(None));
    let polls = Rc::new(Cell::new(0));
    let future = PayloadFuture {
        output: Some(output),
        waker: waker.clone(),
        polls: polls.clone(),
        behavior,
        _guard: guard,
        _padding: [0; N],
    };
    assert_eq!(std::mem::size_of_val(&future) >= 2048, N >= 2048);
    (future, waker, polls)
}

fn release_waker(waker: Waker, foreign: bool) {
    if foreign {
        thread::spawn(move || drop(waker))
            .join()
            .expect("foreign waker release panicked");
    } else {
        drop(waker);
    }
}

fn completed_output<const N: usize>(waker_first: bool, foreign: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let output_drops = DropProbe::new();
    let (future, saved_waker, polls) = payload_future::<_, N>(
        output_drops.guard(),
        Behavior::Complete,
        future_drops.guard(),
    );
    let (handle, allocation) = ex.run(async move {
        let handle = crate::spawn_local(future).detach();
        let allocation = AllocationProbe::track_handle(&handle);
        while polls.get() == 0 {
            yield_now().await;
        }
        (handle, allocation)
    });
    let waker = saved_waker
        .borrow_mut()
        .take()
        .expect("task was not polled");
    future_drops.assert_dropped_once();
    output_drops.assert_not_dropped();
    allocation.assert_live();

    drop(ex);
    output_drops.assert_not_dropped();
    if waker_first {
        release_waker(waker, foreign);
        // The handle must retain its unread !Send result after the last waker.
        allocation.assert_live();
        output_drops.assert_not_dropped();
        let mut handle = handle;
        let dummy = dummy_waker();
        let output = match Pin::new(&mut handle).poll(&mut Context::from_waker(&dummy)) {
            Poll::Ready(Some(output)) => output,
            _ => panic!("completed result was lost during shutdown"),
        };
        drop(handle);
        output_drops.assert_not_dropped();
        drop(output);
    } else {
        drop(handle);
        output_drops.assert_dropped_once();
        allocation.assert_live();
        release_waker(waker, foreign);
    }
    output_drops.assert_dropped_once();
    future_drops.assert_dropped_once();
    allocation.assert_freed();
}

fn suspended_shutdown<const N: usize>(foreign: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let (future, saved_waker, polls) =
        payload_future::<_, N>((), Behavior::Pending, future_drops.guard());
    let allocation = ex.run(async {
        let handle = crate::spawn_local(future).detach();
        let allocation = AllocationProbe::track_handle(&handle);
        while polls.get() == 0 {
            yield_now().await;
        }
        // There is no runnable task left in any executor queue.
        drop(handle);
        allocation
    });
    let waker = saved_waker
        .borrow_mut()
        .take()
        .expect("task was not polled");
    future_drops.assert_not_dropped();
    drop(ex);
    let drops_at_shutdown = future_drops.drops();
    release_waker(waker, foreign);

    assert_eq!(
        drops_at_shutdown, 1,
        "shutdown retained the suspended future"
    );
    assert_eq!(polls.get(), 1, "shutdown polled the suspended future again");
    future_drops.assert_dropped_once();
    allocation.assert_freed();
}

fn canceled_future<const N: usize>(foreign: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let (future, saved_waker, polls) =
        payload_future::<_, N>((), Behavior::Pending, future_drops.guard());
    let allocation = ex.run(async {
        let handle = crate::spawn_local(future).detach();
        let allocation = AllocationProbe::track_handle(&handle);
        while polls.get() == 0 {
            yield_now().await;
        }
        handle.cancel();
        assert!(handle.await.is_none(), "canceled task produced a result");
        allocation
    });
    let waker = saved_waker
        .borrow_mut()
        .take()
        .expect("task was not polled");
    future_drops.assert_dropped_once();
    allocation.assert_live();
    drop(ex);
    release_waker(waker, foreign);

    assert_eq!(polls.get(), 1, "cancellation polled the future again");
    future_drops.assert_dropped_once();
    allocation.assert_freed();
}

fn suspended_handle_survives_shutdown<const N: usize>(foreign: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let (future, saved_waker, polls) =
        payload_future::<_, N>((), Behavior::Pending, future_drops.guard());
    let (mut handle, allocation) = ex.run(async {
        let handle = crate::spawn_local(future).detach();
        let allocation = AllocationProbe::track_handle(&handle);
        while polls.get() == 0 {
            yield_now().await;
        }
        (handle, allocation)
    });
    let waker = saved_waker
        .borrow_mut()
        .take()
        .expect("task was not polled");
    future_drops.assert_not_dropped();
    drop(ex);
    let drops_at_shutdown = future_drops.drops();
    allocation.assert_live();

    // Poll once so missing shutdown cancellation fails without hanging the test.
    let dummy = dummy_waker();
    let result = Pin::new(&mut handle).poll(&mut Context::from_waker(&dummy));
    drop(handle);
    release_waker(waker, foreign);

    assert!(
        matches!(result, Poll::Ready(None)),
        "shutdown did not cancel the task"
    );
    assert_eq!(
        drops_at_shutdown, 1,
        "shutdown retained the suspended future"
    );
    assert_eq!(polls.get(), 1, "shutdown polled the suspended future again");
    future_drops.assert_dropped_once();
    allocation.assert_freed();
}

fn panicking_future<const N: usize>(foreign: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let (future, saved_waker, polls) =
        payload_future::<_, N>((), Behavior::Panic, future_drops.guard());
    let result = catch_unwind(AssertUnwindSafe(|| ex.run(future)));
    let panic = result.expect_err("future did not panic");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"intentional lifecycle test panic"),
        "executor panicked for an unrelated reason",
    );
    let waker = saved_waker
        .borrow_mut()
        .take()
        .expect("task was not polled");
    let allocation = AllocationProbe::track_waker(&waker);
    future_drops.assert_dropped_once();
    drop(ex);
    release_waker(waker, foreign);

    assert_eq!(polls.get(), 1);
    future_drops.assert_dropped_once();
    allocation.assert_freed();
}

fn schedule_capture<const N: usize>() {
    let ex = executor();
    let future_drops = DropProbe::new();
    let schedule_drops = DropProbe::new();
    let (future, saved_waker, _) =
        payload_future::<_, N>((), Behavior::Complete, future_drops.guard());
    let schedule_guard = schedule_drops.guard();
    let allocation = ex.run(async {
        // A real executor ID and owner context preserve the raw task contract.
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            future,
            move |task| {
                let _ = &schedule_guard;
                drop(task);
            },
            false,
        );
        let allocation = AllocationProbe::track_handle(&handle);
        task.run_right_away();
        assert!(handle.await.is_some());
        allocation
    });
    let waker = saved_waker
        .borrow_mut()
        .take()
        .expect("task was not polled");
    future_drops.assert_dropped_once();
    drop(ex);
    let drops_before_foreign_release = schedule_drops.drops();
    release_waker(waker, true);

    assert_eq!(
        drops_before_foreign_release, 1,
        "the !Send schedule capture survived owner cleanup",
    );
    schedule_drops.assert_dropped_once();
    future_drops.assert_dropped_once();
    allocation.assert_freed();
}

struct ReentrantOutput {
    other_waker: Option<Waker>,
    wake: bool,
    _guard: DropGuard,
}

impl Drop for ReentrantOutput {
    fn drop(&mut self) {
        if let Some(waker) = self.other_waker.take() {
            if self.wake {
                waker.wake();
            } else {
                drop(waker);
            }
        }
    }
}

fn reentrant_output<const N: usize>(wake: bool) {
    let ex = executor();
    let (other_waker, other_allocation, other_future_drops) = completed_waker::<N>(&ex);
    let future_drops = DropProbe::new();
    let output_drops = DropProbe::new();
    let (future, saved_waker, polls) = payload_future::<_, N>(
        ReentrantOutput {
            other_waker: Some(other_waker),
            wake,
            _guard: output_drops.guard(),
        },
        Behavior::Complete,
        future_drops.guard(),
    );
    let (handle, allocation) = ex.run(async {
        let handle = crate::spawn_local(future).detach();
        let allocation = AllocationProbe::track_handle(&handle);
        while polls.get() == 0 {
            yield_now().await;
        }
        (handle, allocation)
    });
    let waker = saved_waker
        .borrow_mut()
        .take()
        .expect("task was not polled");
    drop(ex);
    // This destructor reenters another task's raw waker path on the owner.
    drop(handle);
    release_waker(waker, true);

    assert_eq!(polls.get(), 1);
    output_drops.assert_dropped_once();
    future_drops.assert_dropped_once();
    other_future_drops.assert_dropped_once();
    allocation.assert_freed();
    other_allocation.assert_freed();
}

macro_rules! test_both_sizes {
    ($inline:ident, $boxed:ident, $helper:ident $(, $arg:expr)*) => {
        #[test]
        fn $inline() {
            $helper::<0>($($arg),*);
        }

        #[test]
        fn $boxed() {
            $helper::<4096>($($arg),*);
        }
    };
}

test_both_sizes!(
    completed_output_outlives_owner_waker_inline,
    completed_output_outlives_owner_waker_boxed,
    completed_output,
    true,
    false
);
test_both_sizes!(
    completed_output_outlives_foreign_waker_inline,
    completed_output_outlives_foreign_waker_boxed,
    completed_output,
    true,
    true
);
test_both_sizes!(
    completed_output_dropped_before_owner_waker_inline,
    completed_output_dropped_before_owner_waker_boxed,
    completed_output,
    false,
    false
);
test_both_sizes!(
    completed_output_dropped_before_foreign_waker_inline,
    completed_output_dropped_before_foreign_waker_boxed,
    completed_output,
    false,
    true
);
test_both_sizes!(
    suspended_future_shutdown_owner_release_inline,
    suspended_future_shutdown_owner_release_boxed,
    suspended_shutdown,
    false
);
test_both_sizes!(
    suspended_future_shutdown_foreign_release_inline,
    suspended_future_shutdown_foreign_release_boxed,
    suspended_shutdown,
    true
);
test_both_sizes!(
    canceled_future_owner_release_inline,
    canceled_future_owner_release_boxed,
    canceled_future,
    false
);
test_both_sizes!(
    suspended_handle_survives_shutdown_owner_release_inline,
    suspended_handle_survives_shutdown_owner_release_boxed,
    suspended_handle_survives_shutdown,
    false
);
test_both_sizes!(
    suspended_handle_survives_shutdown_foreign_release_inline,
    suspended_handle_survives_shutdown_foreign_release_boxed,
    suspended_handle_survives_shutdown,
    true
);
test_both_sizes!(
    canceled_future_foreign_release_inline,
    canceled_future_foreign_release_boxed,
    canceled_future,
    true
);
test_both_sizes!(
    panicking_future_owner_release_inline,
    panicking_future_owner_release_boxed,
    panicking_future,
    false
);
test_both_sizes!(
    panicking_future_foreign_release_inline,
    panicking_future_foreign_release_boxed,
    panicking_future,
    true
);
test_both_sizes!(
    schedule_capture_dropped_on_owner_inline,
    schedule_capture_dropped_on_owner_boxed,
    schedule_capture
);
test_both_sizes!(
    output_destructor_drops_another_waker_inline,
    output_destructor_drops_another_waker_boxed,
    reentrant_output,
    false
);
test_both_sizes!(
    output_destructor_wakes_another_task_inline,
    output_destructor_wakes_another_task_boxed,
    reentrant_output,
    true
);
