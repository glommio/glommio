//! Its own test binary: it lowers the process's descriptor limit.

use glommio::{LocalExecutorPoolBuilder, PoolPlacement};
use std::{fs::File, os::unix::io::AsRawFd, sync::mpsc, time::Duration};

fn is_open(fd: i32) -> bool {
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

/// Leaves exactly one descriptor free under a lowered limit, so a pool's
/// first shard can create its notifier and the second cannot. Returns the
/// limit to restore.
fn leave_one_free_descriptor(fillers: &mut Vec<File>) -> libc::rlimit {
    loop {
        let file = File::open("/dev/null").unwrap();
        let next_free = !is_open(file.as_raw_fd() + 1);
        fillers.push(file);
        if next_free {
            break;
        }
    }
    let mut old = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut old) }, 0);
    let one_free = libc::rlimit {
        rlim_cur: (fillers.last().unwrap().as_raw_fd() + 2) as libc::rlim_t,
        rlim_max: old.rlim_max,
    };
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &one_free) },
        0
    );
    old
}

/// Guards against a pool whose second shard fails to start leaving the first
/// waiting forever for it, and the caller with it.
#[test]
fn a_shard_that_fails_to_start_does_not_strand_the_others() {
    let mut fillers = Vec::new();
    let old = leave_one_free_descriptor(&mut fillers);
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started =
            LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(2)).on_all_shards(|| async {});
        done_tx
            .send(started.map(|_| ()).map_err(|e| e.to_string()))
            .unwrap();
    });
    let result = done_rx.recv_timeout(Duration::from_secs(10));
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &old) }, 0);
    drop(fillers);
    let started = result.expect("the pool never returned");
    assert!(
        started.is_err(),
        "the second shard was expected to fail: {started:?}"
    );
}
