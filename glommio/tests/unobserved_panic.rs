//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! Choosing what happens to a blocking panic nobody is waiting for.

use glommio::{LocalExecutorBuilder, UnobservedPanic};
use std::{
    os::unix::process::ExitStatusExt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

/// Polls a blocking job once so it is submitted, then abandons it. The closure
/// keeps running and panics with nothing left to resume into.
fn abandon_a_panicking_job(policy: UnobservedPanic) {
    LocalExecutorBuilder::default()
        .unobserved_panic(policy)
        .spawn(|| async {
            let job = glommio::executor().spawn_blocking(|| {
                std::thread::sleep(Duration::from_millis(50));
                panic!("nobody is waiting");
            });
            let mut job = Box::pin(job);
            assert!(
                futures_lite::future::poll_once(&mut job).await.is_none(),
                "the job should still be running"
            );
            drop(job);
            glommio::timer::sleep(Duration::from_millis(400)).await;
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn a_handler_receives_the_payload() {
    let seen = Arc::new(AtomicUsize::new(0));
    let message = Arc::new(Mutex::new(String::new()));
    let (count, text) = (seen.clone(), message.clone());

    abandon_a_panicking_job(UnobservedPanic::Handler(Arc::new(move |payload| {
        count.fetch_add(1, Ordering::SeqCst);
        if let Some(s) = payload.downcast_ref::<&str>() {
            *text.lock().unwrap() = (*s).to_string();
        }
    })));

    assert_eq!(seen.load(Ordering::SeqCst), 1, "the handler ran once");
    assert_eq!(*message.lock().unwrap(), "nobody is waiting");
}

#[test]
fn ignore_is_the_default_and_lets_the_process_continue() {
    abandon_a_panicking_job(UnobservedPanic::Ignore);
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

/// `Abort` ends the process, so it cannot be asserted from inside the process
/// it ends. The child re-runs this same test with the marker set and takes the
/// abort; the parent checks how it died rather than what it printed.
#[test]
fn abort_ends_the_process() {
    const MARKER: &str = "GLOMMIO_TEST_UNOBSERVED_ABORT_CHILD";

    if std::env::var_os(MARKER).is_some() {
        abandon_a_panicking_job(UnobservedPanic::Abort);
        unreachable!("Abort should have ended this process");
    }

    let child = std::process::Command::new(
        std::env::current_exe().expect("the test binary has to be re-runnable"),
    )
    .args(["abort_ends_the_process", "--exact", "--test-threads=1"])
    .env(MARKER, "1")
    .output()
    .expect("failed to re-run the test binary");

    assert_eq!(
        child.status.signal(),
        Some(libc::SIGABRT),
        "expected the child to abort, it exited {:?}\nstderr:\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
}
