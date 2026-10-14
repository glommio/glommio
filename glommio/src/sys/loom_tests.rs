//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! Model checks of the foreign wake protocol, run under loom:
//!
//! ```bash
//! RUSTFLAGS="--cfg loom" cargo test --release -p glommio --lib loom_tests
//! ```
//!
//! An eventfd write is a counter here, and "sleeping" records that the owner
//! decided to sleep. The properties are checked after every thread has joined.

use super::new_sleep_notifier;
use loom::thread;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Wake, Waker},
};

#[derive(Default)]
struct Counts {
    woken: AtomicUsize,
    dropped: AtomicUsize,
}

struct Counted(Arc<Counts>);

impl Wake for Counted {
    fn wake(self: Arc<Self>) {
        self.0.woken.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

fn waker() -> (Waker, Arc<Counts>) {
    let counts = Arc::new(Counts::default());
    (Waker::from(Arc::new(Counted(counts.clone()))), counts)
}

/// P1: an owner that sleeps does so with every waker either woken or an
/// eventfd write pending, so a real owner would wake and drain it.
///
/// Sleeping is modelled as waiting for the producer to finish: a real owner
/// stays asleep until something wakes it, so nothing it does on waking can
/// affect what the producer saw.
#[test]
fn no_wake_is_lost_when_the_owner_sleeps() {
    loom::model(|| {
        let notifier = new_sleep_notifier().unwrap();
        let (w, counts) = waker();

        let mut producer = Some({
            let notifier = notifier.clone();
            thread::spawn(move || notifier.queue_waker(w, false))
        });

        notifier
            .sleep_unless_woken(
                || notifier.process_foreign_wakes(),
                || {
                    producer.take().unwrap().join().unwrap();
                    assert!(
                        counts.woken.load(Ordering::SeqCst) == 1 || notifier.signals() > 0,
                        "the owner slept with a waker queued and nothing to wake it"
                    );
                    Ok(())
                },
            )
            .unwrap();
        if let Some(producer) = producer {
            producer.join().unwrap();
        }
    });
}

/// P2: a forced wake reaches a busy owner, whatever an ordinary wake did first.
#[test]
fn a_forced_wake_gets_through_to_a_busy_owner() {
    loom::model(|| {
        let notifier = new_sleep_notifier().unwrap();
        let (ordinary, _) = waker();
        let (forced, forced_counts) = waker();

        let producer = {
            let notifier = notifier.clone();
            thread::spawn(move || {
                notifier.queue_waker(ordinary, false);
                notifier.queue_waker(forced, true);
            })
        };

        notifier.process_foreign_wakes();
        producer.join().unwrap();

        assert!(
            forced_counts.woken.load(Ordering::SeqCst) == 1 || notifier.signals() > 0,
            "a forced wake was neither delivered nor signalled"
        );
    });
}

/// P3: once closed, nothing is left queued and every waker was woken or
/// dropped, exactly once.
#[test]
fn closing_leaves_nothing_behind() {
    loom::model(|| {
        let notifier = new_sleep_notifier().unwrap();
        let (w, counts) = waker();

        let producer = {
            let notifier = notifier.clone();
            thread::spawn(move || notifier.queue_waker(w, false))
        };

        notifier.close_foreign_wakes();
        producer.join().unwrap();

        assert_eq!(
            notifier.process_foreign_wakes(),
            0,
            "a waker outlived close"
        );
        assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
        assert!(counts.woken.load(Ordering::SeqCst) <= 1);
    });
}
