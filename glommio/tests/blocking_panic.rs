// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! A blocking closure that panics must not take anything else with it.
//!
//! See <https://github.com/glommio/glommio/issues/37>.

use futures::FutureExt;
use glommio::{LocalExecutor, LocalExecutorBuilder, Placement, PoolPlacement};
use std::{
    collections::HashSet,
    panic::AssertUnwindSafe,
    sync::{mpsc, Arc, Barrier, Mutex},
    thread::{self, ThreadId},
    time::Duration,
};

/// A payload no other panic in the process can produce.
///
/// Matching on it proves the caller resumed the closure's own panic rather
/// than something glommio substituted, which a message comparison cannot:
/// any `&str` payload could have come from somewhere else.
#[derive(Debug)]
struct SyntheticPanic;

impl SyntheticPanic {
    fn trigger() -> ! {
        std::panic::panic_any(SyntheticPanic)
    }
}

/// Runs `body` on its own thread and fails if it has not finished in time.
///
/// A hang is the failure being tested for, so it has to become an assertion
/// rather than a test binary that never returns.
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
fn the_caller_resumes_the_panic_the_closure_raised() {
    within(Duration::from_secs(10), || {
        let outcome = std::panic::catch_unwind(|| {
            LocalExecutor::default().run(async {
                let _: () = glommio::executor()
                    .spawn_blocking(SyntheticPanic::trigger)
                    .await;
            });
        });
        let payload = outcome.expect_err("the caller resumed without seeing the panic");
        assert!(
            payload.downcast_ref::<SyntheticPanic>().is_some(),
            "the caller saw some other panic: {payload:?}"
        );
    });
}

/// Every worker in the pool panics, and then every worker is used again.
///
/// Comparing the two sets of thread ids is what makes this bite. Asserting
/// only that later work completes would pass with a single surviving worker,
/// which is the failure this is about: the pool loses a thread per panic and
/// degrades quietly until it has none.
///
/// The barrier is what forces one job onto each worker rather than all of them
/// onto whichever is free first, so the pool placement is pinned to a known
/// size instead of inferred.
#[test]
fn every_worker_survives_panicking() {
    const WORKERS: usize = 4;

    within(Duration::from_secs(30), || {
        LocalExecutorBuilder::new(Placement::Unbound)
            .blocking_thread_pool_placement(PoolPlacement::Unbound(WORKERS))
            .spawn(|| async {
                let panicked = ids_from_every_worker(WORKERS, true).await;
                assert_eq!(
                    panicked.len(),
                    WORKERS,
                    "expected every worker to take a panicking job"
                );

                let survived = ids_from_every_worker(WORKERS, false).await;
                assert_eq!(
                    survived, panicked,
                    "the pool is not the same set of threads it was before the panics"
                );
            })
            .unwrap()
            .join()
            .unwrap();
    });
}

/// Puts one job on each of `workers` threads, returning the ids that ran them.
///
/// The barrier holds every job until all of them are running, so no worker can
/// take two. When `panicking`, each job records itself and then unwinds.
async fn ids_from_every_worker(workers: usize, panicking: bool) -> HashSet<ThreadId> {
    let barrier = Arc::new(Barrier::new(workers));
    let seen = Arc::new(Mutex::new(HashSet::new()));

    let jobs: Vec<_> = (0..workers)
        .map(|_| {
            let barrier = barrier.clone();
            let seen = seen.clone();
            glommio::spawn_local(async move {
                // The panic surfaces when the future is polled, so it has to
                // be caught around the await rather than around a closure.
                let _ = AssertUnwindSafe(glommio::executor().spawn_blocking(move || {
                    barrier.wait();
                    seen.lock().unwrap().insert(thread::current().id());
                    if panicking {
                        SyntheticPanic::trigger();
                    }
                }))
                .catch_unwind()
                .await;
            })
        })
        .collect();

    for job in jobs {
        job.await;
    }
    let ids = seen.lock().unwrap().clone();
    ids
}
