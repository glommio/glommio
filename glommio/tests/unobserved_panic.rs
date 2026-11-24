//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! Choosing what happens to a blocking panic nobody is waiting for.

use glommio::{LocalExecutorBuilder, PoolPlacement, UnobservedPanic};
use std::{
    os::unix::process::ExitStatusExt,
    process::{Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

/// Which side lets go of the job's slot last, and so which thread runs the
/// policy.
#[derive(Clone, Copy)]
enum LastToLetGo {
    /// The future is dropped while the closure is still running, so the pool
    /// worker releases the slot.
    Worker,
    /// The closure has already panicked when the future is dropped, so the
    /// executor releases the slot.
    Executor,
}

/// Submits a blocking job, abandons it, and returns once the policy has run.
///
/// Every step waits on a signal rather than a sleep. The pool has one worker,
/// which serves jobs in order and releases a job's slot before taking the
/// next, so a job queued behind the abandoned one finishing means the
/// abandoned one is done with. Returns the executor's thread.
fn abandon_a_panicking_job(policy: UnobservedPanic, last: LastToLetGo) -> ThreadId {
    LocalExecutorBuilder::default()
        .blocking_thread_pool_placement(PoolPlacement::Unbound(1))
        .unobserved_panic(policy)
        .spawn(move || async move {
            let (release, gate) = mpsc::channel::<()>();
            let job = glommio::executor().spawn_blocking(move || {
                let _ = gate.recv();
                panic!("nobody is waiting");
            });
            let mut job = Box::pin(job);
            assert!(
                futures_lite::future::poll_once(&mut job).await.is_none(),
                "the job is held at its gate"
            );
            let behind_the_job = || glommio::executor().spawn_blocking(|| ());
            match last {
                LastToLetGo::Worker => {
                    drop(job);
                    release.send(()).unwrap();
                }
                LastToLetGo::Executor => {
                    release.send(()).unwrap();
                    behind_the_job().await;
                    drop(job);
                }
            }
            behind_the_job().await;
            thread::current().id()
        })
        .unwrap()
        .join()
        .unwrap()
}

/// Each payload's message, and the thread the handler ran on.
type Seen = Arc<Mutex<Vec<(String, ThreadId)>>>;

/// Records each payload the handler is given, and the thread it ran on.
fn recording_handler() -> (UnobservedPanic, Seen) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let handler = UnobservedPanic::Handler(Arc::new(move |payload| {
        let text = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .unwrap_or_default();
        log.lock().unwrap().push((text, thread::current().id()));
    }));
    (handler, seen)
}

#[test]
fn a_handler_receives_the_payload_on_the_worker() {
    let (handler, seen) = recording_handler();
    let executor = abandon_a_panicking_job(handler, LastToLetGo::Worker);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "the handler ran once");
    assert_eq!(seen[0].0, "nobody is waiting");
    assert_ne!(seen[0].1, executor, "the worker released the slot");
}

#[test]
fn a_handler_runs_on_the_executor_when_the_job_finished_first() {
    let (handler, seen) = recording_handler();
    let executor = abandon_a_panicking_job(handler, LastToLetGo::Executor);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "the handler ran once");
    assert_eq!(seen[0].0, "nobody is waiting");
    assert_eq!(seen[0].1, executor, "the executor released the slot");
}

#[test]
fn ignore_is_the_default_and_lets_the_process_continue() {
    abandon_a_panicking_job(UnobservedPanic::Ignore, LastToLetGo::Worker);
    abandon_a_panicking_job(UnobservedPanic::Ignore, LastToLetGo::Executor);
}

#[test]
fn a_collected_panic_is_not_reported_as_unobserved() {
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    let policy = UnobservedPanic::Handler(Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    }));

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        LocalExecutorBuilder::default()
            .unobserved_panic(policy)
            .spawn(|| async {
                // Awaited, so the panic is resumed here and not unobserved.
                glommio::executor()
                    .spawn_blocking(|| panic!("awaited"))
                    .await
            })
            .unwrap()
            .join()
            .unwrap()
    }));

    assert!(outcome.is_err(), "the caller saw its own panic");
    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "a collected panic must not reach the unobserved handler"
    );
}

/// Set in a child that should take the path under test and abort.
const CHILD: &str = "GLOMMIO_TEST_UNOBSERVED_ABORT_CHILD";

/// Re-runs one test of this binary as a child with [`CHILD`] set, and returns
/// how it ended.
///
/// An abort cannot be asserted from inside the process it ends, so the parent
/// checks how the child died rather than what it printed. Bounded, since a
/// child that fails to abort may instead hang on a pool that lost its worker.
fn run_child(test: &str) -> (ExitStatus, String) {
    let mut child = Command::new(std::env::current_exe().expect("the test binary is re-runnable"))
        .args([test, "--exact", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to re-run the test binary");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("the child neither aborted nor exited within 30s");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    (status, stderr)
}

fn assert_child_aborts(test: &str) {
    let (status, stderr) = run_child(test);
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "expected the child to abort, it exited {status:?}\nstderr:\n{stderr}"
    );
}

fn panicking_handler() -> UnobservedPanic {
    UnobservedPanic::Handler(Arc::new(|_| panic!("the handler itself panicked")))
}

#[test]
fn abort_ends_the_process() {
    if std::env::var_os(CHILD).is_some() {
        abandon_a_panicking_job(UnobservedPanic::Abort, LastToLetGo::Worker);
        unreachable!("Abort should have ended this process");
    }
    assert_child_aborts("abort_ends_the_process");
}

/// A panicking handler on the worker would otherwise end the worker's loop and
/// leave the shard's blocking pool without a thread.
#[test]
fn a_panicking_handler_on_the_worker_aborts() {
    if std::env::var_os(CHILD).is_some() {
        abandon_a_panicking_job(panicking_handler(), LastToLetGo::Worker);
        unreachable!("the panicking handler should have ended this process");
    }
    assert_child_aborts("a_panicking_handler_on_the_worker_aborts");
}

/// A panicking handler on the executor would otherwise unwind into whichever
/// task dropped the job.
#[test]
fn a_panicking_handler_on_the_executor_aborts() {
    if std::env::var_os(CHILD).is_some() {
        abandon_a_panicking_job(panicking_handler(), LastToLetGo::Executor);
        unreachable!("the panicking handler should have ended this process");
    }
    assert_child_aborts("a_panicking_handler_on_the_executor_aborts");
}
