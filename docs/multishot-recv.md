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

The ring of its own (below) did not move the ladder: buffered 903/888/894 ns
against 899/905/909 ns for the revision before it, three interleaved runs.
What it moved is the pipelined cell: on the PostgreSQL client's
`pipeline-select 1 × 16`, three runs of five seconds, 44564/44499/44578
transactions a second at a p99 of 0.369 ms, where the readiness path does
45079 at 0.365 ms and the revision before the ring did 599 at 196 ms.
`select-only 1 × 1` went from 70046 to 77946/78664.

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

A provided-buffer ring (`IORING_REGISTER_PBUF_RING`) registered on the socket
ring, which is where multishot receives are submitted. The ring holds `count`
entries of `size` bytes each, contiguous, page-aligned, allocated once at
executor start. Giving a buffer back to the kernel is a write to the ring's
tail, no syscall.

**Both numbers are the user's, and the default is off.**
`LocalExecutorBuilder::recv_buffers(RecvBuffers)` and the same on
`LocalExecutorPoolBuilder`, where a `RecvBuffers` is stated one of three ways:
`for_connections(n).arrivals_each(d).message_size(s)` from the workload,
`budget(bytes)` from the memory to spend per executor, or `exact(count,
size)` for a workload already measured. All three resolve to the same two
numbers, readable back through `count()` and `size()` for the startup log,
and `IoStats::recv_buffers_exhausted` counts, afterwards, every receive the
kernel ended for want of a buffer, which is the signal to raise the count. No one default fits a proxy pushing 64 KiB frames
and a key-value store answering in 40 bytes, and the memory is per executor,
so a pool of 64 shards multiplies it by 64. A builder that does not say gets
no buffers and no multishot, and nothing changes for a program that upgrades.
The reason it is opt-in is in the measurements above: on a single connection
multishot buys 10% throughput with 1.5 times the CPU per transaction, by
spending the reply's latency on reactor passes instead of parked, and that
is the application's trade to make. Under fan-in, many connections into one
executor, it is a saving per message.

- `size` is the most one completion carries; a message larger than it arrives
  in several. The protocol's usual message is the right number, 8 KiB if
  unsure, which is the buffer a buffered stream already allocates for itself
  (`Preallocated::DEFAULT_BUFFER_SIZE`).
- `count` bounds how many arrivals the kernel can hold between two visits to
  the completion queue, not how many connections can be open; an idle
  connection holds no buffer. Connections times the arrivals each may have
  outstanding between two visits is the number, and it is an upper bound:
  the kernel runs deferred receives a batch at a time and buffers come back
  between batches, so 64 connections with one message each waiting took 33
  buffers, not 64. A connection nobody reads is bounded separately, below,
  so it cannot be the reason the count runs out. Running out is not fatal:
  the kernel ends the request with `ENOBUFS`, the stream takes what is there
  and re-arms, and the reader never notices, at the cost of a submission.
- `count = 0` disables multishot and every buffered stream keeps the
  completion path.

`count` is at most 32768, a kernel rule. The ring the kernel reads buffer
descriptors from must hold a power of two of entries and is sized to the one
at or above `count`; the buffers themselves are exactly `count`, so a budget
costs what it says and not up to twice that.

### A connection nobody reads

Once armed, the kernel keeps copying a socket into provided buffers whether
or not anyone reads the stream, and that empties the socket, which keeps the
peer's window open. Left alone, one connection the application has stopped
reading, which is ordinary backpressure, would take every buffer on the
executor and put every other connection on the re-arm path for each message;
raising the count would not help. So each request carries a ceiling, the
bytes its stream can take at once (its own buffer, at least two kernel
buffers), and the reactor cancels the request once the queue holds more than
that. The socket then fills, the peer's window closes, and the stream re-arms
when it drains its queue. One bundle can still overshoot by what the socket
held at that moment, which the kernel bounds by the socket's receive queue;
a per-shot length would have capped that, but the kernel then maps that many
bytes of buffers for every shot however little arrives, and drained a small
pool on the first byte.

### A ring of its own, with deferred task work

The receives live on a fourth ring, built with `IORING_SETUP_DEFER_TASKRUN`
and `IORING_SETUP_SINGLE_ISSUER`. On a plain ring the kernel runs each
arrival's receive as task work the moment it is queued, interrupting the
thread per arrival; with sixteen pipelined replies landing a few microseconds
apart that was a receive and a completion each, and nothing bundled. On a
deferred ring a poll wakeup only queues the receive, which runs when this
thread enters the ring and takes everything the socket holds by then.

Deferred work is posted only on an entry that asks for events, so every
submit on the ring carries `GETEVENTS`, and the reactor sleeps in this ring,
polling the main and latency rings' fds from there: deferred work wakes only
the ring's own submitter, never a poller of its fd, so the link cannot run the
other way. A stream that finds its queue empty may enter once between two
passes, so a reply already in the socket is seen on the first poll; a second
empty poll before the next pass enters nothing. A pass that cannot sleep
enters the ring, unless a stream entered since the last pass and no preempt
slice has gone by, since that entry ran the work; a pass that may sleep does
not enter before deciding, because the sleep's own entry runs the work and
returns at once if it posted anything, and entering first found a depth-1
reply on the second or third pass and never parked, at the cost of the whole
core.

The ring exists only when the executor asked for buffers. An executor
without them has no fourth ring and sleeps in the main ring as it always
did; building the ring regardless cost the default path 9.5% more CPU per
round trip on a depth-1 ping-pong, for a ring it never used.

The latency ring stays plain, and that is the reason for the fourth ring.
The preempt timer and the foreign-wake eventfd must interrupt a task that
never enters the kernel, which deferred work by definition cannot.

`peek` never touches the socket while a request is armed: every byte the
socket produces goes to a kernel buffer, so a peek on the socket would see
the bytes behind the ones the request has taken or is about to. It serves
what is queued, reaps what the kernel has posted, and otherwise waits on the
request. A peek may return fewer bytes than asked for.

The kernel also reports pending work in `IORING_SQ_TASKRUN`, and that flag is
never read. On 7.3 a multishot receive that re-queues itself while the work
runs leaves it set over an empty queue, and no entry clears it (the secondary
item of liburing issue 1645, unfixed in mainline at 7.4-rc2). A reactor that
took it for pending work spun at 140k entries a second and the pipeline cell
below fell from 45000 to 599 transactions a second.

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
