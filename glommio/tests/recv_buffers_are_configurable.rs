//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! The receive buffer knobs, driven from outside the crate: every way of
//! stating them resolves, both builders accept them, an executor built with
//! them runs, and the stat that says whether they were enough is readable.

use glommio::{
    LocalExecutorBuilder, LocalExecutorPoolBuilder, Placement, PoolPlacement, RecvBuffers,
};

#[test]
fn the_three_ways_of_stating_the_buffers_resolve() {
    let exact = RecvBuffers::exact(100, 4096);
    assert_eq!(
        (exact.count(), exact.size()),
        (100, 4096),
        "exact means exact"
    );

    let budget = RecvBuffers::budget(16 << 20);
    assert_eq!(budget.count(), 2048);
    assert_eq!(budget.size(), RecvBuffers::DEFAULT_SIZE);
    assert_eq!(budget.bytes(), 16 << 20, "the whole budget is used");
    let odd = RecvBuffers::budget_of(100_000, 4096);
    assert_eq!(odd.count(), 24, "as many as fit, no rounding");
    assert!(odd.bytes() <= 100_000);
    let vast = RecvBuffers::budget(1 << 40);
    assert_eq!(
        vast.count(),
        RecvBuffers::MAX_COUNT,
        "capped at the kernel's limit"
    );

    let shaped = RecvBuffers::for_connections(1000)
        .arrivals_each(4)
        .message_size(512)
        .resolve();
    assert_eq!((shaped.count(), shaped.size()), (4000, 512));
    let defaults = RecvBuffers::for_connections(3).resolve();
    assert_eq!(defaults.count(), 6, "two arrivals each");
    assert_eq!(defaults.size(), RecvBuffers::DEFAULT_SIZE);
    let huge = RecvBuffers::for_connections(usize::MAX).resolve();
    assert_eq!(huge.count(), RecvBuffers::MAX_COUNT, "capped, not refused");

    assert_eq!(RecvBuffers::none().count(), 0);
    assert_eq!(RecvBuffers::default(), RecvBuffers::none());
    assert_eq!(
        RecvBuffers::exact(0, 4096),
        RecvBuffers::none(),
        "off is one value, whatever size came with it"
    );
    assert_eq!(
        RecvBuffers::for_connections(0).resolve(),
        RecvBuffers::none()
    );
    assert_eq!(
        RecvBuffers::for_connections(10).arrivals_each(0).resolve(),
        RecvBuffers::none()
    );
}

#[test]
fn a_single_executor_takes_recv_buffers_and_reports_exhaustion() {
    let ex = LocalExecutorBuilder::new(Placement::Unbound)
        .recv_buffers(RecvBuffers::exact(256, 16 * 1024))
        .make()
        .unwrap();
    let exhausted = ex.run(async {
        glommio::executor()
            .io_stats()
            .all_rings()
            .recv_buffers_exhausted()
    });
    assert_eq!(exhausted, 0);
}

#[test]
fn a_pool_takes_a_shape() {
    let handles = LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(2))
        .recv_buffers(RecvBuffers::for_connections(16).message_size(1024))
        .on_all_shards(|| async { 21 * 2 })
        .unwrap();
    for result in handles.join_all() {
        assert_eq!(result.unwrap(), 42);
    }
}

#[test]
fn no_recv_buffers_is_the_default_and_is_allowed_explicitly() {
    let ex = LocalExecutorBuilder::new(Placement::Unbound)
        .recv_buffers(RecvBuffers::none())
        .make()
        .unwrap();
    ex.run(async {});
    LocalExecutorBuilder::new(Placement::Unbound)
        .make()
        .unwrap()
        .run(async {});
}

#[test]
#[should_panic(expected = "above the kernel's limit")]
fn too_many_recv_buffers_is_refused_at_build_time() {
    let _ = RecvBuffers::exact(32769, 4096);
}

#[test]
#[should_panic(expected = "holds no buffer")]
fn a_budget_below_one_buffer_is_refused() {
    let _ = RecvBuffers::budget(100);
}

/// The kernel describes a buffer with a 32-bit length; a larger one was
/// registered as its length modulo 2^32 and read back whole, which
/// corrupted the stream without an error.
#[test]
#[should_panic(expected = "32-bit length")]
fn a_buffer_the_kernel_cannot_describe_is_refused() {
    let _ = RecvBuffers::exact(1, 1 << 32);
}

#[test]
#[should_panic(expected = "zero bytes")]
fn a_zero_message_size_is_refused_where_it_is_given() {
    let _ = RecvBuffers::for_connections(4).message_size(0);
}
