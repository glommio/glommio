//! What multishot recv and provided buffers would be worth.
//!
//! glommio reads a socket by speculating -- a non-blocking `recv`, then a
//! `PollAdd` on `EAGAIN`, then a second `recv` when the poll completes. Two
//! syscalls, an SQE and a CQE per readiness edge. The alternatives put the
//! read itself on the ring.
//!
//! The regime that matters is **data that has not arrived yet**: a server
//! waiting on many connections, woken when one of them speaks. If the data is
//! already buffered the speculation wins trivially and there is nothing to
//! discuss, so here the server arms first and the client writes afterwards.
//!
//! Four rungs, all reading one message from each of `CONNECTIONS` sockets per
//! round:
//!
//! 1. **readiness** — `recv`, `PollAdd` on `EAGAIN`, `recv` again. What
//!    glommio does, written by hand.
//! 2. **single-shot Recv** — one `IORING_OP_RECV` per message. The kernel's
//!    internal fast poll handles not-ready, so it is one SQE and one CQE.
//! 3. **multishot recv + provided buffer ring** — armed once per connection,
//!    the kernel picks a buffer when data lands and posts a CQE. Zero
//!    syscalls and no per-read submission.
//! 4. **glommio** — the library, for scale.
//!
//! CPU time is the meter, not wall clock: the client is the bottleneck in
//! every rung, so wall clock measures the client.
//!
//! Run with:
//! ```bash
//! cargo run --release --example recv_ladder
//! ```

use io_uring::{
    cqueue, opcode, squeue,
    types::{BufRingEntry, Fd},
    IoUring,
};
use std::{
    alloc::{alloc_zeroed, Layout},
    io::Write,
    net::{TcpListener, TcpStream},
    os::unix::io::{AsRawFd, RawFd},
    sync::atomic::{AtomicU16, Ordering},
    sync::mpsc,
    time::Duration,
};

const CONNECTIONS: usize = 256;
const ROUNDS: usize = 60;
/// Rounds run before the meter starts: the first pass through a rung faults
/// in buffers and warms the socket structures, and charging that to the rung
/// is how a ladder ends up measuring its own first iteration.
const WARMUP: usize = 5;
/// Server and client on distinct cores in the same L3, so the rungs are not
/// ranked by wherever the scheduler happened to put them.
const CPU_SERVER: usize = 0;
const CPU_CLIENT: usize = 4;
const MESSAGES: usize = CONNECTIONS * ROUNDS;
const MSG: &[u8] = b"GET / HTTP/1.1\r\nhost: localhost\r\n\r\n";
const BUF_LEN: usize = 256;
const RING_ENTRIES: u16 = 512;
const BGID: u16 = 7;

fn pin(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn report(label: &str, cpu: Duration) {
    println!(
        "{label:<48} {:>8.0} ns cpu / message",
        cpu.as_nanos() as f64 / MESSAGES as f64
    );
}

/// Connections, plus a writer that speaks only when told to -- so the server
/// is always armed and waiting before any data exists.
struct Peers {
    server_fds: Vec<RawFd>,
    write_now: mpsc::Sender<()>,
    writer: Option<std::thread::JoinHandle<()>>,
    _listener: TcpListener,
}

fn peers() -> Peers {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (write_now, when) = mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        pin(CPU_CLIENT);
        let mut clients: Vec<TcpStream> = (0..CONNECTIONS)
            .map(|_| {
                let stream = TcpStream::connect(addr).unwrap();
                stream.set_nodelay(true).unwrap();
                stream
            })
            .collect();
        while when.recv().is_ok() {
            for client in clients.iter_mut() {
                client.write_all(MSG).unwrap();
            }
        }
    });

    let server_fds = (0..CONNECTIONS)
        .map(|_| {
            let (stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(true).unwrap();
            let fd = stream.as_raw_fd();
            std::mem::forget(stream); // the probe owns the fd for its lifetime
            fd
        })
        .collect();

    Peers {
        server_fds,
        write_now,
        writer: Some(writer),
        _listener: listener,
    }
}

