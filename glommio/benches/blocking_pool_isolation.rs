//! How long a `spawn_blocking` waits when filesystem work is queued ahead of it.
//!
//! `rename`, `remove`, `create_dir` and `truncate` share the blocking thread
//! pool with `spawn_blocking`, and the pool is one thread by default. A user's
//! blocking closure therefore queues behind whatever filesystem work the
//! runtime happens to be doing, on the same channel.
//!
//! That is only visible when the filesystem work is slow, which for these
//! operations means files with many extents: freeing each one is a separate
//! update to the extent tree and the journal, so a heavily fragmented file
//! takes about a millisecond to unlink where a contiguous one takes five
//! microseconds. Size alone does not do it -- a 256MiB file written in one
//! piece is a single extent.
//!
//! ```sh
//! export GLOMMIO_TEST_POLLIO_ROOTDIR=/some/real/filesystem
//! cargo bench --bench blocking_pool_isolation
//! GLOMMIO_DISABLE_URING_OPS=UNLINKAT cargo bench --bench blocking_pool_isolation
//! ```

use futures::future::join_all;
use glommio::LocalExecutorBuilder;
use std::{
    fs::OpenOptions,
    io::{Seek, SeekFrom, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

const FILES: usize = 30;
const EXTENTS: usize = 2048;
const CHUNK: u64 = 4096;

fn fragmented(path: &std::path::Path) {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .expect("scratch file");
    for i in 0..EXTENTS as u64 {
        file.seek(SeekFrom::Start(i * CHUNK * 4)).unwrap();
        file.write_all(&[b'x'; CHUNK as usize]).unwrap();
    }
    file.sync_all().unwrap();
}

fn main() {
    let root = match std::env::var("GLOMMIO_TEST_POLLIO_ROOTDIR") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            eprintln!("set GLOMMIO_TEST_POLLIO_ROOTDIR to a real filesystem");
            std::process::exit(1);
        }
    };
    let dir = root.join(format!("glommio-isolation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let on_ring = std::env::var("GLOMMIO_DISABLE_URING_OPS")
        .unwrap_or_default()
        .is_empty();
    println!(
        "removes go to {}",
        if on_ring {
            "the ring"
        } else {
            "the blocking pool"
        }
    );

    let dir2 = dir.clone();
    let (fs_elapsed, blocking_latency) = LocalExecutorBuilder::default()
        .spawn(move || async move {
            let paths: Vec<_> = (0..FILES).map(|i| dir2.join(format!("f{i}"))).collect();
            for path in &paths {
                fragmented(path);
            }

            // Issue every remove, then immediately ask the pool to run a
            // trivial closure. On the pool path that closure is behind all of
            // them in one channel, served by one thread.
            // The removes have to be in the queue before the closure is, or
            // this measures the opposite: `spawn_local` schedules a task that
            // the executor may run before the main future gets back to polling
            // them, which puts the closure at the front.
            let started = Instant::now();
            let remover = glommio::spawn_local(async move {
                join_all(paths.iter().map(glommio::io::remove))
                    .await
                    .into_iter()
                    .for_each(|r| r.unwrap());
            });
            glommio::executor().yield_task_queue_now().await;

            let asked_at = Instant::now();
            glommio::executor().spawn_blocking(|| {}).await;
            let latency = asked_at.elapsed();

            remover.await;
            (started.elapsed(), latency)
        })
        .unwrap()
        .join()
        .unwrap();

    let _ = std::fs::remove_dir_all(&dir);
    println!("  {FILES} fragmented removes took {fs_elapsed:?}");
    println!("  the spawn_blocking beside them took {blocking_latency:?}");
}
