//! Buffered reads while the receive buffer is lent to the kernel.
use super::{TcpListener, TcpStream};
use crate::timer::Timer;
use futures_lite::{
    future::poll_once,
    io::{AsyncReadExt, AsyncWriteExt},
};
use std::{
    future::Future,
    io,
    io::Write as _,
    pin::pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Wake, Waker},
    time::{Duration, Instant},
};

struct CountingWaker {
    inner: Waker,
    count: Arc<AtomicUsize>,
}

impl Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.inner.wake_by_ref();
    }
}

async fn counted<F: Future>(f: F, count: Arc<AtomicUsize>) -> F::Output {
    let mut f = pin!(f);
    std::future::poll_fn(|cx| {
        let w = Waker::from(Arc::new(CountingWaker {
            inner: cx.waker().clone(),
            count: count.clone(),
        }));
        f.as_mut().poll(&mut Context::from_waker(&w))
    })
    .await
}

/// A read that was started and abandoned (dropped future, or a timeout)
/// leaves the completion in flight with earlier bytes; peek must still see
/// those bytes first.
#[test]
fn peek_after_abandoned_read_sees_bytes_in_order() {
    test_executor!(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut writer = TcpStream::connect(addr).await.unwrap();
        let mut stream = listener.accept().await.unwrap().buffered();
        let mut small = [0u8; 4];
        assert!(poll_once(stream.read(&mut small)).await.is_none());
        writer.write_all(b"AAAA").await.unwrap();
        Timer::new(Duration::from_millis(50)).await;
        writer.write_all(b"BBBB").await.unwrap();
        Timer::new(Duration::from_millis(50)).await;
        let mut p = [0u8; 4];
        let n = stream.peek(&mut p).await.unwrap();
        let peeked = p[..n].to_vec();
        let n = stream.read(&mut small).await.unwrap();
        let read = small[..n].to_vec();
        assert_eq!(
            (
                String::from_utf8(peeked).unwrap(),
                String::from_utf8(read).unwrap()
            ),
            ("AAAA".to_string(), "AAAA".to_string())
        );
    });
}

/// Same, but the only bytes there are sit in the lent buffer.
#[test]
fn peek_after_abandoned_read_does_not_hang() {
    test_executor!(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut writer = TcpStream::connect(addr).await.unwrap();
        let stream = listener.accept().await.unwrap().buffered();
        let mut stream = stream;
        let mut small = [0u8; 4];
        assert!(poll_once(stream.read(&mut small)).await.is_none());
        writer.write_all(b"hello").await.unwrap();
        Timer::new(Duration::from_millis(50)).await;
        let mut p = [0u8; 5];
        let r = crate::timer::timeout(Duration::from_secs(1), async {
            Ok(stream.peek(&mut p).await)
        })
        .await;
        assert!(r.is_ok(), "peek hung with 5 bytes received: {r:?}");
    });
}

/// A read whose buffer is at least the receive buffer's size bypasses it and
/// reads the socket directly, while earlier bytes sit in the lent buffer.
#[test]
fn large_read_after_abandoned_read_keeps_order() {
    test_executor!(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut writer = TcpStream::connect(addr).await.unwrap();
        let mut stream = listener.accept().await.unwrap().buffered();
        let mut small = [0u8; 4];
        assert!(poll_once(stream.read(&mut small)).await.is_none());
        writer.write_all(b"AAAA").await.unwrap();
        Timer::new(Duration::from_millis(50)).await;
        writer.write_all(b"BBBB").await.unwrap();
        Timer::new(Duration::from_millis(50)).await;
        let mut big = vec![0u8; 8192];
        let n = stream.read(&mut big).await.unwrap();
        let got = String::from_utf8_lossy(&big[..n]).to_string();
        assert!(
            n > 0 && "AAAABBBB".starts_with(&got),
            "stream order broken: got {got:?} first"
        );
    });
}

/// The same reordering reached through the read timeout, with no future
/// dropped by the caller at all.
#[test]
fn large_read_after_read_timeout_keeps_order() {
    test_executor!(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut writer = TcpStream::connect(addr).await.unwrap();
        let mut stream = listener.accept().await.unwrap().buffered();
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut small = [0u8; 4];
        let err = stream.read(&mut small).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        writer.write_all(b"AAAA").await.unwrap();
        Timer::new(Duration::from_millis(50)).await;
        writer.write_all(b"BBBB").await.unwrap();
        Timer::new(Duration::from_millis(50)).await;
        let mut big = vec![0u8; 8192];
        let n = stream.read(&mut big).await.unwrap();
        let got = String::from_utf8_lossy(&big[..n]).to_string();
        assert!(
            n > 0 && "AAAABBBB".starts_with(&got),
            "stream order broken: got {got:?} first"
        );
    });
}