impl Peers {
    fn speak(&self) {
        self.write_now.send(()).unwrap();
    }

    fn finish(mut self) {
        drop(std::mem::replace(&mut self.write_now, mpsc::channel().0));
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        for fd in &self.server_fds {
            unsafe { libc::close(*fd) };
        }
    }
}

fn recv_now(fd: RawFd, buf: &mut [u8]) -> Option<usize> {
    let read = unsafe {
        libc::recv(
            fd,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if read >= 0 {
        Some(read as usize)
    } else {
        None
    }
}

/// Rung 1: glommio's shape. Speculate, register readiness, read again.
fn run_readiness() {
    let peers = peers();
    let mut ring: IoUring = IoUring::new(1024).unwrap();
    let mut buf = [0u8; BUF_LEN];
    let mut cpu = Duration::ZERO;

    for round in 0..WARMUP + ROUNDS {
        let counted = round >= WARMUP;
        let cpu_start = thread_cpu();

        // Every socket is empty, so every speculation fails and every
        // connection needs a readiness registration. That is the point.
        let mut waiting = 0;
        for (index, fd) in peers.server_fds.iter().enumerate() {
            if recv_now(*fd, &mut buf).is_none() {
                let poll = opcode::PollAdd::new(Fd(*fd), libc::POLLIN as _)
                    .build()
                    .user_data(index as u64);
                unsafe { ring.submission().push(&poll).unwrap() };
                waiting += 1;
            }
        }
        ring.submit().unwrap();
        if counted {
            cpu += thread_cpu() - cpu_start;
        }

        peers.speak();

        let cpu_start = thread_cpu();
        let mut ready = Vec::with_capacity(waiting);
        while ready.len() < waiting {
            ring.submit_and_wait(1).unwrap();
            for completion in ring.completion().collect::<Vec<cqueue::Entry>>() {
                ready.push(completion.user_data() as usize);
            }
        }
        // The second syscall: the poll said readable, now go and read.
        for index in ready {
            let read = recv_now(peers.server_fds[index], &mut buf);
            assert_eq!(read, Some(MSG.len()), "short read on the readiness path");
        }
        if counted {
            cpu += thread_cpu() - cpu_start;
        }
    }

    peers.finish();
    report("readiness: recv, PollAdd, recv (glommio's shape)", cpu);
}

/// Rung 2: one `Recv` per message, kernel-side fast poll handling not-ready.
fn run_single_shot() {
    let peers = peers();
    let mut ring: IoUring = IoUring::new(1024).unwrap();
    let mut buffers = vec![[0u8; BUF_LEN]; CONNECTIONS];
    let mut cpu = Duration::ZERO;

    for round in 0..WARMUP + ROUNDS {
        let counted = round >= WARMUP;
        let cpu_start = thread_cpu();
        for (index, fd) in peers.server_fds.iter().enumerate() {
            let recv = opcode::Recv::new(Fd(*fd), buffers[index].as_mut_ptr(), BUF_LEN as u32)
                .build()
                .user_data(index as u64);
            unsafe { ring.submission().push(&recv).unwrap() };
        }
        ring.submit().unwrap();
        if counted {
            cpu += thread_cpu() - cpu_start;
        }

        peers.speak();

        let cpu_start = thread_cpu();
        let mut received = 0;
        while received < CONNECTIONS {
            ring.submit_and_wait(1).unwrap();
            for completion in ring.completion().collect::<Vec<cqueue::Entry>>() {
                assert_eq!(completion.result(), MSG.len() as i32);
                received += 1;
            }
        }
        if counted {
            cpu += thread_cpu() - cpu_start;
        }
    }

    peers.finish();
    report("single-shot Recv (1 SQE + 1 CQE per message)", cpu);
}

/// A ring of buffers the kernel picks from when data actually arrives.
struct BufRing {
    entries: *mut BufRingEntry,
    memory: *mut u8,
    tail: u16,
}

impl BufRing {
    fn new(ring: &IoUring) -> Self {
        let ring_bytes = RING_ENTRIES as usize * std::mem::size_of::<BufRingEntry>();
        let entries = unsafe {
            alloc_zeroed(Layout::from_size_align(ring_bytes, 4096).unwrap()) as *mut BufRingEntry
        };
        let memory = unsafe {
            alloc_zeroed(Layout::from_size_align(RING_ENTRIES as usize * BUF_LEN, 4096).unwrap())
        };

        unsafe {
            ring.submitter()
                .register_buf_ring_with_flags(entries as u64, RING_ENTRIES, BGID, 0)
                .expect("buffer rings need kernel 5.19+");
        }

        let mut buf_ring = BufRing {
            entries,
            memory,
            tail: 0,
        };
        for bid in 0..RING_ENTRIES {
            buf_ring.give_back(bid);
        }
        buf_ring.publish();
        buf_ring
    }

    /// Hands one buffer to the kernel. Costs a memory write, not a syscall --
    /// which is the whole economic argument for buffer rings.
    fn give_back(&mut self, bid: u16) {
        let slot = (self.tail & (RING_ENTRIES - 1)) as usize;
        unsafe {
            let entry = &mut *self.entries.add(slot);
            entry.set_addr(self.memory.add(bid as usize * BUF_LEN) as u64);
            entry.set_len(BUF_LEN as u32);
            entry.set_bid(bid);
        }
        self.tail = self.tail.wrapping_add(1);
    }

    fn publish(&self) {
        unsafe {
            let tail = BufRingEntry::tail(self.entries) as *const AtomicU16;
            (*tail).store(self.tail, Ordering::Release);
        }
    }
}

/// Rung 2b: speculate first, then let the ring do the read.
///
/// The shape a runtime would actually ship: `yolo_recv` still wins outright
/// when data is already buffered (a streaming reader), and when it is not,
/// the `PollAdd` and the second `recv` collapse into one `Recv`.
fn run_hybrid() {
    let peers = peers();
    let mut ring: IoUring = IoUring::new(1024).unwrap();
    let mut buffers = vec![[0u8; BUF_LEN]; CONNECTIONS];
    let mut probe = [0u8; BUF_LEN];
    let mut cpu = Duration::ZERO;

    for round in 0..WARMUP + ROUNDS {
        let counted = round >= WARMUP;
        let cpu_start = thread_cpu();

        let mut waiting = 0;
        for (index, fd) in peers.server_fds.iter().enumerate() {
            // Empty every time here, which is the regime under test; a
            // streaming reader would take this branch and stop.
            if recv_now(*fd, &mut probe).is_none() {
                let recv = opcode::Recv::new(Fd(*fd), buffers[index].as_mut_ptr(), BUF_LEN as u32)
                    .build()
                    .user_data(index as u64);
                unsafe { ring.submission().push(&recv).unwrap() };
                waiting += 1;
            }
        }
        ring.submit().unwrap();
        if counted {
            cpu += thread_cpu() - cpu_start;
        }

        peers.speak();

        let cpu_start = thread_cpu();
        let mut received = 0;
        while received < waiting {
            ring.submit_and_wait(1).unwrap();
            for completion in ring.completion().collect::<Vec<cqueue::Entry>>() {
                assert_eq!(completion.result(), MSG.len() as i32);
                received += 1;
            }
        }
        if counted {
            cpu += thread_cpu() - cpu_start;
        }
    }

    peers.finish();
    report("speculate, then Recv (no second syscall)", cpu);
}

/// Rung 3: armed once per connection, then nothing but completions.
fn run_multishot() {
    let peers = peers();
    let mut ring: IoUring = IoUring::builder().build(1024).unwrap();
    let mut buf_ring = BufRing::new(&ring);
    let mut cpu = Duration::ZERO;

    let cpu_start = thread_cpu();
    for (index, fd) in peers.server_fds.iter().enumerate() {
        let recv = opcode::RecvMulti::new(Fd(*fd), BGID)
            .build()
            .flags(squeue::Flags::BUFFER_SELECT)
            .user_data(index as u64);
        unsafe { ring.submission().push(&recv).unwrap() };
    }
    ring.submit().unwrap();
    cpu += thread_cpu() - cpu_start;

    for round in 0..WARMUP + ROUNDS {
        let counted = round >= WARMUP;
        peers.speak();

        let cpu_start = thread_cpu();
        let mut received = 0;
        while received < CONNECTIONS {
            ring.submit_and_wait(1).unwrap();
            for completion in ring.completion().collect::<Vec<cqueue::Entry>>() {
                assert!(
                    completion.result() > 0,
                    "multishot recv failed: {} (-105 is ENOBUFS)",
                    completion.result()
                );
                assert!(
                    cqueue::more(completion.flags()),
                    "multishot disarmed; re-arming is the caller's job"
                );
                let bid = cqueue::buffer_select(completion.flags())
                    .expect("no buffer id on a provided-buffer completion");
                // Data is already in the kernel-chosen buffer: a real server
                // parses here, then returns it.
                buf_ring.give_back(bid);
                received += 1;
            }
            buf_ring.publish();
        }
        if counted {
            cpu += thread_cpu() - cpu_start;
        }
    }

    peers.finish();
    report("multishot recv + provided buffers (0 syscalls)", cpu);
}

/// Rung 4: the library, on both of its read paths.
///
/// The unbuffered path speculates and falls back to readiness. The buffered
/// path owns its receive buffer, which is what makes a completion read sound,
/// so only it can take one.
///
/// One task per connection, all parked in `read` before the writer speaks --
/// the same regime as the hand-written rungs. Reading the connections in a
/// loop from a single task is *not* the same thing: by the time such a loop
/// reaches connection 200 the data is already there, the speculation succeeds,
/// and the rung silently measures the easy case.
fn run_glommio(buffered: bool) {
    use futures_lite::io::AsyncReadExt;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let (write_now, when) = mpsc::channel::<()>();
    let (armed_tx, armed) = mpsc::channel::<()>();

    let writer = std::thread::spawn(move || {
        pin(CPU_CLIENT);
        std::thread::sleep(Duration::from_millis(200));
        let mut clients: Vec<TcpStream> = (0..CONNECTIONS)
            .map(|_| {
                let stream = TcpStream::connect(addr).unwrap();
                stream.set_nodelay(true).unwrap();
                stream
            })
            .collect();
        while when.recv().is_ok() {
            for client in clients.iter_mut() {
                client.write_all(MSG).unwrap();
            }
        }
    });

    let cpu = glommio::LocalExecutorBuilder::new(glommio::Placement::Fixed(CPU_SERVER))
        .spawn(move || async move {
            let listener = glommio::net::TcpListener::bind(addr).unwrap();
            let mut streams = Vec::with_capacity(CONNECTIONS);
            for _ in 0..CONNECTIONS {
                let stream = listener.accept().await.unwrap();
                streams.push(if buffered {
                    Reader::Buffered(stream.buffered())
                } else {
                    Reader::Plain(stream)
                });
            }
            armed_tx.send(()).unwrap();

            // Each reader reports every message it takes, so the driver knows
            // when all of them are parked again and it is safe to ask for the
            // next round.
            let (done, finished) = glommio::channels::local_channel::new_unbounded();
            let done = std::rc::Rc::new(done);
            let readers: Vec<_> = streams
                .into_iter()
                .map(|mut reader| {
                    let done = done.clone();
                    glommio::spawn_local(async move {
                        let mut buf = [0u8; BUF_LEN];
                        for _ in 0..WARMUP + ROUNDS {
                            let read = match &mut reader {
                                Reader::Plain(stream) => stream.read(&mut buf).await.unwrap(),
                                Reader::Buffered(stream) => stream.read(&mut buf).await.unwrap(),
                            };
                            assert_eq!(read, MSG.len());
                            done.try_send(()).unwrap();
                        }
                    })
                    .detach()
                })
                .collect();
            drop(done);

            let mut cpu = Duration::ZERO;
            let mut counted_from = None;
            for round in 0..WARMUP + ROUNDS {
                if round == WARMUP {
                    counted_from = Some(thread_cpu());
                }
                write_now.send(()).unwrap();
                for _ in 0..CONNECTIONS {
                    finished.recv().await.unwrap();
                }
            }
            cpu += thread_cpu() - counted_from.unwrap();

            drop(write_now);
            for reader in readers {
                reader.await;
            }
            cpu
        })
        .unwrap()
        .join()
        .unwrap();

    armed.recv().unwrap();
    let _ = writer.join();
    report(
        if buffered {
            "glommio TcpStream::read, buffered"
        } else {
            "glommio TcpStream::read, unbuffered"
        },
        cpu,
    );
}

enum Reader {
    Plain(glommio::net::TcpStream),
    Buffered(glommio::net::TcpStream<glommio::net::Preallocated>),
}

/// The other regime: one connection with data always waiting.
///
/// This is where speculation earns its keep, and where turning every read
/// into a submission would be a regression. Reported per 64 KiB read rather
/// than per message.
fn run_glommio_streaming(buffered: bool) {
    use futures_lite::io::AsyncReadExt;

    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 4096;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let writer = std::thread::spawn(move || {
        pin(CPU_CLIENT);
        let mut client = TcpStream::connect(addr).unwrap();
        let payload = vec![b'x'; CHUNK];
        for _ in 0..CHUNKS {
            client.write_all(&payload).unwrap();
        }
    });

    let (stream, _) = listener.accept().unwrap();
    let fd = stream.as_raw_fd();
    std::mem::forget(stream);

    let cpu = glommio::LocalExecutorBuilder::new(glommio::Placement::Fixed(CPU_SERVER))
        .spawn(move || async move {
            let stream = unsafe {
                <glommio::net::TcpStream as std::os::unix::io::FromRawFd>::from_raw_fd(fd)
            };
            let mut reader = if buffered {
                Reader::Buffered(stream.buffered())
            } else {
                Reader::Plain(stream)
            };

            let mut buf = vec![0u8; CHUNK];
            let mut taken = 0usize;
            let mut cpu = Duration::ZERO;
            let target = CHUNK * CHUNKS;
            let mut counted_from = None;

            while taken < target {
                if counted_from.is_none() && taken > target / 8 {
                    counted_from = Some(thread_cpu()); // warm-up discarded
                }
                let read = match &mut reader {
                    Reader::Plain(stream) => stream.read(&mut buf).await.unwrap(),
                    Reader::Buffered(stream) => stream.read(&mut buf).await.unwrap(),
                };
                if read == 0 {
                    break;
                }
                taken += read;
            }
            if let Some(start) = counted_from {
                cpu += thread_cpu() - start;
            }
            (cpu, taken)
        })
        .unwrap()
        .join()
        .unwrap();

    let _ = writer.join();
    let (cpu, taken) = cpu;
    println!(
        "{:<48} {:>8.0} ns cpu / 64KiB",
        if buffered {
            "streaming, buffered"
        } else {
            "streaming, unbuffered"
        },
        cpu.as_nanos() as f64 / (taken as f64 / CHUNK as f64)
    );
}

fn main() {
    pin(CPU_SERVER);
    println!("{MESSAGES} messages across {CONNECTIONS} connections, data arriving after the read is posted\n");
    run_readiness();
    run_single_shot();
    run_hybrid();
    run_multishot();
    run_glommio(false);
    run_glommio(true);
    println!();
    run_glommio_streaming(false);
    run_glommio_streaming(true);
}
