// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! Choosing what happens to a blocking panic nobody is waiting for.

use glommio::{LocalExecutorBuilder, UnobservedPanic};
use std::{
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
