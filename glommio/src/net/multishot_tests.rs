//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! A buffered stream reading through a multishot receive, from the stream's
//! side: what it sees is the same byte stream, whatever the kernel's buffers
//! are doing underneath.

use super::{Preallocated, TcpListener, TcpStream};
use crate::{timer::Timer, LocalExecutorBuilder, Placement};
use futures_lite::{
    future::poll_once,
    io::{AsyncReadExt, AsyncWriteExt},
};
use std::time::Duration;

/// An executor with `count` provided buffers of `size` bytes.
fn executor(count: usize, size: usize) -> crate::LocalExecutor {
    LocalExecutorBuilder::new(Placement::Unbound)
        .recv_buffers(crate::RecvBuffers::exact(count, size))
        .make()
        .unwrap()
}

/// A connected pair on loopback, the accepted side buffered with `rx`.
async fn pair(rx: Preallocated) -> (TcpStream, TcpStream<Preallocated>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let writer = TcpStream::connect(addr).await.unwrap();
    let reader = listener.accept().await.unwrap().buffered_with(rx);
    (writer, reader)
}

fn buffers_held() -> usize {
    crate::executor()
        .reactor()
        .sys
        .recv_buffers()
        .map(|b| b.borrow().held())
        .unwrap_or(0)
}

/// Messages come out in order and whole, and every kernel buffer goes back
/// once its bytes are consumed.
#[test]
fn reads_arrive_in_order_and_give_their_buffers_back() {
    executor(8, 64).run(async {
        let (mut writer, mut reader) = pair(Preallocated::new(256)).await;
        for i in 0..20u8 {
            writer.write_all(&[i; 10]).await.unwrap();
            let mut got = [0u8; 10];
            reader.read_exact(&mut got).await.unwrap();
            assert_eq!(got, [i; 10]);
        }
        assert_eq!(
            buffers_held(),
            0,
            "a consumed arrival still holds its buffer"
        );
    });
}

/// A stream buffer smaller than a kernel buffer drains one arrival in
/// several steps without losing or reordering anything.
#[test]
fn a_small_stream_buffer_drains_a_large_arrival_in_steps() {
    executor(4, 1024).run(async {
        let (mut writer, mut reader) = pair(Preallocated::new(16)).await;
        let message: Vec<u8> = (0..200u8).collect();
        writer.write_all(&message).await.unwrap();
        Timer::new(Duration::from_millis(20)).await;

        let mut got = vec![0u8; 200];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(got, message);
        assert_eq!(buffers_held(), 0);
    });
}

/// More arrivals than buffers: the kernel ends the request with ENOBUFS, the
/// stream takes what is there, re-arms, and the reader never notices.
#[test]
fn running_out_of_buffers_is_invisible_to_the_reader() {
    executor(2, 32).run(async {
        let (mut writer, mut reader) = pair(Preallocated::new(256)).await;
        let mut expected = Vec::new();
        for i in 0..6u8 {
            writer.write_all(&[i; 8]).await.unwrap();
            expected.extend_from_slice(&[i; 8]);
            Timer::new(Duration::from_millis(5)).await;
        }
        let mut got = vec![0u8; expected.len()];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(got, expected);

        writer.write_all(b"after").await.unwrap();
        let mut got = [0u8; 5];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"after");
        assert_eq!(buffers_held(), 0);
        assert!(
            crate::executor().io_stats().recv_buffers_exhausted >= 1,
            "running dry was not counted"
        );
    });
}

/// End of file arrives through the same request and reads as zero.
#[test]
fn end_of_file_reads_as_zero() {
    executor(4, 64).run(async {
        let (mut writer, mut reader) = pair(Preallocated::default()).await;
        writer.write_all(b"last").await.unwrap();
        drop(writer);
        let mut got = Vec::new();
        reader.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"last");
        let mut again = [0u8; 1];
        assert_eq!(reader.read(&mut again).await.unwrap(), 0);
    });
}

/// A read larger than the stream buffer used to go to the socket directly;
/// with a request in flight it must not, and still returns the bytes in order.
#[test]
fn a_read_larger_than_the_buffer_keeps_order() {
    executor(8, 64).run(async {
        let (mut writer, mut reader) = pair(Preallocated::new(32)).await;
        let message: Vec<u8> = (0..255u8).collect();
        writer.write_all(&message).await.unwrap();
        Timer::new(Duration::from_millis(20)).await;

        let mut got = vec![0u8; message.len()];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(got, message);
    });
}

