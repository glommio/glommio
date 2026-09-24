// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! A blocking job finishing after its executor is gone must not panic.
//!
//! The worker finds a closed response channel, which is how an executor's life
//! ends rather than a failure. Panicking there is stderr noise under
//! unwinding and a dead process under `panic = "abort"`.
//!
//! This file is its own test binary because the panic hook is process wide.

use glommio::LocalExecutorBuilder;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[test]
fn a_job_outliving_its_executor_does_not_panic() {
    let panics = Arc::new(AtomicUsize::new(0));
    let counter = panics.clone();
    std::panic::set_hook(Box::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    }));

    // Set as the closure returns. Waiting on this rather than on a duration is
    // what keeps the test honest: the worker attempts the send the moment the
    // closure is done, so a test that only slept could finish before the job
    // did and pass without the condition ever arising. It used to.
    let finished = Arc::new(AtomicBool::new(false));
    let signal = finished.clone();

    LocalExecutorBuilder::default()
        .spawn(move || async move {
            // Polled once so the job is submitted, then abandoned, which is
            // what `select!` does when another branch wins. The closure keeps
            // running and the executor is free to exit first.
            let job = glommio::executor().spawn_blocking(move || {
                std::thread::sleep(Duration::from_millis(200));
                signal.store(true, Ordering::SeqCst);
            });
            let mut job = Box::pin(job);
            assert!(
                futures_lite::future::poll_once(&mut job).await.is_none(),
                "the job should still be running"
            );
            drop(job);
        })
        .unwrap()
        .join()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while !finished.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the blocking job never finished, so the failed send never happened and this test \
             proved nothing"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // Slack for the send that follows the closure's return, not for the job.
    std::thread::sleep(Duration::from_millis(100));
    let seen = panics.load(Ordering::SeqCst);
    let _ = std::panic::take_hook();

    assert_eq!(
        seen, 0,
        "a worker panicked when its executor had already gone"
    );
}
