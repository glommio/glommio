//! Test-only hooks: synchronization for foreign wake submission, and
//! injected ring submission failures.

use std::{
    cell::{Cell, RefCell},
    io,
    sync::mpsc::{Receiver, Sender},
    time::Duration,
};

struct ForeignWakePause {
    queued: Sender<()>,
    resume: Receiver<()>,
}

thread_local! {
    static FOREIGN_WAKE_PAUSE: RefCell<Option<ForeignWakePause>> = const { RefCell::new(None) };
    static SUBMIT_FAILURE: Cell<Option<(&'static str, i32)>> = const { Cell::new(None) };
}

/// Pauses this thread's next submission after enqueueing and releasing the queue lock.
pub(crate) fn pause_next_foreign_wake(queued: Sender<()>, resume: Receiver<()>) {
    FOREIGN_WAKE_PAUSE.with(|pause| {
        assert!(pause
            .borrow_mut()
            .replace(ForeignWakePause { queued, resume })
            .is_none());
    });
}

/// Lets the owner consume a notification before its submitting thread returns.
pub(crate) fn after_foreign_wake_queued() {
    let pause = FOREIGN_WAKE_PAUSE.with(|pause| pause.borrow_mut().take());
    if let Some(pause) = pause {
        pause
            .queued
            .send(())
            .expect("owner stopped waiting for the foreign notification");
        pause
            .resume
            .recv_timeout(Duration::from_secs(10))
            .expect("owner did not resume the foreign submission");
    }
}

/// Fails this thread's next submission to the ring named `ring` with `errno`.
pub(crate) fn fail_next_submit(ring: &'static str, errno: i32) {
    assert!(SUBMIT_FAILURE
        .with(|f| f.replace(Some((ring, errno))))
        .is_none());
}

/// Takes the failure armed for the ring named `ring`, if any.
pub(crate) fn take_submit_failure(ring: &str) -> Option<io::Error> {
    SUBMIT_FAILURE.with(|f| match f.get() {
        Some((name, errno)) if name == ring => {
            f.set(None);
            Some(io::Error::from_raw_os_error(errno))
        }
        _ => None,
    })
}
