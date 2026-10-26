//! What `rename`, `remove` and `Directory::create` cost on each path.
//!
//! These three go to io_uring where the kernel has the opcode and to the
//! blocking pool where it does not. Which path a process takes is decided once
//! at startup, so comparing them means running this twice:
//!
//! ```sh
//! cargo bench --bench filesystem_ops
//! GLOMMIO_DISABLE_URING_OPS=RENAMEAT,UNLINKAT,MKDIRAT,FTRUNCATE cargo bench --bench filesystem_ops
//! ```
//!
//! Every operation needs a file or directory that does not exist yet, so the
//! work is set up outside the timed region with `iter_custom` rather than
//! measured along with it.
//!
//! The operations are issued together and waited on together. Awaiting each
//! one before issuing the next leaves a single request in flight, which is not
//! how either path is used: the ring never gets to amortise a submission
//! across a batch, and the pool never gets to use more than one of its
//! threads. Measured that way the two look closer than they are.

mod common;

use common::Glommio;
use criterion::{criterion_group, criterion_main, Criterion};
use futures::future::join_all;
use glommio::io::{Directory, DmaFile};
use std::{path::PathBuf, time::Instant};

/// A directory of its own per benchmark, cleaned up as it goes.
fn scratch(name: &str) -> PathBuf {
    // `temp_dir` is usually tmpfs, where nothing blocks and an unlink costs
    // about 1.5us. The point of moving these to the ring is what happens when
    // they do block, so point this at a real filesystem to measure that.
    let root = match std::env::var("GLOMMIO_TEST_POLLIO_ROOTDIR") {
        Ok(path) => PathBuf::from(path),
        Err(_) => std::env::temp_dir(),
    };
    let dir = root.join(format!("glommio-fsbench-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// Says which path this process took.
///
/// Printed rather than folded into the benchmark name, because criterion
/// compares baselines by name and a name that changes with the path cannot be
/// compared against one that does not.
fn announce_root() {
    println!(
        "root: {}",
        std::env::var("GLOMMIO_TEST_POLLIO_ROOTDIR")
            .unwrap_or_else(|_| std::env::temp_dir().display().to_string())
    );
}

fn announce_path() {
    let forced = !std::env::var("GLOMMIO_DISABLE_URING_OPS")
        .unwrap_or_default()
        .is_empty();
    println!(
        "path: {}",
        if forced {
            "blocking pool (forced with GLOMMIO_DISABLE_URING_OPS)"
        } else {
            "io_uring where the kernel has the opcode"
        }
    );
}

fn filesystem_ops(c: &mut Criterion) {
    let ex = Glommio::default();
    announce_path();
    announce_root();
    let mut group = c.benchmark_group("filesystem ops");

    group.bench_function("remove", |b| {
        let dir = scratch("remove");
        b.to_async(&ex).iter_custom(|iters| {
            let dir = dir.clone();
            async move {
                let paths: Vec<_> = (0..iters).map(|i| dir.join(format!("f{i}"))).collect();
                for path in &paths {
                    std::fs::write(path, b"").unwrap();
                }
                let start = Instant::now();
                join_all(paths.iter().map(glommio::io::remove))
                    .await
                    .into_iter()
                    .for_each(|r| r.unwrap());
                start.elapsed()
            }
        })
    });

    group.bench_function("rename", |b| {
        let dir = scratch("rename");
        b.to_async(&ex).iter_custom(|iters| {
            let dir = dir.clone();
            async move {
                let sources: Vec<_> = (0..iters).map(|i| dir.join(format!("a{i}"))).collect();
                let targets: Vec<_> = (0..iters).map(|i| dir.join(format!("b{i}"))).collect();
                for path in &sources {
                    std::fs::write(path, b"").unwrap();
                }
                let start = Instant::now();
                join_all(
                    sources
                        .iter()
                        .zip(&targets)
                        .map(|(from, to)| glommio::io::rename(from, to)),
                )
                .await
                .into_iter()
                .for_each(|r| r.unwrap());
                let elapsed = start.elapsed();
                for path in &targets {
                    let _ = std::fs::remove_file(path);
                }
                elapsed
            }
        })
    });

    group.bench_function("create_dir", |b| {
        let dir = scratch("mkdir");
        b.to_async(&ex).iter_custom(|iters| {
            let dir = dir.clone();
            async move {
                let paths: Vec<_> = (0..iters).map(|i| dir.join(format!("d{i}"))).collect();
                let start = Instant::now();
                join_all(paths.iter().map(Directory::create))
                    .await
                    .into_iter()
                    .for_each(|r| {
                        r.unwrap();
                    });
                let elapsed = start.elapsed();
                for path in &paths {
                    let _ = std::fs::remove_dir(path);
                }
                elapsed
            }
        })
    });

    group.bench_function("truncate", |b| {
        let dir = scratch("truncate");
        b.to_async(&ex).iter_custom(|iters| {
            let dir = dir.clone();
            async move {
                // A file each, like the other three. Truncating one file
                // repeatedly holds a single inode and cannot be issued as a
                // batch, which makes it a measurement of the sequential case
                // rather than of truncate.
                let mut files = Vec::with_capacity(iters as usize);
                for i in 0..iters {
                    let file = DmaFile::create(dir.join(format!("f{i}"))).await.unwrap();
                    let buf = file.alloc_dma_buffer(4096);
                    file.write_at(buf, 0).await.unwrap();
                    files.push(file);
                }

                let start = Instant::now();
                join_all(files.iter().map(|f| f.truncate(2048)))
                    .await
                    .into_iter()
                    .for_each(|r| r.unwrap());
                let elapsed = start.elapsed();

                for file in files {
                    file.close().await.unwrap();
                }
                elapsed
            }
        })
    });

    group.finish();
}

criterion_group!(benches, filesystem_ops);
criterion_main!(benches);
