//! Test-only hooks: synchronization for foreign wake submission, injected
//! ring submission failures, and a record of file syncs and truncates.

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
    static FILE_OPS: RefCell<Option<Vec<&'static str>>> = const { RefCell::new(None) };
}

/// Starts recording, in order, the syncs and truncates this thread issues.
pub(crate) fn record_file_ops() {
    FILE_OPS.with(|ops| assert!(ops.replace(Some(Vec::new())).is_none()));
}

/// Notes a sync or truncate, if this thread is recording them.
pub(crate) fn note_file_op(op: &'static str) {
    FILE_OPS.with(|ops| {
        if let Some(ops) = ops.borrow_mut().as_mut() {
            ops.push(op);
        }
    });
}

/// Stops recording and returns what was recorded.
pub(crate) fn recorded_file_ops() -> Vec<&'static str> {
    FILE_OPS
        .with(|ops| ops.take())
        .expect("file ops were not being recorded")
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
