//! Shared harness pieces for the benchmarks.

use criterion::async_executor::AsyncExecutor;
use glommio::LocalExecutor;
use std::future::Future;

/// Runs benchmark futures on one long-lived executor.
///
/// Building an executor costs microseconds because of the io_uring setup, so a
/// fresh one per iteration would swamp anything measured in nanoseconds.
/// `LocalExecutor::run` takes `&self`, so one executor serves every iteration.
#[derive(Default)]
pub struct Glommio(pub LocalExecutor);

impl AsyncExecutor for &Glommio {
    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        self.0.run(future)
    }
}
