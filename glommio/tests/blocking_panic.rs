// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! A blocking closure that panics must not take anything else with it.
//!
//! See <https://github.com/glommio/glommio/issues/37>.

use glommio::LocalExecutor;
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Runs `body` on its own thread and fails if it has not finished in time.
///
/// A hang is the failure being tested for, so it has to become an assertion
/// rather than a test binary that never returns. Eight panics before the
/// healthy call is more than the pool has workers, so a pool that lost one per
/// panic would have none left.
fn within<F: FnOnce() + Send + 'static>(limit: Duration, body: F) {
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        body();
        let _ = tx.send(());
    });
    rx.recv_timeout(limit)
        .expect("the blocking pool never answered");
    worker.join().unwrap();
}

#[test]
fn a_panicking_closure_wakes_its_caller() {
    within(Duration::from_secs(10), || {
        let outcome = std::panic::catch_unwind(|| {
            LocalExecutor::default().run(async {
                glommio::executor().spawn_blocking(|| panic!("boom")).await;
            });
        });
        assert!(
            outcome.is_err(),
            "the caller resumed without seeing the panic"
        );
    });
}

/// The caller sees the payload its own closure panicked with, resumed as
/// `std::thread::JoinHandle::join` does, rather than an internal error that
/// would say nothing about their code.
#[test]
fn the_caller_sees_the_panic_the_closure_raised() {
    within(Duration::from_secs(10), || {
        let outcome = std::panic::catch_unwind(|| {
            LocalExecutor::default().run(async {
                glommio::executor()
                    .spawn_blocking(|| panic!("a message only this closure could raise"))
                    .await;
            });
        });
        let payload = outcome.expect_err("the caller resumed without a panic");
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        assert!(
            message.contains("a message only this closure could raise"),
            "the caller saw {message:?} rather than the closure's own panic"
        );
    });
}

#[test]
fn a_panic_does_not_cost_the_pool_a_worker() {
    within(Duration::from_secs(20), || {
        LocalExecutor::default().run(async {
            for _ in 0..8 {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    glommio::executor().spawn_blocking(|| panic!("boom"))
                }));
            }
            for _ in 0..8 {
                let task = glommio::spawn_local(async {
                    let _ = std::panic::AssertUnwindSafe(
                        glommio::executor().spawn_blocking(|| panic!("boom")),
                    );
                });
                task.detach();
            }
            glommio::timer::sleep(Duration::from_millis(200)).await;

            let started = Instant::now();
            let answer = glommio::executor().spawn_blocking(|| 42u32).await;
            assert_eq!(answer, 42, "the pool stopped answering after panics");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "healthy work took {:?} after earlier panics",
                started.elapsed()
            );
        });
    });
}

/// The caller allocates a `MaybeUninit<R>` for the closure to fill. A closure
/// that unwinds never fills it, so a caller that resumed and read it would be
/// reading memory that was never written, which is undefined rather than
/// merely wrong.
#[test]
fn a_panicking_closure_does_not_yield_uninitialised_memory() {
    within(Duration::from_secs(10), || {
        let outcome = std::panic::catch_unwind(|| {
            LocalExecutor::default().run(async {
                let value: String = glommio::executor()
                    .spawn_blocking(|| -> String { panic!("boom") })
                    .await;
                std::hint::black_box(value.len());
            });
        });
        assert!(outcome.is_err(), "the caller read an uninitialised String");
    });
}
