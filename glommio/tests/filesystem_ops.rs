// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! `rename`, `remove` and `Directory::create` behave the same whichever way
//! they are submitted.
//!
//! Each runs on the ring where the kernel has the opcode and on the blocking
//! pool where it does not, so the two have to agree on results and on errno.
//! `GLOMMIO_DISABLE_URING_OPS` forces the second path on a kernel that has
//! everything, which is the only way to reach it on a modern machine.

use glommio::{
    io::{Directory, DmaFile},
    LocalExecutorBuilder,
};
use std::{io::ErrorKind, path::PathBuf};

/// The errno a failed operation carried, whichever path produced it.
fn kind_of<T>(err: glommio::GlommioError<T>) -> ErrorKind {
    std::io::Error::from(err).kind()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glommio-fsops-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

fn run<F>(body: F)
where
    F: FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static,
{
    LocalExecutorBuilder::default()
        .spawn(move || async move { body().await })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn rename_moves_a_file_and_reports_a_missing_source() {
    let dir = scratch("rename");
    let (from, to) = (dir.join("a"), dir.join("b"));
    std::fs::write(&from, b"payload").unwrap();
    let missing = dir.join("absent");

    run(move || {
        Box::pin(async move {
            glommio::io::rename(&from, &to).await.expect("rename");
            assert!(!from.exists(), "the old name is gone");
            assert_eq!(std::fs::read(&to).unwrap(), b"payload", "contents survive");

            let err = glommio::io::rename(&missing, &to)
                .await
                .expect_err("renaming something absent must fail");
            assert_eq!(kind_of(err), ErrorKind::NotFound, "errno survives the trip");
        })
    });
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn remove_deletes_a_file_and_reports_a_missing_one() {
    let dir = scratch("remove");
    let victim = dir.join("victim");
    std::fs::write(&victim, b"x").unwrap();
    let missing = dir.join("absent");

    run(move || {
        Box::pin(async move {
            glommio::io::remove(&victim).await.expect("remove");
            assert!(!victim.exists(), "the file is gone");

            let err = glommio::io::remove(&missing)
                .await
                .expect_err("removing something absent must fail");
            assert_eq!(kind_of(err), ErrorKind::NotFound, "errno survives the trip");
        })
    });
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn create_dir_makes_one_and_reports_a_missing_parent() {
    let dir = scratch("mkdir");
    let fresh = dir.join("fresh");
    let orphan = dir.join("absent-parent").join("child");

    run(move || {
        Box::pin(async move {
            Directory::create(&fresh).await.expect("create");
            assert!(fresh.is_dir(), "the directory exists");

            // Creating it again is deliberately not an error: `create` maps
            // `AlreadyExists` to success, the way the standard library does.
            Directory::create(&fresh)
                .await
                .expect("creating an existing directory is not an error");

            let err = Directory::create(&orphan)
                .await
                .expect_err("creating under a missing parent must fail");
            assert_eq!(kind_of(err), ErrorKind::NotFound, "errno survives the trip");
        })
    });
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn truncate_shortens_a_file() {
    let dir = scratch("truncate");
    let path = dir.join("file");

    run(move || {
        Box::pin(async move {
            let file = DmaFile::create(&path).await.expect("create");
            let buf = file.alloc_dma_buffer(4096);
            file.write_at(buf, 0).await.expect("write");
            file.fdatasync().await.expect("sync");

            file.truncate(512).await.expect("truncate");
            assert_eq!(file.file_size().await.expect("size"), 512, "file shortened");

            file.close().await.expect("close");
        })
    });
    let _ = std::fs::remove_dir_all(dir);
}
