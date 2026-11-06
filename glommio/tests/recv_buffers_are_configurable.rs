//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! The receive buffer knobs, driven from outside the crate: both builders
//! accept them and an executor built with them runs.

use glommio::{LocalExecutorBuilder, LocalExecutorPoolBuilder, Placement, PoolPlacement};

#[test]
fn a_single_executor_takes_recv_buffers() {
    let ex = LocalExecutorBuilder::new(Placement::Unbound)
        .recv_buffers(256, 16 * 1024)
        .make()
        .unwrap();
    assert_eq!(ex.run(async { 1 + 1 }), 2);
}

#[test]
fn a_pool_takes_recv_buffers() {
    let handles = LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(2))
        .recv_buffers(16, 1024)
        .on_all_shards(|| async { 21 * 2 })
        .unwrap();
    for result in handles.join_all() {
        assert_eq!(result.unwrap(), 42);
    }
}

#[test]
fn zero_recv_buffers_is_allowed() {
    let ex = LocalExecutorBuilder::new(Placement::Unbound)
        .recv_buffers(0, 0)
        .make()
        .unwrap();
    ex.run(async {});
}

#[test]
#[should_panic(expected = "above the kernel's limit")]
fn too_many_recv_buffers_is_refused_at_build_time() {
    let _ = LocalExecutorBuilder::new(Placement::Unbound).recv_buffers(32769, 4096);
}
