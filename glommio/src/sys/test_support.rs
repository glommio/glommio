//! Test-only synchronization for foreign wake submission.

use std::{
    cell::RefCell,
    sync::mpsc::{Receiver, Sender},
    time::Duration,
};

struct ForeignWakePause {
    queued: Sender<()>,
    resume: Receiver<()>,
}

thread_local! {
    static FOREIGN_WAKE_PAUSE: RefCell<Option<ForeignWakePause>> = const { RefCell::new(None) };
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
