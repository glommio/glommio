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
use loom::{
    sync::atomic::{AtomicBool as LoomBool, AtomicUsize as LoomUsize},
    thread,
};
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

/// P4: a producer that publishes outside the waker queue and then notifies,
/// as a shared channel does, cannot be missed by an owner going to sleep.
///
/// The channel's push is a store-release with nothing after it but the
/// notify. Unlike `queue_waker`, nothing in between orders that store
/// before the notify's look at `should_notify`.
#[test]
fn a_notify_after_a_plain_publish_is_not_lost() {
    loom::model(|| {
        let notifier = new_sleep_notifier().unwrap();
        let published = Arc::new(super::sync::AtomicBool::new(false));

        let mut producer = Some({
            let notifier = notifier.clone();
            let published = published.clone();
            thread::spawn(move || {
                published.store(true, Ordering::Release);
                notifier.notify(false);
            })
        });

        notifier
            .sleep_unless_woken(
                || published.load(Ordering::Acquire) as usize,
                || {
                    producer.take().unwrap().join().unwrap();
                    assert!(
                        notifier.signals() > 0,
                        "the owner slept over a published value with nothing to wake it"
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

/// P5: an owner that comes back awake, whether it found a waker or slept,
/// has notification off, so an ordinary wake after that writes nothing.
#[test]
fn an_awake_owner_is_not_signalled_by_an_ordinary_wake() {
    loom::model(|| {
        let notifier = new_sleep_notifier().unwrap();
        let (w, _) = waker();

        let mut producer = Some({
            let notifier = notifier.clone();
            thread::spawn(move || notifier.queue_waker(w, false))
        });

        notifier
            .sleep_unless_woken(
                || notifier.process_foreign_wakes(),
                || {
                    producer.take().unwrap().join().unwrap();
                    Ok(())
                },
            )
            .unwrap();
        if let Some(producer) = producer {
            producer.join().unwrap();
        }

        let before = notifier.signals();
        notifier.notify(false);
        assert_eq!(
            notifier.signals(),
            before,
            "an ordinary wake signalled an owner that was awake"
        );
    });
}

/// One half of a shared channel connecting, as `connect()` does: publish
/// its executor's id, then look for the peer's. A half that finds its peer
/// notifies the peer's executor; one that does not parks until it does.
///
/// Parking waits for the peer to finish, then checks it was signalled: a
/// real executor stays asleep until something writes its eventfd.
fn connect_half(
    me: usize,
    ids: &[LoomUsize; 2],
    notifiers: &[Arc<super::SleepNotifier>; 2],
    parked: &[LoomBool; 2],
    done: &[LoomBool; 2],
) {
    let peer = 1 - me;
    let peer_connected = || ids[peer].load(Ordering::Acquire) != 0;
    ids[me].store(notifiers[me].id(), Ordering::Release);
    let found = peer_connected()
        || notifiers[me]
            .sleep_unless_woken(
                || peer_connected() as usize,
                || {
                    parked[me].store(true, Ordering::SeqCst);
                    while !done[peer].load(Ordering::SeqCst) {
                        assert!(
                            !parked[peer].load(Ordering::SeqCst),
                            "both halves parked, each waiting for the other"
                        );
                        thread::yield_now();
                    }
                    assert!(
                        notifiers[me].signals() > 0,
                        "a half parked with its peer connected and nothing to wake it"
                    );
                    Ok(())
                },
            )
            .unwrap()
            > 0;
    if found {
        notifiers[peer].notify(false);
    }
    done[me].store(true, Ordering::SeqCst);
}

/// P6: two halves of a shared channel connecting at once never leave one
/// of them parked with nothing to wake it.
#[test]
fn a_connect_is_not_left_waiting_for_a_peer_that_connected() {
    loom::model(|| {
        let notifiers = Arc::new([new_sleep_notifier().unwrap(), new_sleep_notifier().unwrap()]);
        let ids = Arc::new([LoomUsize::new(0), LoomUsize::new(0)]);
        let parked = Arc::new([LoomBool::new(false), LoomBool::new(false)]);
        let done = Arc::new([LoomBool::new(false), LoomBool::new(false)]);

        let other = {
            let (notifiers, ids, parked, done) =
                (notifiers.clone(), ids.clone(), parked.clone(), done.clone());
            thread::spawn(move || connect_half(1, &ids, &notifiers, &parked, &done))
        };
        connect_half(0, &ids, &notifiers, &parked, &done);
        other.join().unwrap();
    });
}
