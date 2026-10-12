//! Notification coalescing and publication around a draining pass.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
    task::{Wake, Waker},
    time::Duration,
};

use super::new_sleep_notifier;

#[derive(Default)]
struct Count(AtomicUsize);

impl Wake for Count {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn notification_count(fd: std::os::fd::RawFd) -> u64 {
    let mut readiness = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut readiness, 1, 0) }, 1);
    let mut value = 0u64;
    assert_eq!(
        unsafe { libc::read(fd, (&mut value as *mut u64).cast(), 8) },
        8
    );
    value
}

#[test]
fn forced_notifications_coalesce_until_the_next_drain() {
    let notifier = new_sleep_notifier().unwrap();
    let count = Arc::new(Count::default());
    let waker = Waker::from(count.clone());
    for _ in 0..32 {
        notifier.queue_waker(waker.clone(), true);
    }
    assert_eq!(notification_count(notifier.eventfd_fd()), 1);
    assert_eq!(notifier.process_foreign_wakes(), 32);
    assert_eq!(count.0.load(Ordering::Relaxed), 32);
    assert_eq!(notifier.process_foreign_wakes(), 0);
    notifier.queue_waker(waker, true);
    assert_eq!(notification_count(notifier.eventfd_fd()), 1);
    assert_eq!(notifier.process_foreign_wakes(), 1);
}

#[test]
fn forced_notification_after_an_ordinary_wake_writes_the_eventfd() {
    let notifier = new_sleep_notifier().unwrap();
    let waker = Waker::from(Arc::new(Count::default()));
    notifier.queue_waker(waker.clone(), false);
    notifier.queue_waker(waker, true);
    assert_eq!(notification_count(notifier.eventfd_fd()), 1);
    assert_eq!(notifier.process_foreign_wakes(), 2);
}

#[test]
fn sleeping_owner_is_notified_for_a_non_forced_batch() {
    let notifier = new_sleep_notifier().unwrap();
    let count = Arc::new(Count::default());
    let waker = Waker::from(count.clone());
    notifier.prepare_to_sleep();
    for _ in 0..32 {
        notifier.queue_waker(waker.clone(), false);
    }
    assert_eq!(notification_count(notifier.eventfd_fd()), 1);
    assert_eq!(notifier.process_foreign_wakes(), 32);
    assert_eq!(count.0.load(Ordering::Relaxed), 32);
    notifier.wake_up();
}

#[test]
fn accepted_wake_is_visible_before_the_submitter_returns() {
    let notifier = new_sleep_notifier().unwrap();
    let count = Arc::new(Count::default());
    let (queued_tx, queued_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let remote = notifier.clone();
    let remote_count = count.clone();
    let worker = std::thread::spawn(move || {
        super::test_support::pause_next_foreign_wake(queued_tx, resume_rx);
        remote.queue_waker(Waker::from(remote_count), true);
    });
    queued_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let drained = notifier.process_foreign_wakes();
    resume_tx.send(()).unwrap();
    worker.join().unwrap();
    assert_eq!(drained, 1);
    assert_eq!(count.0.load(Ordering::Relaxed), 1);
}
