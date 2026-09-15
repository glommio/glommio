//! Completed output destructors must unwind without aborting the process.

use std::panic::{catch_unwind, AssertUnwindSafe};

use glommio::{LocalExecutorBuilder, PoolPlacement};

struct PanicOnDrop;

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        panic!("intentional output destructor panic");
    }
}

#[test]
fn completed_output_destructor_panic_is_catchable() {
    let ex = LocalExecutorBuilder::default()
        .io_memory(0)
        .blocking_thread_pool_placement(PoolPlacement::Unbound(1))
        .make()
        .expect("failed to create test executor");
    ex.run(async {
        let handle = glommio::spawn_local(async { PanicOnDrop }).detach();
        let outcome = catch_unwind(AssertUnwindSafe(|| drop(handle)));
        let panic = outcome.expect_err("output destructor did not panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"intentional output destructor panic"),
            "caught an unrelated panic",
        );
    });
}
