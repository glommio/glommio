//! The plain value types glommio hands out can be cloned, compared, hashed
//! and defaulted from a dependent crate, and a read's bytes are reachable
//! through `AsRef<[u8]>`.

use glommio::{
    controllers::ControllerStatus,
    io::{CloseResult, DmaFile, ReadResult, Stat},
    IoStats, Latency, LocalExecutorBuilder,
};
use std::{collections::HashSet, rc::Rc, time::Duration};

fn bytes_of(buf: impl AsRef<[u8]>) -> Vec<u8> {
    buf.as_ref().to_vec()
}

#[test]
fn file_results_are_plain_values() {
    let dir = std::env::temp_dir().join(format!("glommio-value-types-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("abab");
    std::fs::write(&path, b"abab").unwrap();

    LocalExecutorBuilder::default()
        .spawn(move || async move {
            let file = Rc::new(DmaFile::open(&path).await.unwrap());

            let stat: Stat = file.stat().await.unwrap();
            let copy = stat.clone();
            assert_eq!(stat, copy);
            assert_eq!(stat.file_size, 4);

            let read = file.read_at(0, 4).await.unwrap();
            let halves = (
                ReadResult::slice(&read, 0, 2).unwrap(),
                ReadResult::slice(&read, 2, 2).unwrap(),
            );
            assert_eq!(halves.0, halves.1);
            assert_eq!(bytes_of(read), b"abab");

            let other = file.clone();
            let first: CloseResult = file.close_rc().await.unwrap();
            assert_eq!(first, CloseResult::Unreferenced);
            let last = other.close_rc().await.unwrap();
            assert_eq!(last, CloseResult::Closed);
            let again = first.clone();
            assert_eq!(again, first);
            assert_ne!(first, last);
        })
        .unwrap()
        .join()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn scheduler_values_are_comparable_and_hashable() {
    let latencies: HashSet<Latency> = [
        Latency::NotImportant,
        Latency::Matters(Duration::from_millis(1)),
        Latency::NotImportant,
    ]
    .into_iter()
    .collect();
    assert_eq!(latencies.len(), 2);

    assert_eq!(ControllerStatus::Enabled, ControllerStatus::Enabled);
    assert_ne!(ControllerStatus::Enabled, ControllerStatus::Disabled(1));
}

#[test]
fn io_stats_can_be_cloned_and_defaulted() {
    let empty = IoStats::default();
    assert_eq!(empty.all_rings().files_opened(), 0);

    LocalExecutorBuilder::default()
        .spawn(|| async move {
            let stats = glommio::executor().io_stats();
            let kept = stats.clone();
            assert_eq!(
                kept.all_rings().files_opened(),
                stats.all_rings().files_opened()
            );
        })
        .unwrap()
        .join()
        .unwrap();
}
