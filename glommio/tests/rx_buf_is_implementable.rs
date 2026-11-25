//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! `buffered_with` takes any `Buffered` receive buffer, which a caller can
//! only supply if the traits involved can be named from outside the crate.
//!
//! An integration test sees exactly what a dependent crate sees: this file
//! compiles only if `RxBuf` and `NonBuffered` are reachable.
//! From inside the crate they always were, which is why no unit test could
//! notice that `buffered_with` had exactly one possible argument.

use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use glommio::{
    net::{Buffered, NonBuffered, RxBuf, TcpListener, TcpStream},
    LocalExecutor,
};

/// The smallest thing that can hold bytes between the socket and the caller.
///
/// Fixed capacity with two cursors rather than a growing `Vec`: `unfilled` is
/// called before the read completes, so a buffer that grew there would claim
/// to hold bytes that have not arrived.
struct MyBuffer {
    bytes: Vec<u8>,
    head: usize,
    tail: usize,
}

impl MyBuffer {
    const SIZE: usize = 4096;
}

impl Default for MyBuffer {
    fn default() -> Self {
        MyBuffer {
            bytes: vec![0; Self::SIZE],
            head: 0,
            tail: 0,
        }
    }
}

impl RxBuf for MyBuffer {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        let taken = self.peek(buf);
        self.head += taken;
        taken
    }

    fn peek(&self, buf: &mut [u8]) -> usize {
        let taken = std::cmp::min(self.tail - self.head, buf.len());
        buf[..taken].copy_from_slice(&self.bytes[self.head..self.head + taken]);
        taken
    }

    fn is_empty(&self) -> bool {
        self.head >= self.tail
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[self.head..self.tail]
    }

    fn consume(&mut self, amt: usize) {
        self.head = std::cmp::min(self.head + amt, self.tail);
    }

    fn buffer_size(&self) -> usize {
        Self::SIZE
    }

    fn handle_result(&mut self, result: usize) {
        self.tail += result;
    }

    fn unfilled(&mut self) -> &mut [u8] {
        if self.is_empty() {
            self.head = 0;
            self.tail = 0;
        }
        &mut self.bytes[self.tail..]
    }
}

impl Buffered for MyBuffer {}

/// Accepts a stream with any receive buffer, which needs `RxBuf` in scope.
fn local_port<B: RxBuf>(stream: &TcpStream<B>) -> u16 {
    stream.local_addr().unwrap().port()
}

#[test]
fn a_foreign_rx_buf_can_back_a_stream() {
    LocalExecutor::default().run(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let reader = glommio::spawn_local(async move {
            let mut stream = listener
                .accept()
                .await
                .unwrap()
                .buffered_with(MyBuffer::default());
            assert_eq!(local_port(&stream), addr.port());
            let mut got = [0u8; 32];
            let read = stream.read(&mut got).await.unwrap();
            got[..read].to_vec()
        })
        .detach();

        let mut writer: TcpStream<NonBuffered> = TcpStream::connect(addr).await.unwrap();
        assert_ne!(local_port(&writer), addr.port());
        writer.write_all(b"through my own buffer").await.unwrap();

        assert_eq!(reader.await.unwrap(), b"through my own buffer".to_vec());
    });
}

#[test]
fn a_foreign_rx_buf_need_not_lend_its_buffer() {
    let mut buffer = MyBuffer::default();
    assert!(
        RxBuf::take_kernel_buffer(&mut buffer).is_none(),
        "the default must keep an implementation on the readiness path"
    );
}