/// `peek` sees queued arrivals without consuming them.
#[test]
fn peek_sees_queued_arrivals_and_consumes_nothing() {
    executor(8, 64).run(async {
        let (mut writer, mut reader) = pair(Preallocated::default()).await;
        writer.write_all(b"peekaboo").await.unwrap();
        Timer::new(Duration::from_millis(20)).await;

        let mut peeked = [0u8; 4];
        assert_eq!(reader.peek(&mut peeked).await.unwrap(), 4);
        assert_eq!(&peeked, b"peek");
        let mut got = [0u8; 8];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"peekaboo");
    });
}

/// The read timeout applies while the request waits for an arrival.
#[test]
fn read_timeout_fires_while_waiting_on_the_request() {
    executor(8, 64).run(async {
        let (_writer, mut reader) = pair(Preallocated::default()).await;
        reader
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut got = [0u8; 1];
        let err = reader.read(&mut got).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    });
}

/// Dropping a stream with arrivals still queued gives their buffers back.
#[test]
fn dropping_a_stream_returns_its_queued_buffers() {
    executor(8, 64).run(async {
        let (mut writer, reader) = pair(Preallocated::default()).await;
        for _ in 0..3 {
            writer.write_all(b"queued").await.unwrap();
            Timer::new(Duration::from_millis(5)).await;
        }
        drop(reader);
        Timer::new(Duration::from_millis(20)).await;
        assert_eq!(buffers_held(), 0);
    });
}

/// Without a buffer pool the buffered stream reads exactly as it did.
#[test]
fn without_buffers_the_stream_reads_the_old_way() {
    executor(0, 0).run(async {
        let (mut writer, mut reader) = pair(Preallocated::default()).await;
        writer.write_all(b"plain").await.unwrap();
        let mut got = [0u8; 5];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"plain");
    });
}

/// An arrival the reactor has not looked at yet is still seen on the first
/// poll: the stream reaps the ring inline instead of waiting for a pass.
#[test]
fn an_arrival_already_posted_is_ready_on_the_first_poll() {
    executor(8, 64).run(async {
        let (mut writer, mut reader) = pair(Preallocated::default()).await;
        writer.write_all(b"now").await.unwrap();
        std::thread::sleep(Duration::from_millis(20));

        let mut got = [0u8; 3];
        let polled = poll_once(reader.read(&mut got)).await;
        assert_eq!(
            polled.map(|r| r.unwrap()),
            Some(3),
            "read suspended with data posted"
        );
        assert_eq!(&got, b"now");
    });
}

/// A reap that took only the stream's own arrival asks for nothing: a pass
/// per message is what a pipeline cannot afford.
#[test]
fn an_inline_reap_of_an_arrival_alone_asks_for_no_pass() {
    LocalExecutorBuilder::new(Placement::Unbound)
        .recv_buffers(crate::RecvBuffers::exact(8, 64))
        .preempt_timer(Duration::from_secs(10))
        .make()
        .unwrap()
        .run(async {
            let (mut writer, mut reader) = pair(Preallocated::default()).await;
            Timer::new(Duration::from_millis(5)).await;
            writer.write_all(b"x").await.unwrap();
            std::thread::sleep(Duration::from_millis(20));
            let mut got = [0u8; 1];
            poll_once(reader.read(&mut got)).await.unwrap().unwrap();
            assert!(
                !crate::executor().need_preempt(),
                "a reap of the stream's own arrival asked for a pass"
            );
        });
}

/// Arrivals that are already queued come out of one fill together, not one
/// per poll: a pipeline's replies sit in the socket as a dozen completions
/// and a reader must not pay a round trip for each.
#[test]
fn one_fill_takes_every_queued_arrival_that_fits() {
    use futures_lite::io::AsyncBufReadExt;
    executor(32, 64).run(async {
        let (mut writer, mut reader) = pair(Preallocated::new(4096)).await;
        for i in 0..12u8 {
            writer.write_all(&[i; 48]).await.unwrap();
            Timer::new(Duration::from_millis(3)).await;
        }
        let filled = reader.fill_buf().await.unwrap().len();
        assert_eq!(
            filled,
            12 * 48,
            "a fill returned one arrival of the dozen queued"
        );
    });
}

/// A message larger than a kernel buffer comes back whole and in order
/// through the stream, whether the kernel bundled it or split it.
#[test]
fn a_message_spanning_kernel_buffers_reads_whole() {
    executor(8, 32).run(async {
        let (mut writer, mut reader) = pair(Preallocated::new(4096)).await;
        let message: Vec<u8> = (0..120u8).collect();
        writer.write_all(&message).await.unwrap();
        let mut got = vec![0u8; message.len()];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(got, message);
        assert_eq!(buffers_held(), 0);
    });
}
