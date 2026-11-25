// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! A `DmaBuffer` made from a `ReadResult` through the public `From` impl can
//! be mutated from outside the crate, and the mutation stays in the buffer.

use glommio::{
    io::{DmaBuffer, OpenOptions},
    LocalExecutorBuilder,
};

#[test]
fn a_buffer_converted_from_a_read_is_mutable_and_private() {
    let dir = std::env::temp_dir().join(format!("glommio-dmabuf-from-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file_path = dir.join("testfile");

    LocalExecutorBuilder::default()
        .spawn(move || async move {
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .dma_open(&file_path)
                .await
                .unwrap();
            let mut source = file.alloc_dma_buffer(4096);
            source.as_bytes_mut().fill(b'a');
            file.write_at(source, 0).await.unwrap();

            let read = file.read_at(0, 4096).await.unwrap();
            let kept = read.clone();
            let mut buffer: DmaBuffer = read.into();
            buffer.as_bytes_mut()[..5].copy_from_slice(b"hello");
            buffer.as_mut()[5] = b'!';

            assert_eq!(&buffer.as_bytes()[..6], b"hello!");
            assert_eq!(buffer.len(), 4096);
            assert!(kept.iter().all(|&b| b == b'a'));
            file.close().await.unwrap();
        })
        .unwrap()
        .join()
        .unwrap();

    let _ = std::fs::remove_dir_all(&dir);
}
