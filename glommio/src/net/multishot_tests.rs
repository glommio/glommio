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

/// A task queue that keeps the core busy for `for_how_long`, yielding at
/// every slice, so the reactor never sleeps and every pass comes from the
/// preempt timer: the regime in which a reply has to be found by the pass.
fn burn(for_how_long: Duration) -> crate::Task<()> {
    let queue = crate::executor().create_task_queue(
        crate::Shares::Static(1),
        crate::Latency::NotImportant,
        "burner",
    );
    crate::spawn_local_into(
        async move {
            let until = std::time::Instant::now() + for_how_long;
            while std::time::Instant::now() < until {
                crate::yield_if_needed().await;
            }
        },
        queue,
    )
    .unwrap()
}

/// Fan-in under stress, with peeks: twelve connections writing chunks that
/// span buffers, a quarter of the readers dropped mid-stream with arrivals
/// queued, and every third reader peeking before it reads. Every byte must
/// come out in order, through the peek as much as the read, and every
/// buffer must come back. A peek that waited on the socket beside the armed
/// receive saw the bytes behind the ones the request was taking, three runs
/// in three.
#[test]
fn fan_in_with_peeks_and_dropped_readers_keeps_order_and_buffers() {
    use std::io::Write as _;
    for round in 0..3u64 {
        executor(8, 64).run(async move {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            const CONNS: usize = 12;
            let writers = std::thread::spawn(move || {
                let mut socks: Vec<_> = (0..CONNS)
                    .map(|_| std::net::TcpStream::connect(addr).unwrap())
                    .collect();
                let mut seed = round.wrapping_mul(6364136223846793005).wrapping_add(1);
                let mut sent = [0usize; CONNS];
                for _ in 0..400 {
                    seed = seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let c = (seed >> 33) as usize % CONNS;
                    let len = 1 + (seed >> 40) as usize % 300;
                    let chunk: Vec<u8> = (0..len).map(|i| ((sent[c] + i) % 251) as u8).collect();
                    if socks[c].write_all(&chunk).is_ok() {
                        sent[c] += len;
                    }
                    if seed % 7 == 0 {
                        std::thread::sleep(Duration::from_micros(50));
                    }
                }
            });
            let mut tasks = vec![];
            for i in 0..CONNS {
                let reader = listener
                    .accept()
                    .await
                    .unwrap()
                    .buffered_with(Preallocated::new(97));
                tasks.push(crate::spawn_local(async move {
                    let mut reader = reader;
                    let mut got = 0usize;
                    let mut buf = [0u8; 113];
                    let quit_at = if i % 4 == 0 { 500 } else { usize::MAX };
                    loop {
                        if got >= quit_at {
                            return;
                        }
                        if i % 3 == 0 {
                            let mut p = [0u8; 50];
                            let n = reader.peek(&mut p).await.unwrap();
                            let want: Vec<u8> = (0..n).map(|k| ((got + k) % 251) as u8).collect();
                            assert_eq!(
                                &p[..n],
                                &want[..],
                                "round {round}: a peek on connection {i} at byte {got} was out of order"
                            );
                        }
                        let n = reader.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        for (k, b) in buf[..n].iter().enumerate() {
                            assert_eq!(*b, ((got + k) % 251) as u8, "connection {i} byte {}", got + k);
                        }
                        got += n;
                    }
                }));
            }
            for t in tasks {
                t.await;
            }
            writers.join().unwrap();
            Timer::new(Duration::from_millis(20)).await;
            let mut woke = 0;
            crate::executor().reactor().sys.poll_io(&mut woke).unwrap();
            assert_eq!(buffers_held(), 0, "round {round}: buffers leaked");
        });
    }
}

/// A reply that lands while a CPU-bound queue runs its slice is seen on the
/// pass that ends the slice, not the one after: the stream's own entry a
/// slice ago does not excuse the pass from entering. The reader parks right
/// after a pass, so a reply sent a few milliseconds later lands in that
/// same slice: the pass that ends it finds the reply, or the next one does,
/// a slice later. Measured at 32 ms against 52 ms with a 20 ms slice; the
/// bound sits between.
#[test]
fn a_reply_beside_a_cpu_bound_queue_waits_one_slice_not_two() {
    use std::time::Instant;
    const SLICE: Duration = Duration::from_millis(20);
    LocalExecutorBuilder::new(Placement::Unbound)
        .recv_buffers(crate::RecvBuffers::exact(8, 64))
        .preempt_timer(SLICE)
        .make()
        .unwrap()
        .run(async {
            let (writer, mut reader) = pair(Preallocated::default()).await;
            let burner = burn(SLICE * 20);
            let fd = std::os::fd::AsRawFd::as_raw_fd(&writer);
            let mut total = Duration::ZERO;
            let phases = [2u64, 6, 10, 14];
            for phase in phases {
                let sender = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(phase));
                    let at = Instant::now();
                    let n = unsafe { libc::send(fd, b"x".as_ptr() as *const _, 1, 0) };
                    assert_eq!(n, 1);
                    at
                });
                let mut got = [0u8; 1];
                reader.read_exact(&mut got).await.unwrap();
                let seen = Instant::now();
                total += seen.duration_since(sender.join().unwrap());
            }
            burner.await;
            let mean = total / phases.len() as u32;
            assert!(
                mean < SLICE * 2,
                "replies waited {mean:?} on average beside a CPU-bound queue with a {SLICE:?} slice"
            );
            drop(writer);
        });
}

/// A connection nobody reads must not take every buffer: once its queue
/// holds more than its owner can take, its request is cancelled and the
/// socket backs up to the peer, while every other connection on the
/// executor keeps reading without running dry. Reading it later gets every
/// byte in order, through a re-armed request. The flood comes as small
/// writes a moment apart, since one bundle takes whatever the socket holds
/// at that moment and the ceiling acts between bundles.
#[test]
fn an_unread_connection_is_throttled_and_the_others_keep_their_buffers() {
    executor(16, 64).run(async {
        let (flooder, unread) = pair(Preallocated::new(256)).await;
        let (mut writer, mut reader) = pair(Preallocated::default()).await;
        let flood: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&flooder);
        let flooder_thread = std::thread::spawn(move || {
            for chunk in flood.chunks(100) {
                let mut sent = 0;
                while sent < chunk.len() {
                    let n = unsafe {
                        libc::send(
                            fd,
                            chunk[sent..].as_ptr() as *const _,
                            chunk.len() - sent,
                            0,
                        )
                    };
                    if n < 0 {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    sent += n as usize;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Timer::new(Duration::from_millis(100)).await;
        let held = buffers_held();
        assert!(
            held <= 8,
            "the unread connection took {held} of 16 buffers; its ceiling is 256 bytes, four"
        );

        for i in 0..20u8 {
            writer.write_all(&[i; 32]).await.unwrap();
            let mut got = [0u8; 32];
            reader.read_exact(&mut got).await.unwrap();
            assert_eq!(got, [i; 32]);
        }
        assert_eq!(
            crate::executor().io_stats().recv_buffers_exhausted,
            0,
            "the readers ran dry beside an unread connection"
        );

        let mut unread = unread;
        let mut got = vec![0u8; 4000];
        unread.read_exact(&mut got).await.unwrap();
        assert!(
            got.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8),
            "the throttled connection lost or reordered bytes"
        );
        flooder_thread.join().unwrap();
        drop(flooder);
    });
}