/// Timed-out reads leave the completion pending; they must not leave a waker
/// each behind, all woken by the one completion.
#[test]
fn wakers_do_not_accumulate_across_timeouts() {
    test_executor!(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut writer = TcpStream::connect(addr).await.unwrap();
        let mut stream = listener.accept().await.unwrap().buffered();
        stream
            .set_read_timeout(Some(Duration::from_millis(2)))
            .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let mut small = [0u8; 4];
        const K: usize = 200;
        for _ in 0..K {
            let err = counted(stream.read(&mut small), count.clone())
                .await
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        }
        stream.set_read_timeout(None).unwrap();
        count.store(0, Ordering::SeqCst);
        let read = crate::spawn_local(async move {
            let mut small = [0u8; 4];
            let n = counted(stream.read(&mut small), count.clone())
                .await
                .unwrap();
            (n, count.load(Ordering::SeqCst))
        });
        Timer::new(Duration::from_millis(20)).await;
        writer.write_all(b"ping").await.unwrap();
        let (n, wakes) = read.await;
        assert_eq!(n, 4);
        assert!(wakes <= 2, "one completion woke the reader {wakes} times");
    });
}

/// A read with bytes already waiting in the socket returns them at once.
#[test]
fn read_with_data_waiting_is_ready_at_once() {
    test_executor!(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut writer = TcpStream::connect(addr).await.unwrap();
        let mut stream = listener.accept().await.unwrap().buffered();
        let mut small = [0u8; 4];
        let first = crate::spawn_local(async move {
            let n = stream.read(&mut small).await.unwrap();
            assert_eq!(n, 4);
            stream
        });
        Timer::new(Duration::from_millis(20)).await;
        writer.write_all(b"AAAA").await.unwrap();
        let mut stream = first.await;
        writer.write_all(b"BBBB").await.unwrap();
        Timer::new(Duration::from_millis(20)).await;
        let mut small = [0u8; 4];
        let r = poll_once(stream.read(&mut small)).await;
        assert!(
            r.is_some(),
            "read suspended with 4 bytes waiting in the socket"
        );
    });
}

/// A read with data waiting completes without waiting for the reactor, even
/// beside a CPU-bound task in the same queue.
#[test]
fn read_with_data_waiting_is_not_delayed_by_a_busy_neighbour() {
    let (tx, rx) = std::sync::mpsc::channel();
    test_executor!(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            s.write_all(b"AAAA").unwrap();
            std::thread::sleep(Duration::from_millis(50));
            s.write_all(b"BBBB").unwrap();
            std::thread::sleep(Duration::from_millis(1000));
        });
        let mut stream = listener.accept().await.unwrap().buffered();
        let mut small = [0u8; 4];
        assert_eq!(stream.read(&mut small).await.unwrap(), 4);
        let reader = crate::spawn_local(async move {
            let spin_until = Instant::now() + Duration::from_millis(100);
            while Instant::now() < spin_until {}
            let t0 = Instant::now();
            assert_eq!(stream.read(&mut small).await.unwrap(), 4);
            t0.elapsed()
        });
        let spinner = crate::spawn_local(async move {
            let end = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < end {
                let spin_until = Instant::now() + Duration::from_micros(500);
                while Instant::now() < spin_until {}
                crate::yield_if_needed().await;
            }
        });
        let lat = reader.await;
        spinner.await;
        peer.join().unwrap();
        tx.send(lat).unwrap();
    });
    let lat = rx.recv().unwrap();
    assert!(lat < Duration::from_millis(20), "read took {lat:?}");
}

/// A buffered stream dropped with a completion read pending: the peer must
/// see the connection close promptly.
#[test]
fn drop_with_read_pending_closes_promptly() {
    test_executor!(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let t0 = Instant::now();
            let mut b = [0u8; 1];
            let r = std::io::Read::read(&mut s, &mut b);
            (r.map_err(|e| e.kind()), t0.elapsed())
        });
        let mut stream = listener.accept().await.unwrap().buffered();
        let mut small = [0u8; 4];
        assert!(poll_once(stream.read(&mut small)).await.is_none());
        Timer::new(Duration::from_millis(20)).await;
        drop(stream);
        Timer::new(Duration::from_millis(20)).await;
        let (r, _) = peer.join().unwrap();
        assert_eq!(r, Ok(0));
    });
}

/// A large read after an abandoned read, when the only bytes received are in
/// the lent buffer: the read must return them rather than wait for more.
#[test]
fn large_read_after_abandoned_read_does_not_hang() {
    test_executor!(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut writer = TcpStream::connect(addr).await.unwrap();
        let mut stream = listener.accept().await.unwrap().buffered();
        let mut small = [0u8; 4];
        assert!(poll_once(stream.read(&mut small)).await.is_none());
        writer.write_all(b"hello").await.unwrap();
        Timer::new(Duration::from_millis(50)).await;
        let mut big = vec![0u8; 8192];
        let r = crate::timer::timeout(Duration::from_secs(1), async {
            Ok(stream.read(&mut big).await)
        })
        .await;
        assert!(r.is_ok(), "large read hung with 5 bytes received: {r:?}");
    });
}

/// shutdown(Read) while a completion read is pending ends the read with EOF.
#[test]
fn shutdown_read_with_read_pending() {
    test_executor!(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _writer = TcpStream::connect(addr).await.unwrap();
        let mut stream = listener.accept().await.unwrap().buffered();
        let mut small = [0u8; 4];
        assert!(poll_once(stream.read(&mut small)).await.is_none());
        stream.shutdown(std::net::Shutdown::Read).await.unwrap();
        let r = crate::timer::timeout(Duration::from_secs(1), async {
            Ok(stream.read(&mut small).await)
        })
        .await;
        assert!(matches!(r, Ok(Ok(0))));
    });
}
