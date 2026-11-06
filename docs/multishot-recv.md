# Multishot receive for buffered sockets

## The problem it solves

A buffered `TcpStream` or `UnixStream` reads the socket in one of two ways.
Readiness: a non-blocking `recv`, a `POLL_ADD` when that returns `EAGAIN`, and a
second `recv` when the poll completes. Completion: lend the stream's buffer to
the kernel and submit one `RECV` into it. Either way every message costs the
reactor one submission, and the two paths disagree on a property that matters:

- With the completion read, a reply that is already in the socket when the task
  comes back to read it is only seen after the reactor has submitted and reaped.
  Beside a CPU-bound task in the same queue that is a preempt slice away. A
  speculative `recv` before the submission hides this, at one wasted syscall
  per message whenever the data has not arrived yet, which under request and
  response is always.
- Measured on a PostgreSQL client at depth 1: removing the speculation saved
  the syscall and moved throughput by nothing; the submission path is 18% of
  the client's user time either way; and the same change took the p99 of a
  sixteen-way pipeline from 0.36 ms to 0.64 ms, because a reply already in the
  socket went through a reactor pass.

Multishot receive takes both costs away. One `RECV` with `IORING_RECV_MULTISHOT`
is armed per socket for its lifetime. The kernel picks a buffer from a ring the
executor registered, copies each arrival into it, and posts a completion. There
is nothing to speculate, nothing to submit per message, and a reply that was
already waiting is a completion already in the queue, which the stream can take
without a syscall.

## Premise, measured

`examples/recv_ladder.rs`, 256 connections with every reader parked before the
data arrives, CPU on the reading thread per message, three runs on a
Threadripper PRO 9975WX, Linux 7.3:

| rung | run 1 | run 2 | run 3 |
|---|---|---|---|
| readiness, by hand: recv, PollAdd, recv | 1138 ns | 1075 ns | 1136 ns |
| single-shot RECV, by hand: one SQE, one CQE | 803 ns | 774 ns | 797 ns |
| multishot RECV with a provided buffer ring, no syscalls | 665 ns | 632 ns | 658 ns |
| glommio `TcpStream::read`, buffered, completion path | 1102 ns | 1106 ns | 1123 ns |

Multishot is 17% under the single-shot primitive and 42% under the readiness
one. The library spends about 300 ns on top of its primitive, so a buffered read
around 950 ns is the realistic landing for this regime, with the per-message
submission gone altogether in request and response.

## Measured, the library

The same ladder, rung 4 buffered, the completion-read base against this
branch, three interleaved runs each, CPU on the reading thread per message:

| arm | run 1 | run 2 | run 3 |
|---|---|---|---|
| completion read, the base | 1102 ns | 1107 ns | 1109 ns |
| multishot | 931 ns | 921 ns | 932 ns |

16% less per message, where the design above predicted about 950 ns. The
streaming rung moved from 2833 ns to 2742 ns per 64 KiB, 3% better, and the
unbuffered rung did not move, as it keeps its path.

## Acceptance

Two cells of the PostgreSQL client's scoreboard, both against the main this
branch was cut from, same script and box as the measurements above:

- depth-1 `select-only prepared 1 × 1`: throughput, and syscalls per
  transaction (today 1 `sendto`, 1 `recvfrom` returning `EAGAIN`, 3.3
  `io_uring_enter`);
- `pipeline-select 1 × 16`: p99 must stay at its current 0.36 ms. This is the
  cell that catches a reply-already-waiting read going through a reactor pass.

Plus, here: `recv_ladder` rung 4 buffered against rung 3, and
`benches/socket_io.rs` for the batched regime, three interleaved runs each.

## Design

### Buffer ring, one per executor

A provided-buffer ring (`IORING_REGISTER_PBUF_RING`) registered on the latency
ring, which is where socket receives are submitted. The ring holds `count`
entries of `size` bytes each, contiguous, page-aligned, allocated once at
executor start. Giving a buffer back to the kernel is a write to the ring's
tail, no syscall.

