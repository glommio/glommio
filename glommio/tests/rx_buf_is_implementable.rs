//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! `buffered_with` takes any `Buffered` receive buffer, which a caller can
//! only supply if the traits involved can be named from outside the crate.
//!
//! An integration test sees exactly what a dependent crate sees: this file
//! compiles only if `RxBuf`, `NonBuffered` and `OwnedRxBuf` are reachable.
//! From inside the crate they always were, which is why no unit test could
//! notice that `buffered_with` had exactly one possible argument.

use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use glommio::{
    net::{Buffered, NonBuffered, OwnedRxBuf, RxBuf, TcpListener, TcpStream},
    LocalExecutor,
};
use std::{cell::Cell, rc::Rc, time::Duration};

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

/// The same buffer, opting in to completion reads, and counting what it lent
/// and got back so the test can tell which path the read took.
struct LendingBuffer {
    inner: MyBuffer,
    lent: Rc<Cell<usize>>,
    restored: Rc<Cell<usize>>,
}

impl RxBuf for LendingBuffer {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        self.inner.read(buf)
    }

    fn peek(&self, buf: &mut [u8]) -> usize {
        self.inner.peek(buf)
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn as_bytes(&self) -> &[u8] {
        self.inner.as_bytes()
    }

    fn consume(&mut self, amt: usize) {
        self.inner.consume(amt)
    }

    fn buffer_size(&self) -> usize {
        self.inner.buffer_size()
    }

    fn handle_result(&mut self, result: usize) {
        self.inner.handle_result(result)
    }

    fn unfilled(&mut self) -> &mut [u8] {
        self.inner.unfilled()
    }

    fn take_kernel_buffer(&mut self) -> Option<OwnedRxBuf> {
        self.lent.set(self.lent.get() + 1);
        self.inner.head = 0;
        self.inner.tail = 0;
        Some(OwnedRxBuf::new(std::mem::take(&mut self.inner.bytes)))
    }

    fn restore_kernel_buffer(&mut self, buffer: OwnedRxBuf) {
        self.restored.set(self.restored.get() + 1);
        self.inner.bytes = buffer.into_vec();
        self.inner.head = 0;
        self.inner.tail = 0;
    }
}

impl Buffered for LendingBuffer {}

#[test]
fn a_foreign_rx_buf_can_lend_its_buffer_to_the_kernel() {
    LocalExecutor::default().run(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let lent = Rc::new(Cell::new(0));
        let restored = Rc::new(Cell::new(0));

        let buffer = LendingBuffer {
            inner: MyBuffer::default(),
            lent: lent.clone(),
            restored: restored.clone(),
        };
        let reader = glommio::spawn_local(async move {
            let mut stream = listener.accept().await.unwrap().buffered_with(buffer);
            let mut got = [0u8; 64];
            let read = stream.read(&mut got).await.unwrap();
            got[..read].to_vec()
        })
        .detach();

        let mut writer = TcpStream::connect(addr).await.unwrap();
        while lent.get() == 0 {
            glommio::timer::sleep(Duration::from_millis(1)).await;
        }
        writer.write_all(b"filled by the kernel").await.unwrap();

        assert_eq!(reader.await.unwrap(), b"filled by the kernel".to_vec());
        assert_eq!(
            (lent.get(), restored.get()),
            (1, 1),
            "the read went through the lent buffer and gave it back"
        );
    });
}

#[test]
#[should_panic(expected = "non-zero length")]
fn lending_an_empty_buffer_is_refused() {
    let _ = OwnedRxBuf::new(Vec::with_capacity(4096));
}
