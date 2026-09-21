//! `record_io_latencies` both ways.
//!
//! The three timestamps an I/O latency is computed from are only taken when a
//! latency collection function is installed, so the flag being off has to mean
//! no readings, and the flag being on has to still produce them.

use glommio::{io::DmaFile, LocalExecutorBuilder, Placement};

/// Writes a file, reads it back, and reports how many I/O latencies landed in
/// the ring's statistics.
///
/// The reading matters: latencies are collected on the read paths, so a
/// write-only workload records nothing however the flag is set. `DmaFile::create`
/// opens write-only, hence the reopen.
fn run_and_count(record: bool) -> usize {
    LocalExecutorBuilder::new(Placement::Unbound)
        .record_io_latencies(record)
        .spawn(|| async move {
            let path = std::env::temp_dir().join(format!(
                "glommio-latency-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let file = DmaFile::create(&path).await.unwrap();
            for i in 0..16 {
                let mut buf = file.alloc_dma_buffer(4096);
                buf.as_bytes_mut().fill(7);
                let written = file.write_at(buf, i * 4096).await.unwrap();
                assert_eq!(written, 4096);
            }
            file.fdatasync().await.unwrap();
            file.close().await.unwrap();

            let file = DmaFile::open(&path).await.unwrap();
            for i in 0..16 {
                let read = file.read_at(i * 4096, 4096).await.unwrap();
                assert_eq!(read.len(), 4096);
            }
            file.close().await.unwrap();
            std::fs::remove_file(&path).ok();

            glommio::executor()
                .io_stats()
                .all_rings()
                .io_latency_us()
                .count()
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn latencies_are_recorded_when_asked_for() {
    assert!(
        run_and_count(true) > 0,
        "record_io_latencies(true) recorded no latencies"
    );
}

#[test]
fn nothing_is_recorded_when_not_asked_for() {
    assert_eq!(
        run_and_count(false),
        0,
        "latencies were recorded without record_io_latencies"
    );
}