**Both numbers are the user's.** `LocalExecutorBuilder::recv_buffers(count,
size)` and the same on `LocalExecutorPoolBuilder`. No one default fits a
proxy pushing 64 KiB frames and a key-value store answering in 40 bytes, and
the memory is per executor, so a pool of 64 shards multiplies it by 64. The
defaults, for a builder that does not say:

- `size`: 8 KiB, the buffer a buffered stream already allocates today
  (`Preallocated::DEFAULT_BUFFER_SIZE`), so the meaning of "a full fill"
  does not change for a stream that never asked for a size.
- `count`: 1024, which makes the pool 8 MiB, on the scale of the 10 MiB of
  `io_memory` an executor already sets aside. The count bounds how many
  arrivals the kernel can hold between two visits to the completion queue, not
  how many connections can be open; an idle connection holds no buffer.
- `count = 0` disables multishot and every buffered stream keeps the
  completion path.

`count` is at most 32768 and is rounded up to a power of two, both kernel
rules.

### Kernel gate

Buffer rings need 5.19, multishot receive 6.0. The startup probe already
registers a throwaway ring; it additionally tries `register_buf_ring` on it.
A kernel that accepts the registration but rejects the multishot flag answers
the first armed receive with `EINVAL`, which the reactor treats as "not on this
kernel", disarms multishot for the executor, and lets the stream fall back.
Either way a buffered stream always works; what changes is how.

### A source with many completions

Every completion today consumes its source: `process_one_event` takes the
source out of the map, stores one result, wakes the waiters. A multishot source
produces a completion per arrival and must stay in the map while the kernel
still holds the request.

`SourceType::RecvMulti` carries a queue of `(buffer id, length)` pairs. A
completion with `IORING_CQE_F_MORE` set pushes onto that queue and wakes the
waiters, leaving the source in place and the ring's in-kernel count alone. A
completion without `MORE` is the last one: the kernel has dropped the request,
for end of file, an error, `ENOBUFS` when the pool ran dry, or a cancellation.
It goes through the existing path, which consumes the source and stores the
result, after pushing any final data it carried.

A stream that finds its source gone re-arms, after returning its buffers, unless
the last result was end of file or an error, which it reports.

### The buffered stream

Only `Buffered` streams arm multishot, at `buffered()` or `buffered_with()`
time, and only when the executor has a pool. The unbuffered path is unchanged:
it has no buffer of its own to copy into and keeps readiness.

Once armed, every byte the socket produces goes to a provided buffer, so the
stream can no longer touch the socket with `recv`. The two places that did:

- the large-read bypass, where a read at least as big as the buffer went to
  the socket directly, now drains the queue into the caller's slice instead;
- `peek` drains the queue into the stream's buffer first, then peeks the socket
  for anything beyond it.

`poll_fill_buf` takes the next `(id, length)` off the queue, copies the bytes
into the stream's own buffer, and gives the kernel buffer back. One copy per
message, bounded by `size`. Handing the kernel buffer to the caller instead
would save that copy; the owned-read measurements from August put the
crossover at 4 KiB per read, so that is a follow-up for large frames, not this
change.

When the queue is empty the stream does not go to sleep yet. It asks the
reactor to consume the latency ring's completion queue right there, no
submission, no syscall: the queue is shared memory and the kernel has already
posted whatever arrived. Only if that finds nothing does the stream register
its waker and return `Pending`. This is the step that keeps a reply already in
the socket visible on the first poll, the property the pipeline p99 cell
guards.

The read timeout, cancellation of a pending read, and drop all work as they
do for the completion path: the timer is armed while the stream waits on the
source; dropping the stream drops the source, which cancels the request, and
the final completion returns any buffers still queued.

### What stays out

- `RecvMultiBundle` (6.10) delivers several buffers per completion. Worth
  measuring after this lands, not before.
- Zero-copy delivery to the caller, as above.
- Datagram sockets. `recv_from` needs the address, which multishot delivers
  only through `RECVMSG`, a separate shape.
