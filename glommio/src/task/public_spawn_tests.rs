//! Public spawning before an executor runs and while another executor runs.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use super::test_support::{executor, AllocationProbe, DropGuard, DropProbe};

struct CompletedFuture<const N: usize> {
    output: Option<DropGuard>,
    _guard: DropGuard,
    _padding: [u8; N],
}

impl<const N: usize> Future for CompletedFuture<N> {
    type Output = DropGuard;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<DropGuard> {
        Poll::Ready(
            self.get_mut()
                .output
                .take()
                .expect("completed future was polled again"),
        )
    }
}

fn completed_output_after_shutdown<const N: usize>() {
    for consume in [false, true] {
        let owner = executor();
        let future_drops = DropProbe::new();
        let output_drops = DropProbe::new();
        let future = CompletedFuture::<N> {
            output: Some(output_drops.guard()),
            _guard: future_drops.guard(),
            _padding: [0; N],
        };
        assert_eq!(std::mem::size_of_val(&future) >= 2048, N >= 2048);
        let handle = owner.spawn(future).detach();
        let allocation = AllocationProbe::track_handle(&handle);

        future_drops.assert_dropped_once();
        output_drops.assert_not_dropped();
        drop(owner);
        output_drops.assert_not_dropped();
        allocation.assert_live();

        if consume {
            let output = futures_lite::future::block_on(handle)
                .expect("completed output was lost during shutdown");
            allocation.assert_freed();
            output_drops.assert_not_dropped();
            drop(output);
        } else {
            drop(handle);
        }
        future_drops.assert_dropped_once();
        output_drops.assert_dropped_once();
        allocation.assert_freed();
    }
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
    completed_output_after_shutdown_inline,
    completed_output_after_shutdown_boxed,
    completed_output_after_shutdown
);
