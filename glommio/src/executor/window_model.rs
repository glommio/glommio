//! An exhaustive, executable model of the window a preempted slice opens.
//!
//! # What it is
//!
//! A pure state machine, compiled only for tests, that restates step by
//! step what the executor does around preemption: `run`, `run_task_queues`,
//! `run_one_task_queue`, `finish_task_queue_run` with the charge
//! `account_vruntime` makes, the window in `Preempted`
//! (`preempted_inline`, `preempted_last`, `hold`, `close_window`,
//! `take_from_back`), `activation_vruntime` and `maybe_activate`, and in
//! `context.rs` and `multitask.rs` `schedule`, `requeue`,
//! `end_preempted_window`, `spawn_internal`, `with_cleanup` and the
//! scheduler's wake path, with the raw task's `do_wake`, `cancel` and `run`
//! in `task/raw.rs`, the heap order of task queues (`impl Ord for
//! TaskQueue` and `push_runnable`), and the order of
//! `react`, whose timer wakers run before the rings are drained. It
//! does not drive the executor. It explores every interleaving of the events
//! below up to a bound, breadth first with visited states deduplicated, so
//! the first violation of each property it finds is one of the shortest, and
//! prints it as an event trace.
//!
//! Events: the run loop polls the next task; the running task wakes another,
//! `spawn_local`s a child polled inline (nested up to `inline_depth`),
//! cancels a task, removes an idle queue, yields, awaits or completes; the
//! preempt timer fires at any point, including after a slice is picked and
//! before its first poll; a reactor pass wakes parked tasks in any order, or
//! runs a waker that spawns, cancels or removes a queue, before the rings
//! are drained, with a preemption still pending, or after; under the earlier
//! designs, a vruntime tie is broken either way; a shares callback, a poll, a waker or a task a waker
//! polls inline panics; `run` returns or is called again; a handle is
//! dropped between runs; the executor is dropped. A picked slice beginning
//! and a pass draining its rings are free: they do not count against the
//! bound.
//!
//! # What it checks, after every step
//!
//! - `Accounting`: a task is scheduled if and only if exactly one queue entry
//!   or window slot holds its runnable, and running if and only if its poll
//!   is on the stack.
//! - `Stranded`: outside a slice, a queue with runnables, or marked active,
//!   is in the heap, so the executor cannot go idle with work it never finds.
//! - `OutsideRun`: no window and no slice marked running outside `run`.
//! - `Bound`: let P be the tasks whose polls were on the stack when the timer
//!   fired in a slice, and W the tasks a reactor pass, or a waker it ran,
//!   scheduled between the end of that slice and the end of its window (next
//!   round, park, or `run` returning). No task of P is polled again before
//!   every task of W in its queue, or in a queue that was idle and charged no
//!   more than P's queue was once its slice was charged, ties included.
//! - `BoundTie`: the same, reported apart when the task of P was polled
//!   first only because its queue won a vruntime tie with the late task's:
//!   by entering the heap first under the current rule, or either way.
//! - `BoundAfterWindow` and `BoundAfterWindowTie`: the same, reported apart
//!   when the task of P was woken again only after its window, so the window
//!   had no say in it: by a strict reversal of vruntimes, or by a tie.
//! - `WindowOutlivesIntent`: a window exists only from the timer firing in a
//!   slice to the end of that window.
//! - `Fifo`: a queue polls the tasks the window does not mean to hold in the
//!   order they were scheduled.
//! - `UserCodeUnderBorrow`: cancelling or dropping a runnable never happens
//!   in a step the code performs with `queues` borrowed.
//! - `CodeAssertion`: the `debug_assert`s on this path hold.
//! - `SpuriousHold`: only tasks of P are held, or a task at the reused
//!   address of the last one, which the design tolerates.
//! - `InlineAddressReused`: a reused address can match only the last task.
//! - `AddressReuseOvertaken` and `AddressReuseSlices`: what that reuse
//!   costs: once released, nothing overtakes the task, and the inline tasks
//!   ahead of it spend at most one slice before it runs.
//! - `Placement`: a queue activated outside a window lands no later than just
//!   behind the least runnable vruntime, the running queue included.
//! - `WindowPlacement`: a queue activated in a window lands no later than one
//!   behind the usual place.
//!
//! # Which rules it mirrors
//!
//! `Rule::Current` is the executor's rule. Every other `Rule` restates an
//! earlier design whose bug was found in review, or a mutation of the current
//! one, and each has a test that the model finds that bug, with the property
//! and the shape of the trace asserted. A model that cannot find known bugs
//! proves nothing; these tests are what show it can.
//!
//! # How to run it
//!
//! `cargo test -p glommio --lib -- window_model` runs every test at bounds
//! that keep each one to about two seconds in a debug build. In release the
//! bounds are wider: `cargo test -p glommio --lib --release -- window_model
//! --nocapture` prints each search's state count and every trace. Deeper
//! searches of the current rule are opt in: `WINDOW_MODEL_EVENTS=16` with
//! `--release` takes a few minutes and tens of millions of states.
//!
//! # Drift
//!
//! Every item it mirrors carries the line "Modelled by the test-only
//! `executor::window_model`: mirror changes there and rerun it." in its
//! docs, and `MIRRORED` holds a fingerprint of each one's source, layout and
//! docs stripped; `task/raw.rs`'s `do_wake`, `cancel`, `run` and
//! `PollGuard`'s `Drop` included, since the model relies on their wake,
//! cancel and unwind semantics. `react`'s
//! order is not fingerprinted: a change there needs this model reviewed. `mirrored_code_is_unchanged` fails when one changes. Then
//! mirror the change here, keep every earlier rule's test passing, rerun the
//! deep searches, and update the fingerprints, which
//! `WINDOW_MODEL_PRINT_FINGERPRINTS=1` prints.
//!
//! # Assumptions, and what it does not cover
//!
//! - Up to five task addresses, three queues and three nested inline polls.
//!   The main future is abstract: `run` may return wherever it is polled.
//! - `need_preempt` is set only by the preempt timer or by a latency timer's
//!   timeout on the latency ring, both the model's Timer event, and cleared
//!   only by `react`. `react` repeats its pass when a latency timer fires
//!   inside its wait, which only adds wakes to the same pass. A reactor pass
//!   delivers its wakes in any order, and the park's own pass is folded into
//!   the next one. The idle spin pass, `spin_poll_io`, drains the
//!   rings before its timer wakers run, so in the code `need_preempt` reads
//!   false there while the model keeps it set. Nothing in a pass reads it
//!   but an inline poll, and an inline poll outside a slice opens no window;
//!   the spin pass is followed by a `react` pass that drains before any
//!   queue runs, or by the park, which ends the window. So no `Bound` or
//!   `Fifo` violation depends on it.
//! - Join handles are left out. Cancelling a task whose handle a parked task
//!   awaits schedules it and wakes that task in one call; in the model that
//!   is a cancel and then a wake of the parked task, or a cancel and then a
//!   yield when the awaiting task is the one cancelling. Every search that
//!   allows cancels allows both, one right after the other, so every such
//!   trace is in the search one event longer: that can push a `Bound` or
//!   `Fifo` violation past the bound, but hides none within it.
//! - Vruntime is coarse: a preempted slice charges three times the queue's
//!   weight, a short one once, an empty one nothing; overflow and dynamic
//!   shares are left out. The code charges an empty slice the little time it
//!   took, so where the model has an empty slice tie, the code has the
//!   queue strictly behind.
//! - Ties go to the queue that entered the heap first, as in the code; the
//!   order queues entered is kept as ranks among those in the heap, and an
//!   initial state is searched in every order its runnable queues could
//!   have entered. The earlier designs, which ran on a heap that broke ties
//!   in no particular order, and `Rule::TiesEitherWay`, are searched with
//!   ties going either way.
//! - A finished task frees its address at once, the worst case for reuse.
//!   Cancelling a running task, `spawn_into` another queue, foreign wakes,
//!   `yield_if_needed`, queue creation and multiple executors are left out.
//! - With one executor, a cleanup wake of a task the window remembers can only
//!   come after its slice, and the hold that the earlier design put it through
//!   then activated the default queue; stranding it needs a second executor.
//!   Forgetting a remembered task that another executor's context destroys
//!   needs one too, and is not modelled beyond the code path.
//! - With one executor, cleanup never runs inside a slice or while one is set
//!   up: it runs between runs and at drop. So `slice_owner`, which only
//!   cleanup during another executor's slice sets, stays empty, and a window
//!   that such cleanup opens before a slice's first poll, which the slice's
//!   end keeps with no last task, steps and names, cannot arise; neither is
//!   modelled beyond the code path that handles it. A slice that
//!   `need_preempt` ends before its first poll is modelled, and opens no
//!   window. The tripwire covers `with_cleanup`, `preempted_inline` and
//!   `preempted_last`.
//! - When a poll panics, `PollGuard` drops its future as the slice unwinds,
//!   and that destructor can wake and spawn; the futures of the polls beneath
//!   it, dropped as the unwind goes on, run nothing in the model.
//! - Which steps hold the `queues` borrow is transcribed from the code, not
//!   derived from it.
//! - It checks orderings, not time: how long a task waits in slices is
//!   bounded only by the event bound, and starvation of a low-share queue
//!   shows only as the debt it carries, not as the slices it would wait.

use std::{
    collections::{BTreeMap, HashMap},
    fmt::{self, Write as _},
    hash::{BuildHasherDefault, Hash, Hasher},
    time::{Duration, Instant},
};

const NT: usize = 5;
const NQ: usize = 3;
const NONE: u8 = u8::MAX;
const MAX_FRAMES: usize = 4;
const MAX_INLINE: usize = MAX_FRAMES - 1;
const PREEMPTED_CHARGE: u8 = 3;
const SHORT_CHARGE: u8 = 1;

/// 128-bit FNV-1a. The visited set keeps this fingerprint of each state
/// rather than the state (hash compaction), so a search of tens of millions
/// of states fits in a few hundred megabytes. Two states sharing a
/// fingerprint would hide the second, with probability about n^2 / 2^129:
/// below 1e-23 at 30 million states.
struct Fnv128(u128);

impl Default for Fnv128 {
    fn default() -> Self {
        Self(0x6c62_272e_07bb_0142_62b8_2175_6295_c58d)
    }
}

impl Hasher for Fnv128 {
    fn finish(&self) -> u64 {
        (self.0 ^ (self.0 >> 64)) as u64
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u128::from(*b);
            self.0 = self
                .0
                .wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
        }
    }
}

/// Hashes a fingerprint, which is uniform already, by folding it.
#[derive(Default)]
struct Fold(u64);

impl Hasher for Fold {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.0 ^= u64::from_le_bytes(word);
        }
    }
}

/// The fewest events each visited state was reached with.
type Seen = HashMap<u128, u8, BuildHasherDefault<Fold>>;

fn fingerprint(s: &State) -> u128 {
    let mut h = Fnv128::default();
    s.hash(&mut h);
    h.0
}

fn bit(t: u8) -> u8 {
    1 << t
}

fn bits(mask: u8) -> impl Iterator<Item = u8> {
    (0..NT as u8).filter(move |t| mask & bit(*t) != 0)
}

/// The design whose rule the model runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    /// The rule as the executor implements it, read from the code.
    Current,
    /// An earlier design: one inline slot, so with nested inline polls only the
    /// innermost is remembered.
    OneInlineSlot,
    /// (e): the task polled last is not recorded if a window is open already.
    SkipLastInWindow,
    /// (b): the window is not ended when `run` returns.
    NoClearAtExit,
    /// (f): an inline poll may open a window whenever `active_executing` is
    /// set, rather than only within a slice.
    WindowByActiveExecuting,
    /// (d): the preempt timer is not sampled before an inline poll.
    NoBeforeSample,
    /// (c): an inline task that woke itself is taken from the back of its
    /// queue when the slice ends, assumed still to be last.
    TakeInlineAtSliceEnd,
    /// (g): one slot; recording the last task displaces a held inline task,
    /// which is cancelled while the queues are borrowed.
    OneSlotCancelUnderBorrow,
    /// (b), the other half: the window is not ended before the executor
    /// parks.
    NoClearAtPark,
    /// A queue whose only runnable is held is not kept runnable.
    NoKeepHeldQueueActive,
    /// One slot, and a displaced runnable is dropped rather than queued.
    OneSlotDropDisplaced,
    /// `requeue` queues a runnable without activating its queue.
    RequeueWithoutActivate,
    /// `hold` holds a runnable without activating its queue.
    HoldWithoutActivate,
    /// A queue whose only runnable is held is marked active but not pushed
    /// back into the heap.
    KeepActiveWithoutRepush,
    /// A queue woken in the window is placed as usual, just behind the least
    /// vruntime: the first commit of the fix without the second.
    UsualActivation,
    /// The window releases its held runnables at the front of their queues.
    /// It records finished inline children as the earlier designs did: with only `last`
    /// reusable, a task at a reused address is released last and so first,
    /// and nothing could overtake it.
    RequeueAtFront,
    /// An earlier design: an inline child is recorded even if its first poll finished
    /// it, so its address can be taken by a sibling spawned later in the
    /// slice.
    RecordFinishedChildren,
    /// An earlier design: the slice is marked running before the guard that ends it is
    /// armed, so a panic in a shares callback leaves it marked.
    GuardAfterPrepare,
    /// An earlier design: the held-only queue check runs only when `need_preempt`
    /// ended the slice, not when it unwinds.
    HoldsCheckOnlyWhenPreempted,
    /// An earlier design: closing the window leaves the cached least vruntime as it
    /// was, above a queue the window placed by its charge.
    NoBaselineRefresh,
    /// An earlier design: the runnable of a task whose queue is gone goes to
    /// the default queue through `schedule`, so a window that remembers the
    /// task holds it, counted against the removed queue.
    CleanupThroughHold,
    /// An earlier design: the queue whose slice opened the window, reactivated in it,
    /// is placed like the queues the window woke, and can tie with one.
    PreemptedQueueLikeWoken,
    /// The queue whose slice opened the window, reactivated in it, keeps its
    /// own charge, however much a slice cost it.
    PreemptedQueueUnclamped,
    /// An earlier design: the queue whose slice opened the window,
    /// reactivated in it, is placed at its own charge between bounds one
    /// behind the others', so it ties with a woken queue charged the same.
    PreemptedQueueAtItsCharge,
    /// An earlier design: the slice end charges no step past the slice, and
    /// a reactivated preempted queue is placed one behind its own charge
    /// instead. One that stays runnable, its task having yielded, keeps its
    /// bare charge and ties with a woken queue charged the same.
    NoSliceEndStep,
    /// The current rule as it was before queues of equal vruntime ran in the
    /// order they entered the heap: the heap breaks a tie either way.
    TiesEitherWay,
    /// The current rule with a tie going to the queue that entered the heap
    /// last.
    LastEnteredFirst,
}

/// How the heap of runnable queues breaks a vruntime tie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ties {
    /// Either way: the search takes both.
    EitherWay,
    /// To the queue that entered the heap first, as `impl Ord for TaskQueue`
    /// does.
    FirstEntered,
    /// To the queue that entered it last.
    LastEntered,
}

/// A property the model checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Property {
    Accounting,
    Stranded,
    OutsideRun,
    Bound,
    /// As `Bound`, but the preempted task's queue was picked over a late
    /// task's queue on a vruntime tie: it entered the heap first, or, under
    /// a rule that breaks ties either way, the heap picked it.
    BoundTie,
    /// As `Bound`, but the preempted task was woken again only after its
    /// window ended, so the window had no say in where its queue went.
    BoundAfterWindow,
    /// As `BoundAfterWindow`, but the preempted task's queue won a vruntime
    /// tie with the late task's.
    BoundAfterWindowTie,
    WindowOutlivesIntent,
    Fifo,
    UserCodeUnderBorrow,
    CodeAssertion,
    SpuriousHold,
    /// A task held through address reuse is overtaken, after the window
    /// releases it, by a task of its queue that was not ahead of it then, or
    /// by one that was ahead of it polled twice.
    AddressReuseOvertaken,
    /// A task held through address reuse waits more than one slice of the
    /// inline tasks released ahead of it.
    AddressReuseSlices,
    /// A reused address matched an inline entry: the window remembered a
    /// task polled inline that has since been freed.
    InlineAddressReused,
    /// A queue activated outside a window is placed behind the least
    /// vruntime of the runnable queues, the one running included, plus one.
    Placement,
    /// A queue activated in a window is placed further back than just
    /// behind the usual place, carrying a debt an idle queue would not.
    WindowPlacement,
}

/// Something reachable that is not a violation but is worth a trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Note {
    /// A task was held at the address of a task in P that had been freed.
    AddressReuseHold,
    /// A `Bound` violation whose late task was held through address reuse.
    AddressReuseBound,
    /// A waker spawned a task before the rings were drained, with the
    /// preempt completion of a preempted slice still pending and its window
    /// open.
    PreDrainSpawn,
    /// `need_preempt` ended a slice before it polled any task.
    PreemptedBeforeFirstPoll,
    /// A panicked poll's future, dropped by `PollGuard` while its slice
    /// unwinds, spawned a task.
    UnwindSpawn,
    /// A task the open window remembers was woken after its queue was
    /// removed, and went to cleanup.
    RememberedCleanup,
    /// A state queued for the next level was reached again, by free events,
    /// within the current one: a search that kept the first arrival would
    /// have explored it with one event less to spare.
    ReachedCheaper,
}

const NOTE_ABA: u8 = 1;
const NOTE_ABA_BOUND: u8 = 2;
const NOTE_PRE_DRAIN_SPAWN: u8 = 4;
const NOTE_PREEMPTED_BEFORE_POLL: u8 = 8;
const NOTE_UNWIND_SPAWN: u8 = 16;
const NOTE_REMEMBERED_CLEANUP: u8 = 32;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
enum St {
    #[default]
    Free,
    Parked,
    Scheduled,
    Running,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
struct Task {
    st: St,
    home: u8,
    /// Woken while its poll was running: scheduled when the poll returns.
    deferred: bool,
    /// Cancelled (`CLOSED`): its next run drops the future instead.
    closed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Queue {
    exists: bool,
    runnables: [u8; NT],
    len: u8,
    active: bool,
    in_heap: bool,
    v: u8,
    /// `TaskQueue::entry`, as a rank among the queues in the heap: lower
    /// entered it earlier. Zero outside the heap, and for every queue under a
    /// rule that breaks ties either way.
    entry: u8,
    /// Vruntime charged per unit of runtime: the reciprocal of its shares,
    /// relative to the other queues.
    weight: u8,
}

impl Queue {
    fn new(exists: bool, v: u8) -> Self {
        Self {
            exists,
            runnables: [NONE; NT],
            len: 0,
            active: false,
            in_heap: false,
            v,
            entry: 0,
            weight: 1,
        }
    }

    fn items(&self) -> &[u8] {
        &self.runnables[..self.len as usize]
    }

    fn push_back(&mut self, t: u8) -> bool {
        if self.len as usize == NT {
            return false;
        }
        self.runnables[self.len as usize] = t;
        self.len += 1;
        true
    }

    fn pop_front(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let t = self.runnables[0];
        self.runnables.copy_within(1..self.len as usize, 0);
        self.len -= 1;
        self.runnables[self.len as usize] = NONE;
        Some(t)
    }

    fn remove(&mut self, t: u8) -> bool {
        let Some(i) = self.items().iter().position(|x| *x == t) else {
            return false;
        };
        self.runnables.copy_within(i + 1..self.len as usize, i);
        self.len -= 1;
        self.runnables[self.len as usize] = NONE;
        true
    }

    fn back(&self) -> Option<u8> {
        self.items().last().copied()
    }

    fn pop_back(&mut self) {
        if self.len > 0 {
            self.len -= 1;
            self.runnables[self.len as usize] = NONE;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Held {
    task: u8,
    runnable: bool,
}

/// The code's `Preempted`: the `inline` list, innermost first, `last`, and
/// the least vruntime as the slice began.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Window {
    inline: [Held; MAX_INLINE],
    n_inline: u8,
    last: Option<Held>,
    v: u8,
    /// The queue whose slice opened it.
    q: u8,
}

impl Window {
    fn new(v: u8, q: u8) -> Self {
        Self {
            inline: [Held {
                task: NONE,
                runnable: false,
            }; MAX_INLINE],
            n_inline: 0,
            last: None,
            v,
            q,
        }
    }

    fn inline(&self) -> &[Held] {
        &self.inline[..self.n_inline as usize]
    }

    fn push_inline(&mut self, held: Held) {
        self.inline[self.n_inline as usize] = held;
        self.n_inline += 1;
    }

    /// Takes the first inline entry, for the single-slot designs.
    fn take_inline(&mut self) -> Option<Held> {
        if self.n_inline == 0 {
            return None;
        }
        let first = self.inline[0];
        self.inline.copy_within(1..self.n_inline as usize, 0);
        self.n_inline -= 1;
        self.inline[self.n_inline as usize] = Held {
            task: NONE,
            runnable: false,
        };
        Some(first)
    }

    /// `Preempted::slot`: the inline list in order, then `last`.
    fn slot_mut(&mut self, t: u8) -> Option<&mut Held> {
        let n = self.n_inline as usize;
        self.inline[..n]
            .iter_mut()
            .chain(self.last.as_mut())
            .find(|h| h.task == t)
    }

    /// The held runnables in release order: the inline list, then `last`.
    fn held(&self) -> impl Iterator<Item = u8> + '_ {
        self.inline()
            .iter()
            .copied()
            .chain(self.last)
            .filter(|h| h.runnable)
            .map(|h| h.task)
    }

    /// `Preempted::forget`: drops the entries of `t` that hold nothing.
    fn forget(&mut self, t: u8) {
        let n = self.n_inline as usize;
        let kept: Vec<Held> = self.inline[..n]
            .iter()
            .copied()
            .filter(|h| h.task != t || h.runnable)
            .collect();
        for (i, h) in self.inline.iter_mut().enumerate() {
            *h = kept.get(i).copied().unwrap_or(Held {
                task: NONE,
                runnable: false,
            });
        }
        self.n_inline = kept.len() as u8;
        if self.last.is_some_and(|h| h.task == t && !h.runnable) {
            self.last = None;
        }
    }

    fn refers_to(&self, t: u8) -> bool {
        self.inline()
            .iter()
            .chain(self.last.as_ref())
            .any(|h| h.task == t)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
enum Kind {
    /// Polled by the run loop.
    #[default]
    RunLoop,
    /// Polled inline by `spawn_local`; `was` is the timer sampled before.
    Inline { was: bool },
    /// A cancelled runnable run, or a runnable dropped: its future's
    /// destructor, which can wake and spawn.
    Dtor,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Frame {
    task: u8,
    kind: Kind,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            task: NONE,
            kind: Kind::RunLoop,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum PassKind {
    /// `react`, at the loop top: clears `need_preempt` as it rearms.
    React,
    /// `spin_poll_io` on the idle path, before parking.
    Spin,
}

/// Where the run loop resumes once the poll stack empties.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
enum Base {
    #[default]
    Slice,
    Pass {
        kind: PassKind,
        woke: bool,
    },
    Outside,
    Dropping,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Pc {
    Outside,
    Dropping,
    /// A reactor pass. Before it has `started`, the main future is polled,
    /// so `run` may return.
    Pass {
        kind: PassKind,
        started: bool,
        woke: bool,
    },
    /// `run_task_queues`, stopped on a vruntime tie.
    Round,
    /// A slice that has been chosen, before its first poll: the preempt
    /// timer can fire here, so that the slice ends having polled nothing, as
    /// `run_one_task_queue` checks `need_preempt` before its first
    /// `get_task`, and a shares callback can panic. Beginning the slice is a
    /// free event.
    SliceStart,
    /// The run loop of a slice, between polls: never a stopping point.
    Slice,
    /// A task is being polled; the top frame acts.
    Poll,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
struct Budget {
    /// Panics: in a shares callback as a slice starts, or in a poll.
    panics: u8,
    spawns: u8,
    cancels: u8,
    removes: u8,
    runs: u8,
    may_drop: bool,
}

/// State kept only to state the properties.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Ghost {
    /// The preempt timer fired in a slice and the window it opens has not
    /// ended.
    open: bool,
    a_v0: u8,
    /// The preempted queue's vruntime once its slice is charged.
    a_v1: u8,
    p: u8,
    w: u8,
    /// Addresses of tasks in P that have been freed.
    pdead: u8,
    /// `pairs[p]` has bit `w` if `w` must be polled before `p` is again.
    pairs: [u8; NT],
    /// Tasks held through address reuse in the open window.
    aba: u8,
    /// Bit `w` of `aba_pairs[p]`: the pair `(p, w)` was created by `w`'s
    /// wake that a reused address held, the one obligation the design
    /// exempts.
    aba_pairs: [u8; NT],
    /// Tasks with pending pairs last scheduled outside their window.
    late: u8,
    /// Per queue, the tasks scheduled while no window was intended, in order.
    fifo: [[u8; NT]; NQ],
    fifo_len: [u8; NQ],
    /// Tasks held through address reuse that a window has released and that
    /// have not been polled since.
    reused: u8,
    /// For each of those: the queue it was released into, the tasks ahead
    /// of it then, those of them released from the inline list, and the
    /// slices those inline tasks have ended preempted since.
    reused_queue: [u8; NT],
    reused_ahead: [u8; NT],
    reused_inline: [u8; NT],
    reused_slices: [u8; NT],
}

impl Default for Ghost {
    fn default() -> Self {
        Self {
            open: false,
            a_v0: 0,
            a_v1: 0,
            p: 0,
            w: 0,
            pdead: 0,
            pairs: [0; NT],
            aba: 0,
            aba_pairs: [0; NT],
            late: 0,
            fifo: [[NONE; NT]; NQ],
            fifo_len: [0; NQ],
            reused: 0,
            reused_queue: [0; NT],
            reused_ahead: [0; NT],
            reused_inline: [0; NT],
            reused_slices: [0; NT],
        }
    }
}

impl Ghost {
    fn fifo_push(&mut self, q: u8, t: u8) {
        let q = q as usize;
        if (self.fifo_len[q] as usize) < NT {
            self.fifo[q][self.fifo_len[q] as usize] = t;
            self.fifo_len[q] += 1;
        }
    }

    /// Removes `t` from whichever list holds it, returning the queue and the
    /// position it had.
    fn fifo_remove(&mut self, t: u8) -> Option<(u8, usize)> {
        for q in 0..NQ {
            let len = self.fifo_len[q] as usize;
            if let Some(i) = self.fifo[q][..len].iter().position(|x| *x == t) {
                self.fifo[q].copy_within(i + 1..len, i);
                self.fifo_len[q] -= 1;
                self.fifo[q][len - 1] = NONE;
                return Some((q as u8, i));
            }
        }
        None
    }
}

/// The modelled executor.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct State {
    tasks: [Task; NT],
    queues: [Queue; NQ],
    pc: Pc,
    stack: [Frame; MAX_FRAMES],
    depth: u8,
    base: Base,
    preempt: bool,
    in_slice: bool,
    executing: u8,
    window: Option<Window>,
    default_v: u8,
    cur: u8,
    least: u8,
    last: u8,
    /// The queues this slice's queue was tied with when it was picked.
    tied: u8,
    /// The stack index of a panicked poll whose future's destructor is
    /// running as `PollGuard` drops it, before the unwind goes on; `NONE`
    /// when nothing is unwinding.
    unwind_at: u8,
    /// Whether this `react` pass has drained the rings, clearing
    /// `need_preempt`. Its timer wakers run before the drain, its I/O
    /// completions after.
    drained: bool,
    budget: Budget,
    ghost: Ghost,
}

/// One event: a choice the environment, the tasks or the scheduler makes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Ev {
    StartRun,
    Exit,
    ExecutorDrop,
    OutsideWake(u8),
    OutsideCancel(u8),
    DropNext(u8),
    PassWake(u8),
    PassSpawn(u8),
    PassCancel(u8),
    PassRemove(u8),
    PassEnd,
    Pop(u8),
    Timer,
    Wake(u8),
    Spawn(u8),
    Cancel(u8),
    Remove(u8),
    Yield,
    Park,
    Complete,
    Done,
    Begin,
    Drain,
    Panic,
}

impl Ev {
    /// Whether the event costs nothing against the bound. A slice that has
    /// been picked begins unless something happens first; counting that as
    /// an event would only lengthen every trace by one per slice.
    fn is_free(self) -> bool {
        matches!(self, Ev::Begin | Ev::Drain)
    }
}

impl fmt::Display for Ev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ev::StartRun => write!(f, "run() is called"),
            Ev::Exit => write!(f, "the main future is ready, run() returns"),
            Ev::ExecutorDrop => write!(f, "the executor is dropped"),
            Ev::OutsideWake(u) => write!(f, "t{u} is woken"),
            Ev::OutsideCancel(u) => write!(f, "t{u}'s handle is dropped, cancelling it"),
            Ev::DropNext(u) => write!(f, "shutdown drops t{u}"),
            Ev::PassWake(u) => write!(f, "an event wakes t{u}"),
            Ev::PassSpawn(a) => write!(f, "a waker spawn_locals t{a}, polled inline"),
            Ev::PassCancel(u) => write!(f, "a waker cancels t{u}"),
            Ev::PassRemove(q) => write!(f, "a waker removes q{q}"),
            Ev::PassEnd => write!(f, "the pass ends"),
            Ev::Pop(q) => write!(f, "q{q} wins a vruntime tie"),
            Ev::Timer => write!(f, "the preempt timer fires"),
            Ev::Wake(u) => write!(f, "wakes t{u}"),
            Ev::Spawn(a) => write!(f, "spawn_locals t{a}, polled inline"),
            Ev::Cancel(u) => write!(f, "cancels t{u}"),
            Ev::Remove(q) => write!(f, "removes q{q}"),
            Ev::Yield => write!(f, "yields: wakes itself and returns Pending"),
            Ev::Park => write!(f, "returns Pending, awaiting an event"),
            Ev::Complete => write!(f, "completes"),
            Ev::Done => write!(f, "the destructor returns"),
            Ev::Begin => write!(f, "the slice begins"),
            Ev::Drain => write!(f, "the rings are drained, need_preempt cleared"),
            Ev::Panic => write!(f, "panics, unwinding out of run()"),
        }
    }
}

/// A violation, with what broke.
#[derive(Clone, Debug)]
struct Violation {
    property: Property,
    message: String,
}

/// The bounds and the initial states of one search.
#[derive(Clone, Debug)]
struct Bounds {
    max_events: usize,
    inline_depth: u8,
    /// Whether a polled task may wake another task.
    task_wakes: bool,
    /// Whether a polled task may yield, waking itself.
    yields: bool,
    /// Whether the preempt timer may fire between a slice being picked and
    /// its first poll, ending a slice that the model charges nothing.
    early_preempt: bool,
    /// Whether to keep exploring from a state reached by a violation, to see
    /// what it leads to.
    continue_past: bool,
}

/// What one search found.
#[derive(Debug)]
struct Report {
    rule: Rule,
    states: usize,
    transitions: usize,
    /// States reached again within a level with fewer events than the
    /// arrival that queued them.
    cheaper: usize,
    elapsed: Duration,
    found: BTreeMap<Property, (Violation, Vec<String>)>,
    notes: BTreeMap<Note, Vec<String>>,
}

/// The events a printed trace counts: its numbered lines.
fn events_in(trace: &[String]) -> usize {
    trace
        .iter()
        .filter(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        .count()
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "rule {:?}: {} states, {} transitions, {} reached cheaper, {:.2?}",
            self.rule, self.states, self.transitions, self.cheaper, self.elapsed
        )?;
        if self.found.is_empty() {
            writeln!(f, "  no violation found within bounds")?;
        }
        for (property, (violation, trace)) in &self.found {
            writeln!(
                f,
                "  {property:?} after {} events: {}",
                events_in(trace),
                violation.message
            )?;
            for line in trace {
                writeln!(f, "    {line}")?;
            }
        }
        for (note, trace) in &self.notes {
            writeln!(f, "  note {note:?} after {} events:", events_in(trace))?;
            for line in trace {
                writeln!(f, "    {line}")?;
            }
        }
        Ok(())
    }
}

/// One task of an initial state.
#[derive(Clone, Copy, Debug)]
struct Seed {
    home: u8,
    queued: bool,
}

/// Builds an initial state: the queues by their vruntimes, the tasks queued
/// or parked, at the loop top inside `run` or outside it.
fn initial(queue_v: &[u8], seeds: &[Seed], inside_run: bool, budget: Budget) -> State {
    let mut queues = [Queue::new(false, 0); NQ];
    for (q, v) in queue_v.iter().enumerate() {
        queues[q] = Queue::new(true, *v);
    }
    let mut tasks = [Task::default(); NT];
    let mut ghost = Ghost::default();
    for (t, seed) in seeds.iter().enumerate() {
        tasks[t] = Task {
            st: if seed.queued {
                St::Scheduled
            } else {
                St::Parked
            },
            home: seed.home,
            deferred: false,
            closed: false,
        };
        if seed.queued {
            let entered = queues.iter().filter(|q| q.in_heap).count() as u8;
            let q = &mut queues[seed.home as usize];
            q.push_back(t as u8);
            q.active = true;
            if !q.in_heap {
                q.entry = entered;
            }
            q.in_heap = true;
            ghost.fifo_push(seed.home, t as u8);
        }
    }
    let default_v = queues
        .iter()
        .filter(|q| q.in_heap)
        .map(|q| q.v)
        .min()
        .unwrap_or(0);
    let mut s = State {
        tasks,
        queues,
        pc: if inside_run {
            Pc::Pass {
                kind: PassKind::React,
                started: false,
                woke: false,
            }
        } else {
            Pc::Outside
        },
        stack: [Frame::default(); MAX_FRAMES],
        depth: 0,
        base: Base::default(),
        preempt: false,
        in_slice: false,
        executing: NONE,
        window: None,
        default_v,
        cur: 0,
        least: 0,
        last: NONE,
        tied: 0,
        unwind_at: NONE,
        drained: false,
        budget,
        ghost,
    };
    canonicalize(&mut s, true);
    s
}

/// Clears what no longer matters and shifts vruntimes so the least is zero,
/// so that states differing only there are visited once. Every vruntime
/// operation is a comparison, a min, a clamp or a small addition, all of
/// which commute with the shift. Heap entries are only ever compared, so
/// they are kept as ranks among the queues in the heap, or cleared when
/// `entries` is false, under a rule that breaks ties either way.
fn canonicalize(s: &mut State, entries: bool) {
    if !s.in_slice {
        s.cur = 0;
        s.least = 0;
        s.last = NONE;
        s.tied = 0;
    }
    let in_react = matches!(
        s.pc,
        Pc::Pass {
            kind: PassKind::React,
            ..
        }
    ) || (s.pc == Pc::Poll
        && matches!(
            s.base,
            Base::Pass {
                kind: PassKind::React,
                ..
            }
        ));
    if !in_react {
        s.drained = false;
    }
    if !s.ghost.open {
        s.ghost.a_v0 = 0;
        s.ghost.a_v1 = 0;
        s.ghost.p = 0;
        s.ghost.w = 0;
        s.ghost.pdead = 0;
    }
    if s.depth == 0 {
        s.base = Base::default();
    }
    for f in &mut s.stack[s.depth as usize..] {
        *f = Frame::default();
    }
    for q in &mut s.queues {
        if !q.exists {
            *q = Queue::new(false, 0);
        }
    }
    let ranks = s.queues.map(|q| {
        s.queues
            .iter()
            .filter(|o| o.in_heap && o.entry < q.entry)
            .count() as u8
    });
    for (q, rank) in s.queues.iter_mut().zip(ranks) {
        q.entry = if entries && q.in_heap { rank } else { 0 };
    }
    let mut m = s.default_v;
    for q in s.queues.iter().filter(|q| q.exists) {
        m = m.min(q.v);
    }
    if let Some(w) = s.window {
        m = m.min(w.v);
    }
    if s.in_slice {
        m = m.min(s.least);
    }
    if s.ghost.open {
        m = m.min(s.ghost.a_v0).min(s.ghost.a_v1);
    }
    s.default_v -= m;
    for q in s.queues.iter_mut().filter(|q| q.exists) {
        q.v -= m;
    }
    if let Some(w) = s.window.as_mut() {
        w.v -= m;
    }
    if s.in_slice {
        s.least -= m;
    }
    if s.ghost.open {
        s.ghost.a_v0 -= m;
        s.ghost.a_v1 -= m;
    }
}

/// The model of one rule.
struct Model {
    rule: Rule,
    bounds: Bounds,
}

/// One step being applied.
struct Exec<'m> {
    m: &'m Model,
    s: State,
    /// Every property the step breaks, each once, with its first message.
    viol: Vec<Violation>,
    notes: u8,
}

impl Exec<'_> {
    fn fail(&mut self, property: Property, message: impl FnOnce() -> String) {
        if self.viol.iter().all(|v| v.property != property) {
            self.viol.push(Violation {
                property,
                message: message(),
            });
        }
    }

    fn on_stack(&self, t: u8) -> bool {
        self.s.stack[..self.s.depth as usize]
            .iter()
            .any(|f| f.task == t)
    }

    fn entered(&self) -> bool {
        !(self.s.pc == Pc::Outside && self.s.depth == 0)
    }

    fn exists(&self, q: u8) -> bool {
        (q as usize) < NQ && self.s.queues[q as usize].exists
    }

    fn dest(&self, t: u8) -> u8 {
        let home = self.s.tasks[t as usize].home;
        if self.exists(home) {
            home
        } else {
            0
        }
    }

    fn outside_run(&self) -> bool {
        match self.s.pc {
            Pc::Outside | Pc::Dropping => true,
            Pc::Poll => matches!(self.s.base, Base::Outside | Base::Dropping),
            _ => false,
        }
    }

    fn user_code(&mut self, borrowed: bool, what: &'static str) {
        if borrowed {
            self.fail(Property::UserCodeUnderBorrow, || {
                format!("{what} while the queues are borrowed")
            });
        }
    }

    /// `ExecutorQueues::activation_vruntime`.
    fn activation_v(&self, q: usize) -> u8 {
        let usual = self.s.default_v.saturating_add(1);
        match self.s.window {
            _ if self.m.rule == Rule::UsualActivation => usual,
            None => usual,
            Some(w) => {
                let woken = w.v.min(self.s.default_v).saturating_add(1);
                let own = self.s.queues[q].v;
                if w.q as usize == q {
                    match self.m.rule {
                        Rule::PreemptedQueueLikeWoken => own.clamp(woken, usual),
                        Rule::PreemptedQueueUnclamped => own,
                        Rule::NoSliceEndStep => own
                            .saturating_add(1)
                            .clamp(woken.saturating_add(1), usual.saturating_add(1)),
                        _ => own.clamp(woken.saturating_add(1), usual.saturating_add(1)),
                    }
                } else {
                    own.clamp(woken, usual)
                }
            }
        }
    }

    /// `ExecutorQueues::maybe_activate`.
    fn maybe_activate(&mut self, q: u8) {
        let qi = q as usize;
        if !self.s.queues[qi].active {
            let v = self.activation_v(qi);
            if self.s.window.is_none() {
                self.check_placement(q, v);
            } else {
                let bound = self.s.default_v.saturating_add(2);
                if v > bound {
                    self.fail(Property::WindowPlacement, || {
                        format!("q{q} is activated in the window at vruntime {v}, more than one behind the usual place, {bound}")
                    });
                }
            }
            let queue = &mut self.s.queues[qi];
            queue.v = v;
            queue.active = true;
            self.push_runnable(qi);
        }
    }

    /// `ExecutorQueues::push_runnable`: the queue enters the heap behind
    /// every queue there.
    fn push_runnable(&mut self, q: usize) {
        let entry = self
            .s
            .queues
            .iter()
            .filter(|queue| queue.in_heap)
            .map(|queue| queue.entry + 1)
            .max()
            .unwrap_or(0);
        let queue = &mut self.s.queues[q];
        queue.entry = entry;
        queue.in_heap = true;
    }

    /// The `Placement` property for a queue activated at `v`.
    fn check_placement(&mut self, q: u8, v: u8) {
        let running = (self.s.in_slice && self.s.executing != q).then_some(self.s.executing);
        let least = self
            .s
            .queues
            .iter()
            .enumerate()
            .filter(|(i, queue)| {
                queue.exists && *i as u8 != q && (queue.in_heap || running == Some(*i as u8))
            })
            .map(|(_, queue)| queue.v)
            .min();
        if let Some(least) = least {
            if v > least.saturating_add(1) {
                self.fail(Property::Placement, || {
                    format!("q{q} is activated at vruntime {v}, behind the least runnable vruntime {least}")
                });
            }
        }
    }

    fn push_back(&mut self, q: u8, t: u8) {
        if !self.s.queues[q as usize].push_back(t) {
            self.fail(Property::Accounting, || {
                format!("q{q} overflows: a runnable is queued twice")
            });
        }
    }

    fn check_hold(&mut self, t: u8, via_last: bool) {
        if self.s.ghost.p & bit(t) != 0 {
            return;
        }
        if self.s.ghost.pdead & bit(t) != 0 {
            self.notes |= NOTE_ABA;
            self.s.ghost.aba |= bit(t);
            if !via_last {
                self.fail(Property::InlineAddressReused, || {
                    format!("t{t} is held at the address of an inline task that has been freed")
                });
            }
            return;
        }
        self.fail(Property::SpuriousHold, || {
            format!("t{t} is held although its poll was not under way when the preempt timer fired")
        });
    }

    /// `ExecutorContext::schedule`, with the ghost bookkeeping.
    fn schedule(&mut self, q: u8, u: u8) {
        self.schedule_with(q, u, true);
    }

    /// `ExecutorContext::schedule_cleanup`: the runnable of a task whose
    /// queue is gone is cancelled and queued on the default queue, never
    /// held, once the window has forgotten the task if it held nothing of it.
    fn schedule_cleanup(&mut self, u: u8) {
        if self.s.window.is_some_and(|w| w.refers_to(u)) {
            self.notes |= NOTE_REMEMBERED_CLEANUP;
        }
        if self.m.rule == Rule::CleanupThroughHold {
            self.mark_closed(u, false, "schedule_cleanup cancelling a runnable");
            self.schedule(0, u);
            return;
        }
        self.forget(u);
        self.mark_closed(u, false, "schedule_cleanup cancelling a runnable");
        self.schedule_with(0, u, false);
    }

    /// `ExecutorContext::forget_preempted`.
    fn forget(&mut self, t: u8) {
        if let Some(w) = self.s.window.as_mut() {
            w.forget(t);
        }
    }

    /// Schedules `u` on queue `q`, letting the window hold it if `may_hold`.
    fn schedule_with(&mut self, q: u8, u: u8, may_hold: bool) {
        let qi = q as usize;
        let was_active = self.s.queues[qi].active;
        let v_before = self.s.queues[qi].v;
        let task = &mut self.s.tasks[u as usize];
        task.st = St::Scheduled;
        task.deferred = false;
        let mut held = false;
        let mut displaced = false;
        let via_last = self
            .s
            .window
            .is_some_and(|w| !w.inline().iter().any(|h| h.task == u));
        if let Some(slot) = self
            .s
            .window
            .as_mut()
            .filter(|_| may_hold)
            .and_then(|w| w.slot_mut(u))
        {
            displaced = slot.runnable;
            slot.runnable = true;
            held = true;
        }
        if displaced {
            self.fail(Property::Accounting, || {
                format!("t{u}'s held runnable is displaced by another of its own")
            });
        }
        if !held {
            self.push_back(q, u);
        }
        if !held || self.m.rule != Rule::HoldWithoutActivate {
            self.maybe_activate(q);
        }
        if held {
            self.check_hold(u, via_last);
        }
        let open = self.s.ghost.open;
        let in_p = open && self.s.ghost.p & bit(u) != 0;
        if self.s.ghost.pairs[u as usize] != 0 && !in_p {
            self.s.ghost.late |= bit(u);
        }
        let tolerated_reuse = held && via_last && self.s.ghost.pdead & bit(u) != 0;
        if !in_p && !tolerated_reuse {
            self.s.ghost.fifo_push(q, u);
        }
        if open && !self.s.in_slice && !in_p {
            self.s.ghost.w |= bit(u);
            let eligible_cross = !was_active && v_before <= self.s.ghost.a_v1;
            for p in bits(self.s.ghost.p) {
                if self.s.tasks[p as usize].closed {
                    continue;
                }
                if self.dest(p) == q || eligible_cross {
                    self.s.ghost.pairs[p as usize] |= bit(u);
                    if held && self.s.ghost.pdead & bit(u) != 0 {
                        self.s.ghost.aba_pairs[p as usize] |= bit(u);
                    } else {
                        self.s.ghost.aba_pairs[p as usize] &= !bit(u);
                    }
                }
            }
        }
    }

    /// `Scheduler::schedule` and `schedule_inactive`: what a wake of a
    /// task that is not running does.
    fn sched_task(&mut self, u: u8) {
        let home = self.s.tasks[u as usize].home;
        if self.entered() {
            if self.exists(home) {
                self.schedule(home, u);
            } else {
                self.schedule_cleanup(u);
            }
        } else if self.exists(home) && !self.s.tasks[u as usize].closed {
            self.schedule(home, u);
        } else {
            self.forget(u);
            self.s.executing = if self.exists(home) { home } else { 0 };
            self.s.tasks[u as usize].st = St::Running;
            self.s.tasks[u as usize].closed = true;
            self.push_frame(u, Kind::Dtor, Base::Outside);
        }
    }

    fn mark_closed(&mut self, t: u8, borrowed: bool, what: &'static str) {
        if !self.s.tasks[t as usize].closed {
            self.s.tasks[t as usize].closed = true;
            self.s.ghost.pairs[t as usize] = 0;
            self.s.ghost.aba_pairs[t as usize] = 0;
            self.user_code(borrowed, what);
        }
    }

    /// The raw task's `do_wake`.
    fn wake(&mut self, u: u8) {
        let task = self.s.tasks[u as usize];
        if task.closed {
            return;
        }
        match task.st {
            St::Parked => self.sched_task(u),
            St::Running => self.s.tasks[u as usize].deferred = true,
            _ => {}
        }
    }

    /// The raw task's `cancel`.
    fn cancel(&mut self, u: u8) {
        let task = self.s.tasks[u as usize];
        if task.closed || task.st == St::Free {
            return;
        }
        self.mark_closed(u, false, "cancelling a task");
        if task.st == St::Parked {
            self.sched_task(u);
        }
    }

    fn free(&mut self, t: u8) {
        self.s.tasks[t as usize] = Task::default();
        let g = &mut self.s.ghost;
        if g.p & bit(t) != 0 {
            g.p &= !bit(t);
            g.pdead |= bit(t);
        }
        g.w &= !bit(t);
        g.pairs[t as usize] = 0;
        for pairs in &mut g.pairs {
            *pairs &= !bit(t);
        }
        g.aba &= !bit(t);
        g.aba_pairs[t as usize] = 0;
        for pairs in &mut g.aba_pairs {
            *pairs &= !bit(t);
        }
        g.late &= !bit(t);
        g.fifo_remove(t);
        self.untrack_reused(t);
    }

    fn push_frame(&mut self, t: u8, kind: Kind, base: Base) {
        if self.s.depth == 0 {
            self.s.base = base;
        }
        self.s.stack[self.s.depth as usize] = Frame { task: t, kind };
        self.s.depth += 1;
        self.s.pc = Pc::Poll;
    }

    fn take_from_back(&mut self, q: u8, t: u8, via_last: bool) -> bool {
        if self.s.queues[q as usize].back() == Some(t) {
            self.s.queues[q as usize].pop_back();
            self.check_hold(t, via_last);
            true
        } else {
            false
        }
    }

    /// `ExecutorContext::requeue`.
    fn requeue(&mut self, t: u8, borrowed: bool) {
        let home = self.s.tasks[t as usize].home;
        let q = if self.exists(home) {
            home
        } else if self.m.rule == Rule::CleanupThroughHold {
            self.mark_closed(t, borrowed, "requeue cancelling a runnable");
            0
        } else {
            self.forget(t);
            self.mark_closed(t, borrowed, "requeue cancelling a runnable");
            self.schedule_with(0, t, false);
            return;
        };
        if self.m.rule == Rule::RequeueAtFront {
            let queue = &mut self.s.queues[q as usize];
            if (queue.len as usize) < NT {
                queue.runnables.copy_within(0..queue.len as usize, 1);
                queue.runnables[0] = t;
                queue.len += 1;
            }
        } else {
            self.push_back(q, t);
        }
        if self.m.rule != Rule::RequeueWithoutActivate {
            self.maybe_activate(q);
        }
    }

    /// `ExecutorContext::end_preempted_window`.
    fn end_window(&mut self) {
        if let Some(w) = self.s.window.take() {
            if self.m.rule != Rule::NoBaselineRefresh {
                let least = self
                    .s
                    .queues
                    .iter()
                    .filter(|q| q.exists && q.in_heap)
                    .map(|q| q.v)
                    .min();
                if let Some(least) = least {
                    self.s.default_v = self.s.default_v.min(least);
                }
            }
            let mut released_inline = 0u8;
            for (i, h) in w.inline().iter().chain(w.last.as_ref()).enumerate() {
                if !h.runnable {
                    continue;
                }
                let t = h.task;
                self.requeue(t, false);
                if self.s.ghost.aba & bit(t) != 0 {
                    self.track_reused(t, released_inline);
                }
                if i < w.n_inline as usize {
                    released_inline |= bit(t);
                }
            }
        }
    }

    /// Starts following a task held through address reuse as the window
    /// releases it.
    fn track_reused(&mut self, t: u8, released_inline: u8) {
        let q = self.s.tasks[t as usize].home;
        let q = if self.exists(q) { q } else { 0 };
        let mut ahead = 0u8;
        for x in self.s.queues[q as usize].items() {
            if *x == t {
                break;
            }
            ahead |= bit(*x);
        }
        let g = &mut self.s.ghost;
        let i = t as usize;
        g.reused |= bit(t);
        g.reused_queue[i] = q;
        g.reused_ahead[i] = ahead;
        g.reused_inline[i] = ahead & released_inline;
        g.reused_slices[i] = 0;
    }

    fn untrack_reused(&mut self, t: u8) {
        let g = &mut self.s.ghost;
        let i = t as usize;
        g.reused &= !bit(t);
        g.reused_queue[i] = 0;
        g.reused_ahead[i] = 0;
        g.reused_inline[i] = 0;
        g.reused_slices[i] = 0;
        for ahead in &mut g.reused_ahead {
            *ahead &= !bit(t);
        }
    }

    fn ghost_close(&mut self) {
        let g = &mut self.s.ghost;
        g.open = false;
        g.p = 0;
        g.w = 0;
        g.pdead = 0;
        g.a_v0 = 0;
        g.a_v1 = 0;
        g.aba = 0;
    }

    /// `ExecutorQueues::preempted_inline`, gated as `spawn_internal` gates it.
    fn preempted_inline(&mut self, t: u8) {
        let gate = match self.m.rule {
            Rule::WindowByActiveExecuting => self.s.executing != NONE,
            _ => self.s.in_slice,
        };
        if !gate || self.s.executing == NONE {
            return;
        }
        if !self.s.in_slice {
            self.fail(Property::OutsideRun, || {
                format!("t{t}, polled inline outside a slice, opens or extends a window")
            });
        }
        let q = self.s.executing;
        let default_v = self.s.default_v;
        let w = self
            .s
            .window
            .get_or_insert_with(|| Window::new(default_v, q));
        if self.m.single_inline() && w.n_inline > 0 {
            return;
        }
        let runnable = if self.m.rule == Rule::TakeInlineAtSliceEnd {
            false
        } else {
            self.take_from_back(q, t, false)
        };
        if let Some(w) = self.s.window.as_mut() {
            w.push_inline(Held { task: t, runnable });
        }
    }

    /// `ExecutorQueues::preempted_last`; returns runnables to requeue once
    /// the borrow is released.
    /// `t` is `NONE` if the slice ended before it polled any task: then it
    /// opens no window and charges no step, but keeps, steps and names one
    /// that cleanup opened as the slice was set up.
    fn preempted_last(&mut self, q: u8, t: u8, v: u8) -> [u8; 2] {
        let mut displaced = [NONE; 2];
        if self.m.rule == Rule::SkipLastInWindow && self.s.window.is_some() {
            return displaced;
        }
        if t == NONE && self.s.window.is_none() {
            return displaced;
        }
        let runnable = t != NONE && self.take_from_back(q, t, true);
        if self.m.slice_end_step() {
            let queue = &mut self.s.queues[q as usize];
            queue.v = queue.v.saturating_add(1);
        }
        if self.m.rule == Rule::TakeInlineAtSliceEnd {
            if let Some(inline) = self.s.window.and_then(|w| w.inline().first().copied()) {
                if !inline.runnable && self.take_from_back(q, inline.task, false) {
                    if let Some(w) = self.s.window.as_mut() {
                        w.inline[0].runnable = true;
                    }
                }
            }
        }
        let w = self.s.window.get_or_insert_with(|| Window::new(v, q));
        let one_slot = match self.m.rule {
            Rule::OneSlotCancelUnderBorrow | Rule::OneSlotDropDisplaced => w.take_inline(),
            _ => None,
        };
        if let Some(inline) = one_slot.filter(|h| h.runnable) {
            if self.m.rule == Rule::OneSlotCancelUnderBorrow {
                self.mark_closed(inline.task, true, "cancelling a displaced runnable");
                displaced[1] = inline.task;
            }
        }
        let w = self.s.window.as_mut().expect("just inserted");
        w.q = q;
        if t == NONE {
            return displaced;
        }
        if let Some(old) = w.last.replace(Held { task: t, runnable }) {
            if old.runnable {
                displaced[0] = old.task;
            }
        }
        displaced
    }

    /// `LocalExecutor::finish_task_queue_run`, with the charge of
    /// `TaskQueue::account_vruntime`, which leaves the queue active only while
    /// it has runnables. Its overflow, and the reset that follows, are left
    /// out.
    fn finish(&mut self, preempted: bool, unwinding: bool) {
        let q = self.s.cur;
        let qi = q as usize;
        let units = if self.s.last == NONE {
            0
        } else if preempted {
            PREEMPTED_CHARGE
        } else {
            SHORT_CHARGE
        };
        let charge = units.saturating_mul(self.s.queues[qi].weight);
        let queue = &mut self.s.queues[qi];
        queue.v = queue.v.saturating_add(charge);
        if self.s.ghost.open {
            self.s.ghost.a_v1 = queue.v;
        }
        queue.active = queue.len > 0;
        let mut need_repush = queue.active;
        let accounted = queue.v;
        self.s.executing = NONE;
        self.s.in_slice = false;
        let mut displaced = [NONE; 2];
        if preempted {
            let last = self.s.last;
            if last != NONE {
                self.count_reused_slice(last);
            }
            let had_window = self.s.window.is_some();
            let before = self.s.queues[qi].v;
            displaced = self.preempted_last(q, last, self.s.least);
            if last == NONE && !had_window {
                self.notes |= NOTE_PREEMPTED_BEFORE_POLL;
                if self.s.window.is_some() || self.s.queues[qi].v != before {
                    self.fail(Property::CodeAssertion, || {
                        format!("q{q}'s slice, ended before its first poll, opened a window or was charged a step")
                    });
                }
            }
        } else if self.s.window.is_some() && !unwinding {
            self.fail(Property::CodeAssertion, || {
                "a window opened in a slice that need_preempt did not end".into()
            });
        }
        let checks = match self.m.rule {
            Rule::NoKeepHeldQueueActive => false,
            Rule::HoldsCheckOnlyWhenPreempted => preempted && self.s.last != NONE,
            _ => true,
        };
        if checks && !need_repush {
            let holds = self
                .s
                .window
                .is_some_and(|w| w.held().any(|t| self.s.tasks[t as usize].home == q));
            if holds {
                self.s.queues[qi].active = true;
                need_repush = self.m.rule != Rule::KeepActiveWithoutRepush;
            }
        }
        if need_repush {
            self.push_runnable(qi);
        }
        let own = accounted;
        self.s.default_v = self
            .s
            .queues
            .iter()
            .filter(|q| q.exists && q.in_heap)
            .map(|q| q.v)
            .min()
            .unwrap_or(own);
        for t in displaced.into_iter().filter(|t| *t != NONE) {
            self.requeue(t, false);
        }
        self.s.pc = Pc::Round;
    }

    fn start_slice(&mut self, q: u8) {
        let queue = &mut self.s.queues[q as usize];
        queue.in_heap = false;
        self.s.executing = q;
        self.s.in_slice = true;
        self.s.least = self.s.default_v;
        self.s.cur = q;
        self.s.last = NONE;
        self.s.tied = 0;
        self.s.pc = Pc::SliceStart;
    }

    /// A panic in a slice's shares callback, in a poll, in a waker the
    /// reactor runs, or in a task such a waker polls inline: a slice's guard
    /// ends the slice if it is armed, and `run` unwinds, its own guard ending
    /// the window. A panic in a reactor pass has no slice to end.
    ///
    /// A poll that panics first has its future dropped by `PollGuard`, with
    /// the slice still under way: that destructor can wake and spawn, and the
    /// unwind goes on when it returns. The futures of the polls beneath it
    /// are dropped without running anything.
    fn panic(&mut self) {
        self.s.budget.panics -= 1;
        if self.s.pc == Pc::Poll {
            let top = self.s.depth as usize - 1;
            let t = self.s.stack[top].task;
            self.s.stack[top].kind = Kind::Dtor;
            self.mark_closed(t, false, "dropping a panicked future");
            self.s.unwind_at = top as u8;
            return;
        }
        self.unwind();
    }

    /// The rest of a panic: the polls still on the stack are dropped, the
    /// slice's guard runs if it is armed, and `run` returns by unwinding.
    fn unwind(&mut self) {
        self.s.unwind_at = NONE;
        let at_start = self.s.pc == Pc::SliceStart;
        let in_slice = self.s.in_slice;
        while self.s.depth > 0 {
            self.s.depth -= 1;
            let t = self.s.stack[self.s.depth as usize].task;
            self.s.stack[self.s.depth as usize] = Frame::default();
            self.free(t);
        }
        if in_slice && !(at_start && self.m.rule == Rule::GuardAfterPrepare) {
            self.finish(false, true);
        }
        self.exit_run();
    }

    /// The least vruntime among the runnable queues, and the mask of the
    /// queues tied at it. Any vruntime, `u8::MAX` included, can be the least.
    fn min_candidates(&self) -> (Option<u8>, u8) {
        let mut best = None;
        let mut mask = 0u8;
        for (q, queue) in self.s.queues.iter().enumerate() {
            if queue.exists && queue.in_heap {
                match best {
                    Some(b) if queue.v > b => {}
                    Some(b) if queue.v == b => mask |= bit(q as u8),
                    _ => {
                        best = Some(queue.v);
                        mask = bit(q as u8);
                    }
                }
            }
        }
        (best, mask)
    }

    /// One turn of `run_task_queues`; false if it stops on a tie the rule
    /// leaves to either side.
    fn round_step(&mut self) -> bool {
        let (_, mask) = self.min_candidates();
        if self.s.preempt {
            self.s.pc = Pc::Pass {
                kind: if mask == 0 {
                    PassKind::Spin
                } else {
                    PassKind::React
                },
                started: false,
                woke: false,
            };
            return true;
        }
        match mask.count_ones() {
            0 => {
                self.s.pc = Pc::Pass {
                    kind: PassKind::Spin,
                    started: false,
                    woke: false,
                };
                true
            }
            1 => {
                self.start_slice(mask.trailing_zeros() as u8);
                true
            }
            _ => {
                let entry = |q: &u8| self.s.queues[*q as usize].entry;
                let q = match self.m.ties() {
                    Ties::EitherWay => return false,
                    Ties::FirstEntered => bits(mask).min_by_key(entry),
                    Ties::LastEntered => bits(mask).max_by_key(entry),
                };
                let q = q.expect("a tie has candidates");
                self.start_slice(q);
                self.s.tied = mask & !bit(q);
                true
            }
        }
    }

    /// Checks the cost of address reuse: a task the window released after
    /// holding it at a reused address is polled before any task of its queue
    /// that was not ahead of it, and before any of those is polled twice.
    fn check_reused(&mut self, t: u8, q: u8) {
        if self.s.ghost.reused & bit(t) != 0 {
            self.untrack_reused(t);
            return;
        }
        for u in bits(self.s.ghost.reused) {
            let i = u as usize;
            if self.s.ghost.reused_queue[i] != q {
                continue;
            }
            if self.s.ghost.reused_ahead[i] & bit(t) == 0 {
                self.fail(Property::AddressReuseOvertaken, || {
                    format!("t{t} is polled ahead of t{u}, held through address reuse, without having been ahead of it")
                });
            }
            self.s.ghost.reused_ahead[i] &= !bit(t);
        }
    }

    /// Counts a slice that a task released from the inline list ended
    /// preempted against each reused task still waiting behind it.
    fn count_reused_slice(&mut self, last: u8) {
        for u in bits(self.s.ghost.reused) {
            let i = u as usize;
            if self.s.ghost.reused_inline[i] & bit(last) != 0 {
                self.s.ghost.reused_slices[i] += 1;
                let n = self.s.ghost.reused_slices[i];
                if n > 1 {
                    self.fail(Property::AddressReuseSlices, || {
                        format!("t{u}, held through address reuse, has waited {n} slices of inline tasks released ahead of it")
                    });
                }
            }
        }
    }

    fn on_poll(&mut self, t: u8, q: u8) {
        self.check_reused(t, q);
        let pending = self.s.ghost.pairs[t as usize];
        let exempt = pending & self.s.ghost.aba_pairs[t as usize];
        if exempt != 0 {
            self.notes |= NOTE_ABA_BOUND;
        }
        let pending = pending & !exempt;
        let late = self.s.ghost.late & bit(t) != 0;
        self.s.ghost.late &= !bit(t);
        let tied_with_late = bits(pending).any(|w| {
            let queue = self
                .s
                .queues
                .iter()
                .position(|queue| queue.items().contains(&w))
                .map_or(self.dest(w), |queue| queue as u8);
            self.s.tied & bit(queue) != 0
        });
        let property = if late && tied_with_late {
            Property::BoundAfterWindowTie
        } else if late {
            Property::BoundAfterWindow
        } else if tied_with_late {
            Property::BoundTie
        } else {
            Property::Bound
        };
        if pending != 0 && !self.s.tasks[t as usize].closed {
            self.fail(property, || {
                let late: Vec<String> = bits(pending).map(|w| format!("t{w}")).collect();
                format!(
                    "t{t}, preempted, is polled again before {}, woken in its window",
                    late.join(", ")
                )
            });
        }
        for pairs in &mut self.s.ghost.pairs {
            *pairs &= !bit(t);
        }
        for pairs in &mut self.s.ghost.aba_pairs {
            *pairs &= !bit(t);
        }
        if let Some((fq, i)) = self.s.ghost.fifo_remove(t) {
            if fq == q && i > 0 {
                self.fail(Property::Fifo, || {
                    format!("t{t} is polled ahead of a task of q{q} scheduled before it, which no window means to reorder")
                });
            }
        }
    }

    /// The run loop of `run_one_task_queue`, between polls.
    fn slice_step(&mut self) {
        if self.s.preempt {
            self.finish(true, false);
            return;
        }
        let q = self.s.cur;
        match self.s.queues[q as usize].pop_front() {
            Some(t) => {
                self.s.last = t;
                self.on_poll(t, q);
                let kind = if self.s.tasks[t as usize].closed {
                    Kind::Dtor
                } else {
                    Kind::RunLoop
                };
                self.s.tasks[t as usize].st = St::Running;
                self.push_frame(t, kind, Base::Slice);
            }
            None => self.finish(false, false),
        }
    }

    fn settle(&mut self) {
        loop {
            match self.s.pc {
                Pc::Round => {
                    if !self.round_step() {
                        break;
                    }
                }
                Pc::Slice => self.slice_step(),
                _ => break,
            }
        }
    }

    fn start_pass(&mut self) {
        if let Pc::Pass {
            kind,
            started: false,
            woke,
        } = self.s.pc
        {
            self.s.pc = Pc::Pass {
                kind,
                started: true,
                woke,
            };
        }
    }

    /// `sys.wait` drains the rings in a `react` pass: the pending preempt
    /// completion is consumed and `need_preempt` reads false from then on.
    fn drain(&mut self) {
        self.start_pass();
        if matches!(
            self.s.pc,
            Pc::Pass {
                kind: PassKind::React,
                ..
            }
        ) && !self.s.drained
        {
            self.s.preempt = false;
            self.s.drained = true;
        }
    }

    fn pass_woke(&mut self) {
        if let Pc::Pass { kind, started, .. } = self.s.pc {
            self.s.pc = Pc::Pass {
                kind,
                started,
                woke: true,
            };
        }
    }

    fn pass_end(&mut self) {
        self.drain();
        self.s.drained = false;
        let Pc::Pass { kind, woke, .. } = self.s.pc else {
            return;
        };
        let loop_top = Pc::Pass {
            kind: PassKind::React,
            started: false,
            woke: false,
        };
        match kind {
            PassKind::React => {
                self.end_window();
                self.ghost_close();
                self.s.pc = Pc::Round;
            }
            PassKind::Spin if woke => self.s.pc = loop_top,
            PassKind::Spin => {
                if self.m.rule != Rule::NoClearAtPark {
                    self.end_window();
                }
                self.ghost_close();
                let runnable = self.s.queues.iter().filter(|q| q.in_heap).count();
                if runnable > 0 {
                    self.fail(Property::CodeAssertion, || {
                        format!("parking with {runnable} runnable task queues")
                    });
                }
                self.s.pc = loop_top;
            }
        }
    }

    fn exit_run(&mut self) {
        self.s.drained = false;
        if self.m.rule != Rule::NoClearAtExit {
            self.end_window();
        }
        self.ghost_close();
        self.s.pc = Pc::Outside;
    }

    fn spawn(&mut self, a: u8) {
        self.s.budget.spawns -= 1;
        let base = match self.s.pc {
            Pc::Pass { kind, .. } => Base::Pass { kind, woke: true },
            _ => self.s.base,
        };
        let home = if self.s.executing != NONE {
            self.s.executing
        } else {
            0
        };
        self.s.tasks[a as usize] = Task {
            st: St::Running,
            home,
            deferred: false,
            closed: false,
        };
        let was = self.s.preempt;
        if self.s.unwind_at != NONE && self.s.in_slice {
            self.notes |= NOTE_UNWIND_SPAWN;
        }
        if was
            && !self.s.drained
            && self.s.window.is_some()
            && matches!(
                self.s.pc,
                Pc::Pass {
                    kind: PassKind::React,
                    ..
                }
            )
        {
            self.notes |= NOTE_PRE_DRAIN_SPAWN;
        }
        self.push_frame(a, Kind::Inline { was }, base);
    }

    fn timer(&mut self) {
        self.s.preempt = true;
        if self.s.in_slice && self.s.depth > 0 && !self.s.ghost.open {
            let g = &mut self.s.ghost;
            g.open = true;
            g.p = 0;
            for f in &self.s.stack[..self.s.depth as usize] {
                g.p |= bit(f.task);
            }
            g.w = 0;
            g.pdead = 0;
            g.a_v0 = self.s.queues[self.s.cur as usize].v;
            g.a_v1 = g.a_v0;
        }
    }

    fn ret(&mut self, ev: Ev) {
        self.s.depth -= 1;
        let f = self.s.stack[self.s.depth as usize];
        self.s.stack[self.s.depth as usize] = Frame::default();
        let t = f.task;
        let finished = !matches!(ev, Ev::Yield | Ev::Park) || self.s.tasks[t as usize].closed;
        let unwound = self.s.unwind_at == self.s.depth;
        match ev {
            Ev::Yield | Ev::Park => {
                let deferred = ev == Ev::Yield || self.s.tasks[t as usize].deferred;
                self.s.tasks[t as usize].st = St::Parked;
                self.s.tasks[t as usize].deferred = false;
                if deferred {
                    self.sched_task(t);
                }
            }
            _ => self.free(t),
        }
        if unwound {
            self.unwind();
            return;
        }
        if let Kind::Inline { was } = f.kind {
            let fire = match self.m.rule {
                Rule::NoBeforeSample => self.s.preempt,
                _ => !was && self.s.preempt,
            } && (!finished || self.m.records_finished());
            if fire {
                self.preempted_inline(t);
            }
        }
        if self.s.depth > 0 {
            self.s.pc = Pc::Poll;
            return;
        }
        match self.s.base {
            Base::Slice => self.s.pc = Pc::Slice,
            Base::Pass { kind, woke } => {
                self.s.pc = Pc::Pass {
                    kind,
                    started: true,
                    woke,
                }
            }
            Base::Outside => {
                self.s.executing = NONE;
                self.s.pc = Pc::Outside;
            }
            Base::Dropping => self.s.pc = Pc::Dropping,
        }
    }

    fn remove_queue(&mut self, q: u8) {
        self.s.budget.removes -= 1;
        self.s.queues[q as usize] = Queue::new(false, 0);
    }

    fn apply(&mut self, ev: Ev) {
        match ev {
            Ev::Timer => self.timer(),
            Ev::StartRun => {
                self.s.budget.runs -= 1;
                self.s.pc = Pc::Pass {
                    kind: PassKind::React,
                    started: false,
                    woke: false,
                };
            }
            Ev::Exit => self.exit_run(),
            Ev::ExecutorDrop => {
                self.s.pc = Pc::Dropping;
                self.s.executing = 0;
            }
            Ev::OutsideWake(u) | Ev::Wake(u) => self.wake(u),
            Ev::PassWake(u) => {
                self.start_pass();
                self.pass_woke();
                self.wake(u);
            }
            Ev::OutsideCancel(u) | Ev::Cancel(u) => {
                self.s.budget.cancels -= 1;
                self.cancel(u);
            }
            Ev::PassCancel(u) => {
                self.start_pass();
                self.pass_woke();
                self.s.budget.cancels -= 1;
                self.cancel(u);
            }
            Ev::DropNext(u) => {
                if self.s.tasks[u as usize].st == St::Scheduled {
                    for q in &mut self.s.queues {
                        q.remove(u);
                    }
                }
                self.s.ghost.fifo_remove(u);
                self.s.tasks[u as usize].st = St::Running;
                self.s.tasks[u as usize].closed = true;
                self.push_frame(u, Kind::Dtor, Base::Dropping);
            }
            Ev::PassSpawn(a) => {
                self.start_pass();
                self.pass_woke();
                self.spawn(a);
            }
            Ev::Spawn(a) => self.spawn(a),
            Ev::PassRemove(q) => {
                self.start_pass();
                self.remove_queue(q);
            }
            Ev::Remove(q) => self.remove_queue(q),
            Ev::PassEnd => self.pass_end(),
            Ev::Pop(q) => {
                let (_, mask) = self.min_candidates();
                self.start_slice(q);
                self.s.tied = mask & !bit(q);
            }
            Ev::Yield | Ev::Park | Ev::Complete | Ev::Done => self.ret(ev),
            Ev::Begin => self.s.pc = Pc::Slice,
            Ev::Drain => self.drain(),
            Ev::Panic => self.panic(),
        }
    }

    fn check(&mut self) {
        let mut count = [0u8; NT];
        let mut stray = false;
        for queue in &self.s.queues {
            for t in queue.items() {
                count[*t as usize] += 1;
                stray |= !queue.exists;
            }
        }
        if let Some(w) = self.s.window {
            for t in w.held() {
                count[t as usize] += 1;
            }
        }
        for t in 0..NT as u8 {
            let st = self.s.tasks[t as usize].st;
            let expected = u8::from(st == St::Scheduled);
            if count[t as usize] != expected {
                let n = count[t as usize];
                self.fail(Property::Accounting, || {
                    format!("t{t} is {st:?} but its runnable is held or queued {n} times")
                });
            }
            if self.on_stack(t) != (st == St::Running) {
                self.fail(Property::Accounting, || {
                    format!("t{t} is {st:?} but its poll is not on the stack")
                });
            }
        }
        if stray {
            self.fail(Property::Accounting, || {
                "a runnable is left in a removed queue".into()
            });
        }
        let dropping =
            self.s.pc == Pc::Dropping || (self.s.pc == Pc::Poll && self.s.base == Base::Dropping);
        if !dropping {
            for (q, queue) in self.s.queues.into_iter().enumerate() {
                if !queue.exists || (self.s.in_slice && q as u8 == self.s.executing) {
                    continue;
                }
                if (queue.len > 0 || queue.active) && !queue.in_heap {
                    self.fail(Property::Stranded, || {
                        format!("q{q} has runnables or is marked active but is not runnable")
                    });
                }
                if queue.in_heap && !queue.active {
                    self.fail(Property::Stranded, || {
                        format!("q{q} is runnable but not marked active")
                    });
                }
            }
        }
        if self.outside_run() && (self.s.window.is_some() || self.s.in_slice) {
            self.fail(Property::OutsideRun, || {
                "a window is open, or a slice marked running, outside run()".into()
            });
        }
        if self.s.window.is_some() && !self.s.ghost.open {
            self.fail(Property::WindowOutlivesIntent, || {
                "a window record exists outside the window a preempted slice opens".into()
            });
        }
    }
}

impl Model {
    fn new(rule: Rule, bounds: Bounds) -> Self {
        Self { rule, bounds }
    }

    /// How the rule breaks a vruntime tie. The earlier designs ran on a heap
    /// that broke ties either way, and they are checked as they ran.
    fn ties(&self) -> Ties {
        match self.rule {
            Rule::Current => Ties::FirstEntered,
            Rule::LastEnteredFirst => Ties::LastEntered,
            _ => Ties::EitherWay,
        }
    }

    /// The initial states as the rule sees them: under a rule that breaks
    /// ties by heap entry, each in every order its runnable queues could have
    /// entered the heap, since an initial state stands for any history.
    fn entry_orders(&self, inits: &[State]) -> Vec<State> {
        let mut out = Vec::new();
        for init in inits {
            if self.ties() == Ties::EitherWay {
                let mut s = init.clone();
                canonicalize(&mut s, false);
                out.push(s);
                continue;
            }
            let heap: Vec<usize> = (0..NQ).filter(|q| init.queues[*q].in_heap).collect();
            let k = heap.len();
            for code in 0..k.pow(k as u32) {
                let ranks: Vec<usize> = (0..k).map(|i| code / k.pow(i as u32) % k).collect();
                if (0..k).any(|r| !ranks.contains(&r)) {
                    continue;
                }
                let mut s = init.clone();
                for (q, rank) in heap.iter().zip(&ranks) {
                    s.queues[*q].entry = *rank as u8;
                }
                canonicalize(&mut s, true);
                out.push(s);
            }
        }
        out
    }

    /// Whether the slice end that `need_preempt` caused charges the queue one
    /// step past the slice, as the current rule does and the designs that
    /// placed the preempted queue by other means did not.
    fn slice_end_step(&self) -> bool {
        !matches!(
            self.rule,
            Rule::NoSliceEndStep
                | Rule::PreemptedQueueAtItsCharge
                | Rule::PreemptedQueueLikeWoken
                | Rule::PreemptedQueueUnclamped
        )
    }

    /// Whether the rule records an inline child its first poll finished, as
    /// the earlier designs did.
    fn records_finished(&self) -> bool {
        self.single_inline()
            || matches!(
                self.rule,
                Rule::RecordFinishedChildren | Rule::RequeueAtFront
            )
    }

    /// Whether the rule has the single inline slot of the earlier designs.
    fn single_inline(&self) -> bool {
        matches!(
            self.rule,
            Rule::OneInlineSlot
                | Rule::SkipLastInWindow
                | Rule::NoClearAtExit
                | Rule::WindowByActiveExecuting
                | Rule::NoBeforeSample
                | Rule::TakeInlineAtSliceEnd
                | Rule::OneSlotCancelUnderBorrow
                | Rule::OneSlotDropDisplaced
        )
    }

    fn spawn_addrs(&self, s: &State, out: &mut Vec<u8>) {
        let referenced =
            |a: u8| s.window.is_some_and(|w| w.refers_to(a)) || s.ghost.pdead & bit(a) != 0;
        let mut fresh = false;
        for a in 0..NT as u8 {
            if s.tasks[a as usize].st != St::Free {
                continue;
            }
            if referenced(a) {
                out.push(a);
            } else if !fresh {
                fresh = true;
                out.push(a);
            }
        }
    }

    fn events(&self, s: &State) -> Vec<Ev> {
        let mut ev = Vec::new();
        let tasks = || (0..NT as u8).map(|t| (t, s.tasks[t as usize]));
        let parked = |t: &Task| t.st == St::Parked && !t.closed;
        let cancellable = |t: &Task| matches!(t.st, St::Parked | St::Scheduled) && !t.closed;
        let removable = |q: u8| {
            q != 0
                && s.queues[q as usize].exists
                && !s.queues[q as usize].active
                && s.executing != q
        };
        let mut addrs = Vec::new();
        if s.budget.spawns > 0 {
            self.spawn_addrs(s, &mut addrs);
        }
        match s.pc {
            Pc::Outside => {
                if s.budget.runs > 0 {
                    ev.push(Ev::StartRun);
                }
                for (t, task) in tasks() {
                    if parked(&task) {
                        ev.push(Ev::OutsideWake(t));
                    }
                    if s.budget.cancels > 0 && cancellable(&task) {
                        ev.push(Ev::OutsideCancel(t));
                    }
                }
                if s.budget.may_drop {
                    ev.push(Ev::ExecutorDrop);
                }
                if !s.preempt {
                    ev.push(Ev::Timer);
                }
            }
            Pc::Dropping => {
                for (t, task) in tasks() {
                    if matches!(task.st, St::Parked | St::Scheduled) {
                        ev.push(Ev::DropNext(t));
                    }
                }
                if !ev.is_empty() && !s.preempt {
                    ev.push(Ev::Timer);
                }
            }
            Pc::Pass { started, .. } => {
                if !started {
                    ev.push(Ev::Exit);
                }
                if !s.preempt {
                    ev.push(Ev::Timer);
                }
                for (t, task) in tasks() {
                    if parked(&task) {
                        ev.push(Ev::PassWake(t));
                    }
                    if s.budget.cancels > 0 && cancellable(&task) {
                        ev.push(Ev::PassCancel(t));
                    }
                }
                ev.extend(addrs.iter().map(|a| Ev::PassSpawn(*a)));
                if s.budget.removes > 0 {
                    ev.extend((0..NQ as u8).filter(|q| removable(*q)).map(Ev::PassRemove));
                }
                if s.budget.panics > 0 {
                    ev.push(Ev::Panic);
                }
                if matches!(
                    s.pc,
                    Pc::Pass {
                        kind: PassKind::React,
                        ..
                    }
                ) && !s.drained
                {
                    ev.push(Ev::Drain);
                }
                ev.push(Ev::PassEnd);
            }
            Pc::Round => {
                let x = Exec {
                    m: self,
                    s: s.clone(),
                    viol: Vec::new(),
                    notes: 0,
                };
                let (_, mask) = x.min_candidates();
                ev.extend(bits(mask).map(Ev::Pop));
            }
            Pc::SliceStart => {
                ev.push(Ev::Begin);
                if !s.preempt && self.bounds.early_preempt {
                    ev.push(Ev::Timer);
                }
                if s.budget.panics > 0 {
                    ev.push(Ev::Panic);
                }
            }
            Pc::Slice => {}
            Pc::Poll => {
                let top = s.stack[s.depth as usize - 1];
                let me = top.task;
                if !s.preempt {
                    ev.push(Ev::Timer);
                }
                let on_stack = |t: u8| s.stack[..s.depth as usize].iter().any(|f| f.task == t);
                for (t, task) in tasks() {
                    if t == me || task.closed {
                        continue;
                    }
                    let wakeable = task.st == St::Parked
                        || (task.st == St::Running && on_stack(t) && !task.deferred);
                    if wakeable && self.bounds.task_wakes {
                        ev.push(Ev::Wake(t));
                    }
                    if s.budget.cancels > 0 && cancellable(&task) {
                        ev.push(Ev::Cancel(t));
                    }
                }
                if (s.depth as usize) < 1 + self.bounds.inline_depth as usize {
                    ev.extend(addrs.iter().map(|a| Ev::Spawn(*a)));
                }
                if s.budget.removes > 0 && top.kind != Kind::Dtor {
                    ev.extend((0..NQ as u8).filter(|q| removable(*q)).map(Ev::Remove));
                }
                if top.kind == Kind::Dtor {
                    ev.push(Ev::Done);
                } else {
                    if self.bounds.yields {
                        ev.push(Ev::Yield);
                    }
                    ev.extend([Ev::Park, Ev::Complete]);
                    if s.budget.panics > 0
                        && s.unwind_at == NONE
                        && matches!(s.base, Base::Slice | Base::Pass { .. })
                    {
                        ev.push(Ev::Panic);
                    }
                }
            }
        }
        ev
    }

    fn step(&self, s: &State, ev: Ev) -> (State, Vec<Violation>, u8) {
        let mut x = Exec {
            m: self,
            s: s.clone(),
            viol: Vec::new(),
            notes: 0,
        };
        x.apply(ev);
        x.settle();
        x.check();
        canonicalize(&mut x.s, self.ties() != Ties::EitherWay);
        (x.s, x.viol, x.notes)
    }

    /// A one line picture of a state: queues front to back, the window, the
    /// timer.
    fn picture(s: &State) -> String {
        let mut out = String::new();
        let ranked = s.queues.iter().any(|queue| queue.entry > 0);
        for (q, queue) in s.queues.iter().enumerate() {
            if !queue.exists {
                continue;
            }
            let items: Vec<String> = queue.items().iter().map(|t| format!("t{t}")).collect();
            let mark = if s.in_slice && s.executing == q as u8 {
                " running"
            } else if queue.in_heap {
                ""
            } else {
                " idle"
            };
            let _ = write!(out, "q{q}[{}] v{}{mark} ", items.join(" "), queue.v);
            if ranked && queue.in_heap {
                let _ = write!(out, "entered {} ", queue.entry + 1);
            }
        }
        if let Some(w) = s.window {
            let show = |h: Option<Held>| match h {
                None => "-".to_string(),
                Some(h) => format!("t{}{}", h.task, if h.runnable { " held" } else { "" }),
            };
            let inline: Vec<String> = w.inline().iter().map(|h| show(Some(*h))).collect();
            let inline = if inline.is_empty() {
                "-".to_string()
            } else {
                inline.join(", ")
            };
            let _ = write!(out, "window(inline {inline}, last {}) ", show(w.last));
        }
        let parked: Vec<String> = (0..NT)
            .filter(|t| s.tasks[*t].st == St::Parked)
            .map(|t| format!("t{t}"))
            .collect();
        if !parked.is_empty() {
            let _ = write!(out, "parked {} ", parked.join(" "));
        }
        if s.preempt {
            out.push_str("need_preempt ");
        }
        if s.depth > 0 {
            let polls: Vec<String> = s.stack[..s.depth as usize]
                .iter()
                .map(|f| format!("t{}", f.task))
                .collect();
            let _ = write!(out, "polling {}", polls.join(">"));
        }
        out.trim_end().to_string()
    }

    fn context(s: &State, ev: Ev) -> String {
        match s.pc {
            Pc::Outside => "outside run".into(),
            Pc::Dropping => "executor drop".into(),
            Pc::Pass { started: false, .. } if ev == Ev::Exit => "loop top".into(),
            Pc::Pass {
                kind: PassKind::React,
                ..
            } => "react pass".into(),
            Pc::Pass {
                kind: PassKind::Spin,
                ..
            } => "spin pass".into(),
            Pc::Round => "round".into(),
            Pc::SliceStart => format!("q{} slice start", s.cur),
            Pc::Slice => "slice".into(),
            Pc::Poll => {
                let top = s.stack[s.depth as usize - 1];
                match top.kind {
                    Kind::RunLoop => format!("q{} slice: t{}", s.cur, top.task),
                    Kind::Inline { .. } => format!("t{} inline", top.task),
                    Kind::Dtor => format!("t{} destructor", top.task),
                }
            }
        }
    }

    fn describe(&self, init: &State, events: &[Ev]) -> Vec<String> {
        let mut lines = vec![format!("start: {}", Self::picture(init))];
        let mut s = init.clone();
        let mut n = 0;
        for ev in events {
            let context = Self::context(&s, *ev);
            let (next, _, _) = self.step(&s, *ev);
            let picture = Self::picture(&next);
            if ev.is_free() {
                lines.push(format!("    [{context}] {ev}  =>  {picture}"));
            } else {
                n += 1;
                lines.push(format!("{n:>2}. [{context}] {ev}  =>  {picture}"));
            }
            s = next;
        }
        lines
    }

    /// Breadth first over every event sequence up to the bound. A violation
    /// is kept with the events it took, and replaced by one that took fewer:
    /// within a level, a free event reaches a violation at the level's cost
    /// after a paid one may have reached it at one more. So the trace kept
    /// for each property is one of the shortest.
    fn explore(&self, inits: &[State]) -> Report {
        let inits = self.entry_orders(inits);
        let started = Instant::now();
        let mut seen = Seen::default();
        let mut nodes: Vec<(u32, Option<Ev>)> = Vec::new();
        let mut roots: Vec<usize> = Vec::new();
        let mut frontier: Vec<(State, u32, u128)> = Vec::new();
        let mut found: BTreeMap<Property, (Violation, u32, Ev, u8)> = BTreeMap::new();
        let mut notes: BTreeMap<Note, (u32, Ev)> = BTreeMap::new();
        let mut transitions = 0usize;
        let mut cheaper = 0usize;
        for (i, init) in inits.iter().enumerate() {
            let fp = fingerprint(init);
            if let std::collections::hash_map::Entry::Vacant(entry) = seen.entry(fp) {
                entry.insert(0);
                nodes.push((roots.len() as u32, None));
                roots.push(i);
                frontier.push((init.clone(), nodes.len() as u32 - 1, fp));
            }
        }
        for level in 0..=self.bounds.max_events {
            let mut next = Vec::new();
            let mut work = std::mem::take(&mut frontier);
            let mut at = 0;
            while at < work.len() {
                let (s, idx, fp) = work[at].clone();
                at += 1;
                if seen.get(&fp).is_some_and(|best| (*best as usize) < level) {
                    continue;
                }
                for ev in self.events(&s) {
                    if level == self.bounds.max_events && !ev.is_free() {
                        continue;
                    }
                    transitions += 1;
                    let (s2, viol, note) = self.step(&s, ev);
                    if note & NOTE_ABA != 0 {
                        notes.entry(Note::AddressReuseHold).or_insert((idx, ev));
                    }
                    if note & NOTE_ABA_BOUND != 0 {
                        notes.entry(Note::AddressReuseBound).or_insert((idx, ev));
                    }
                    if note & NOTE_PRE_DRAIN_SPAWN != 0 {
                        notes.entry(Note::PreDrainSpawn).or_insert((idx, ev));
                    }
                    if note & NOTE_REMEMBERED_CLEANUP != 0 {
                        notes.entry(Note::RememberedCleanup).or_insert((idx, ev));
                    }
                    if note & NOTE_UNWIND_SPAWN != 0 {
                        notes.entry(Note::UnwindSpawn).or_insert((idx, ev));
                    }
                    if note & NOTE_PREEMPTED_BEFORE_POLL != 0 {
                        notes
                            .entry(Note::PreemptedBeforeFirstPoll)
                            .or_insert((idx, ev));
                    }
                    let violated = !viol.is_empty();
                    let cost = if ev.is_free() { level } else { level + 1 } as u8;
                    for v in viol {
                        if found.get(&v.property).is_none_or(|kept| cost < kept.3) {
                            found.insert(v.property, (v, idx, ev, cost));
                        }
                    }
                    if violated && !self.bounds.continue_past {
                        continue;
                    }
                    let fp2 = fingerprint(&s2);
                    let known = seen.get(&fp2).copied();
                    if known.is_some_and(|best| best <= cost) {
                        continue;
                    }
                    if known.is_some() {
                        cheaper += 1;
                        notes.entry(Note::ReachedCheaper).or_insert((idx, ev));
                    }
                    seen.insert(fp2, cost);
                    nodes.push((idx, Some(ev)));
                    let node = nodes.len() as u32 - 1;
                    if ev.is_free() {
                        work.push((s2, node, fp2));
                    } else {
                        next.push((s2, node, fp2));
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        let path = |mut idx: u32, last: Ev| -> (usize, Vec<Ev>) {
            let mut events = vec![last];
            loop {
                let (parent, ev) = nodes[idx as usize];
                match ev {
                    Some(ev) => {
                        events.push(ev);
                        idx = parent;
                    }
                    None => {
                        events.reverse();
                        return (roots[parent as usize], events);
                    }
                }
            }
        };
        let trace = |idx: u32, ev: Ev| {
            let (init, events) = path(idx, ev);
            self.describe(&inits[init], &events)
        };
        Report {
            rule: self.rule,
            states: seen.len(),
            transitions,
            cheaper,
            elapsed: started.elapsed(),
            found: found
                .into_iter()
                .map(|(p, (v, idx, ev, _))| (p, (v, trace(idx, ev))))
                .collect(),
            notes: notes
                .into_iter()
                .map(|(n, (idx, ev))| (n, trace(idx, ev)))
                .collect(),
        }
    }
}

/// The executor sources the model mirrors, read at compile time for the
/// drift tripwire.
const SOURCES: [(&str, &str); 4] = [
    ("mod.rs", include_str!("mod.rs")),
    ("context.rs", include_str!("context.rs")),
    ("multitask.rs", include_str!("multitask.rs")),
    ("task/raw.rs", include_str!("../task/raw.rs")),
];

/// The items the model mirrors: the file, the text the item's line starts
/// with, and the fingerprint of its source with layout and docs stripped.
/// `mirrored_code_is_unchanged` fails when any of them changes.
const MIRRORED: &[(&str, &str, u64)] = &[
    ("mod.rs", "fn account_vruntime(", 0x1d605a39500b0b33),
    ("mod.rs", "impl Ord for TaskQueue {", 0xf179e133f428dbaa),
    ("mod.rs", "struct Held {", 0x42c70e03b6beb9e3),
    ("mod.rs", "struct Preempted {", 0x7e738adafadfe413),
    ("mod.rs", "impl Preempted {", 0x22a14af581d732b3),
    ("mod.rs", "struct ExecutorQueues {", 0xcfb304a09e9183b1),
    ("mod.rs", "fn take_from_back(", 0x67a9a075df2f2f82),
    ("mod.rs", "fn preempted_inline(", 0x659550d6696aad80),
    ("mod.rs", "fn preempted_last(", 0x3e1893d3a36eba17),
    ("mod.rs", "fn close_window(", 0xd4f1f318e1f4ce02),
    ("mod.rs", "fn hold(", 0x595f37c5c85e6609),
    ("mod.rs", "fn activation_vruntime(", 0xd7c963cdb438af51),
    ("mod.rs", "fn maybe_activate(", 0x8454b04ddcb9cc66),
    ("mod.rs", "fn push_runnable(", 0x2ae660fc8bfd81a5),
    ("mod.rs", "fn run_task_queues(", 0x2bb66f39a7fd05ae),
    ("mod.rs", "fn run_one_task_queue(", 0xf426337dbccdb8f7),
    ("mod.rs", "fn finish_task_queue_run(", 0x369084b8815a33ab),
    ("mod.rs", "pub fn run<T>(", 0xe42fa887fcd45e87),
    (
        "context.rs",
        "pub(super) fn with_cleanup<T>(",
        0xf6db0da5d75f6489,
    ),
    (
        "context.rs",
        "pub(super) fn schedule_cleanup(",
        0xe209b8eacea650ba,
    ),
    ("context.rs", "pub(super) fn schedule(", 0xcdab855177f815a2),
    ("context.rs", "pub(super) fn requeue(", 0x04909e000a2653ee),
    (
        "context.rs",
        "pub(super) fn end_preempted_window(",
        0x78df80de4e7b65c7,
    ),
    (
        "context.rs",
        "pub(super) fn spawn_internal<T>(",
        0x465ae9c1fe18b181,
    ),
    (
        "multitask.rs",
        "fn schedule(&self, runnable: Runnable)",
        0x507c1e64bc2b0975,
    ),
    ("multitask.rs", "fn schedule_inactive(", 0xad3ecbd1cc2686cd),
    (
        "multitask.rs",
        "pub(crate) fn spawn_and_run<T>(",
        0x9ae028c8a7560f05,
    ),
    ("task/raw.rs", "unsafe fn do_wake(", 0x9cbfb2252ef17828),
    ("task/raw.rs", "unsafe fn cancel(ptr", 0xdc7d5a5b06e5f53f),
    ("task/raw.rs", "unsafe fn run(ptr", 0x597be6465e69c934),
    (
        "task/raw.rs",
        "impl<F, R, S> Drop for PollGuard<F, R, S>",
        0xac6d12b89252e442,
    ),
    (
        "context.rs",
        "pub(super) fn forget_preempted(",
        0x353ceeb4272b403f,
    ),
];

/// What a span of Rust source is, as far as the tripwire needs to know.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lexeme {
    /// One character of code.
    Code(char),
    /// Whitespace.
    Space,
    /// A line, doc or block comment.
    Comment,
    /// A string, raw string or char literal, kept whole.
    Literal,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Splits `source` into lexemes with their byte ranges. It knows comments,
/// strings, raw strings, char literals and lifetimes, which is enough to
/// match braces and to tell layout from code; it is not a Rust lexer.
fn lex(source: &str) -> Vec<(usize, usize, Lexeme)> {
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let end_of = |i: usize| chars.get(i).map_or(source.len(), |(at, _)| *at);
    let at = |i: usize| chars.get(i).map(|(_, c)| *c);
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        let start = chars[i].0;
        let prev_ident = i > 0 && is_ident(chars[i - 1].1);
        let (len, lexeme) = if c == '/' && at(i + 1) == Some('/') {
            let mut j = i;
            while j < chars.len() && chars[j].1 != '\n' {
                j += 1;
            }
            (j - i, Lexeme::Comment)
        } else if c == '/' && at(i + 1) == Some('*') {
            let mut depth = 0;
            let mut j = i;
            while j < chars.len() {
                if chars[j].1 == '/' && at(j + 1) == Some('*') {
                    depth += 1;
                    j += 2;
                } else if chars[j].1 == '*' && at(j + 1) == Some('/') {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            (j - i, Lexeme::Comment)
        } else if c == '"' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].1 != '"' {
                if chars[j].1 == '\\' {
                    j += 1;
                }
                j += 1;
            }
            (j + 1 - i, Lexeme::Literal)
        } else if c == 'r'
            && (!prev_ident || (chars[i - 1].1 == 'b' && (i < 2 || !is_ident(chars[i - 2].1))))
            && matches!(at(i + 1), Some('#' | '"'))
            && {
                let mut j = i + 1;
                while at(j) == Some('#') {
                    j += 1;
                }
                at(j) == Some('"')
            }
        {
            let mut j = i + 1;
            let mut hashes = 0;
            while at(j) == Some('#') {
                hashes += 1;
                j += 1;
            }
            j += 1;
            loop {
                if j >= chars.len() {
                    break;
                }
                if chars[j].1 == '"' && (1..=hashes).all(|k| at(j + k) == Some('#')) {
                    j += 1 + hashes;
                    break;
                }
                j += 1;
            }
            (j - i, Lexeme::Literal)
        } else if c == '\'' && at(i + 1) == Some('\\') {
            let mut j = i + 2;
            while j < chars.len() && chars[j].1 != '\'' {
                j += 1;
            }
            (j + 1 - i, Lexeme::Literal)
        } else if c == '\'' && at(i + 2) == Some('\'') {
            (3, Lexeme::Literal)
        } else if c.is_whitespace() {
            (1, Lexeme::Space)
        } else {
            (1, Lexeme::Code(c))
        };
        let next = (i + len).min(chars.len());
        out.push((start, end_of(next), lexeme));
        i = next;
    }
    out
}

/// The source of the item whose line, trimmed, starts with `start`: from
/// there to the brace that closes its body, braces in comments and literals
/// not counted. Fails if no line or more than one starts so, which is what
/// a rename or a duplicate looks like.
fn mirrored_item<'a>(source: &'a str, start: &str) -> Result<&'a str, String> {
    let mut found = None;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with(start) {
            if found.is_some() {
                return Err(format!("more than one item starts with `{start}`"));
            }
            found = Some(offset + line.len() - trimmed.len());
        }
        offset += line.len();
    }
    let begin = found
        .ok_or_else(|| format!("no item starts with `{start}`, so it was renamed or removed"))?;
    let mut depth = 0usize;
    for (_, end, lexeme) in lex(&source[begin..]) {
        match lexeme {
            Lexeme::Code('{') => depth += 1,
            Lexeme::Code('}') => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("unbalanced braces after `{start}`"))?;
                if depth == 0 {
                    return Ok(&source[begin..begin + end]);
                }
            }
            _ => {}
        }
    }
    Err(format!("the body of `{start}` never closes"))
}

/// Fingerprints an item's source so that layout does not count. Comments
/// and docs go, and whitespace goes except where it separates two words.
/// Literals are kept whole. A trailing comma before a closing bracket goes,
/// which is what rustfmt adds or removes when it reflows a line, unless it
/// makes a parenthesised group a 1-tuple; a group right after a word or a
/// closing bracket is taken for a call's arguments.
///
/// What remains: a 1-tuple right after a keyword, as in `match (x,)`, is
/// taken for a call and loses its comma, and so does one whose single
/// element holds a comma inside angle brackets.
fn source_fingerprint(item: &str) -> u64 {
    struct Group {
        commas: usize,
        call: bool,
    }
    let mut out = String::new();
    let mut groups: Vec<Group> = Vec::new();
    let mut space = false;
    for (start, end, lexeme) in lex(item) {
        match lexeme {
            Lexeme::Space | Lexeme::Comment => space = true,
            Lexeme::Literal => {
                out.push_str(&item[start..end]);
                space = false;
            }
            Lexeme::Code(c) => {
                let last = out.chars().last();
                if space && last.is_some_and(is_ident) && is_ident(c) {
                    out.push(' ');
                }
                space = false;
                match c {
                    '(' | '[' | '{' => {
                        let call = c == '('
                            && last
                                .is_some_and(|l| is_ident(l) || matches!(l, ')' | ']' | '>' | '!'));
                        groups.push(Group { commas: 0, call });
                    }
                    ')' | ']' | '}' => {
                        let group = groups.pop();
                        if out.ends_with(',') {
                            let one_tuple =
                                c == ')' && group.is_some_and(|g| g.commas == 1 && !g.call);
                            if !one_tuple {
                                out.pop();
                            }
                        }
                    }
                    '>' if out.ends_with(',') => {
                        out.pop();
                    }
                    ',' => {
                        if let Some(group) = groups.last_mut() {
                            group.commas += 1;
                        }
                    }
                    _ => {}
                }
                out.push(c);
            }
        }
    }
    let mut h = Fnv128::default();
    h.write(out.as_bytes());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Events per sequence for the current rule. A debug build gets a
    /// shorter bound, still past every shortest trace.
    const MAX_EVENTS: usize = if cfg!(debug_assertions) { 8 } else { 14 };

    /// Events per sequence for the earlier rules, whose shortest traces are
    /// at most nine events long.
    const VALIDATE_EVENTS: usize = if cfg!(debug_assertions) { 8 } else { 12 };

    const fn seed(home: u8, queued: bool) -> Seed {
        Seed { home, queued }
    }

    /// The initial states searched, each at the loop top of `run` and
    /// between runs: one queue with a busy task and a parked one, or two busy
    /// tasks and a parked one; and two queues of equal shares, a busy task in
    /// one and a parked task in the other, or in both, with the other queue
    /// charged less than, as much as, or more than the busy one.
    fn inits(cancels: u8, spawns: u8) -> Vec<State> {
        inits_with(cancels, spawns, 0)
    }

    fn inits_with(cancels: u8, spawns: u8, panics: u8) -> Vec<State> {
        let mut v = Vec::new();
        for inside in [true, false] {
            let budget = |removes| Budget {
                panics,
                spawns,
                cancels,
                removes,
                runs: 1,
                may_drop: true,
            };
            v.push(initial(
                &[0],
                &[seed(0, true), seed(0, false)],
                inside,
                budget(0),
            ));
            v.push(initial(
                &[0],
                &[seed(0, true), seed(0, false), seed(0, true)],
                inside,
                budget(0),
            ));
            for q1 in [0, 2, 4] {
                v.push(initial(
                    &[2, q1],
                    &[seed(0, true), seed(1, false)],
                    inside,
                    budget(1),
                ));
                v.push(initial(
                    &[2, q1],
                    &[seed(0, true), seed(1, false), seed(0, false)],
                    inside,
                    budget(1),
                ));
            }
        }
        v
    }

    /// The initial states with one queue, enough for every earlier bug but
    /// the cross-queue ones.
    fn one_queue(cancels: u8, spawns: u8, panics: u8) -> Vec<State> {
        inits_with(cancels, spawns, panics)
            .into_iter()
            .filter(|s| !s.queues[1].exists)
            .collect()
    }

    /// Bounds for a validation: in a debug build, as CI runs it, just the
    /// events its shortest trace takes, so that it stays fast; in release,
    /// at least `VALIDATE_EVENTS`.
    fn needs(events: usize, inline_depth: u8) -> Bounds {
        Bounds {
            max_events: if cfg!(debug_assertions) {
                events
            } else {
                VALIDATE_EVENTS.max(events)
            },
            ..bounds(inline_depth)
        }
    }

    fn bounds(inline_depth: u8) -> Bounds {
        Bounds {
            max_events: VALIDATE_EVENTS,
            inline_depth,
            task_wakes: true,
            yields: true,
            early_preempt: true,
            continue_past: false,
        }
    }

    /// `MAX_EVENTS`, or `WINDOW_MODEL_EVENTS` from the environment for a
    /// deeper search. A smaller value is raised to `MAX_EVENTS`, so that the
    /// variable can only deepen a search, never make it vacuous.
    fn full_bounds(inline_depth: u8) -> Bounds {
        let max_events = std::env::var("WINDOW_MODEL_EVENTS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .map_or(MAX_EVENTS, |v| v.max(MAX_EVENTS));
        Bounds {
            max_events,
            continue_past: true,
            ..bounds(inline_depth)
        }
    }

    fn search(rule: Rule, bounds: Bounds) -> Report {
        search_from(rule, bounds, &one_queue(1, 2, 0))
    }

    fn search_from(rule: Rule, bounds: Bounds, inits: &[State]) -> Report {
        let report = Model::new(rule, bounds).explore(inits);
        println!("{report}");
        report
    }

    fn trace(report: &Report, property: Property) -> &[String] {
        match report.found.get(&property) {
            Some((_, trace)) => trace,
            None => panic!("{property:?} not found for {:?}:\n{report}", report.rule),
        }
    }

    /// The last event a trace numbers: the step that broke the property, or
    /// the one before the free step where it broke.
    fn last_event(trace: &[String]) -> &str {
        trace
            .iter()
            .rev()
            .find(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
            .expect("a trace has an event")
    }

    fn count(trace: &[String], needle: &str) -> usize {
        trace.iter().filter(|line| line.contains(needle)).count()
    }

    /// (e): the spawner of an inline task that spent the slice is not held,
    /// and runs before the task woken in the window.
    #[test]
    fn finds_e_spawner_skipped_when_an_inline_window_is_open() {
        let report = search(Rule::SkipLastInWindow, needs(7, 1));
        let t = trace(&report, Property::Bound);
        assert_eq!(count(t, "spawn_locals"), 1, "{report}");
        assert!(t.last().unwrap().contains("polling t0"), "{report}");
    }

    /// (b): the record survives `run` returning.
    #[test]
    fn finds_b_window_kept_past_run_exit() {
        let report = search(Rule::NoClearAtExit, needs(4, 1));
        let t = trace(&report, Property::OutsideRun);
        assert!(last_event(t).contains("run() returns"), "{report}");
    }

    /// (b): the record survives a park, and a task woken after the park is
    /// held behind one woken after it.
    #[test]
    fn finds_b_window_kept_past_a_park() {
        let report = search(
            Rule::NoClearAtPark,
            Bounds {
                continue_past: true,
                ..needs(7, 1)
            },
        );
        trace(&report, Property::WindowOutlivesIntent);
        trace(&report, Property::Fifo);
    }

    /// (f): a destructor run between runs spawns, and the inline poll opens
    /// a window outside any slice.
    #[test]
    fn finds_f_window_opened_outside_a_slice() {
        let report = search(Rule::WindowByActiveExecuting, needs(4, 1));
        let t = trace(&report, Property::OutsideRun);
        assert_eq!(count(t, "destructor] spawn_locals"), 1, "{report}");
    }

    /// (d): a child spawned after the timer fired is taken for the task
    /// that spent the slice.
    #[test]
    fn finds_d_child_blamed_for_a_pending_preemption() {
        let report = search(Rule::NoBeforeSample, needs(4, 1));
        let t = trace(&report, Property::SpuriousHold);
        let fired = t.iter().position(|l| l.contains("timer fires")).unwrap();
        let spawned = t.iter().position(|l| l.contains("spawn_locals")).unwrap();
        assert!(fired < spawned, "{report}");
    }

    /// (c): the inline task that woke itself is not last in its queue when
    /// the slice ends, because a second inline spawn queued behind it. Task
    /// wakes and cancels, which queue a task behind it just as well, are left
    /// out so that the shortest trace is that one.
    #[test]
    fn finds_c_inline_task_assumed_last() {
        let report = search_from(
            Rule::TakeInlineAtSliceEnd,
            Bounds {
                task_wakes: false,
                ..needs(9, 1)
            },
            &one_queue(0, 2, 0),
        );
        let t = trace(&report, Property::Bound);
        assert_eq!(count(t, "slice: t0] spawn_locals"), 2, "{report}");
    }

    /// (g): a displaced runnable is cancelled while the queues are borrowed.
    #[test]
    fn finds_g_displaced_runnable_cancelled_under_borrow() {
        let report = search(Rule::OneSlotCancelUnderBorrow, needs(5, 1));
        trace(&report, Property::UserCodeUnderBorrow);
    }

    /// Without the second commit's placement, a queue woken in the window
    /// sorts behind the preempted queue, whose held task then runs first.
    #[test]
    fn finds_the_woken_queue_placed_behind_the_preempted_one() {
        let report = search_from(Rule::UsualActivation, needs(5, 1), &inits(1, 2));
        let t = trace(&report, Property::Bound);
        assert!(t.iter().any(|l| l.contains("wakes t1")), "{report}");
        assert!(t[0].contains("q1["), "{report}");
    }

    #[test]
    fn finds_a_displaced_runnable_dropped() {
        let report = search(Rule::OneSlotDropDisplaced, needs(5, 1));
        trace(&report, Property::Accounting);
    }

    #[test]
    fn finds_a_held_queue_left_idle_before_a_park() {
        let report = search(Rule::NoKeepHeldQueueActive, needs(6, 1));
        trace(&report, Property::CodeAssertion);
    }

    #[test]
    fn finds_a_held_queue_marked_active_outside_the_heap() {
        let report = search(Rule::KeepActiveWithoutRepush, needs(5, 1));
        trace(&report, Property::Stranded);
    }

    /// Within bounds, the queue a held runnable returns to is always active
    /// already when the window ends, so `requeue`'s activation carries no
    /// weight: removing it breaks no property.
    #[test]
    fn requeue_activation_is_not_load_bearing_within_bounds() {
        let report = search_from(Rule::RequeueWithoutActivate, bounds(1), &inits(1, 2));
        assert!(report.found.is_empty(), "{report}");
    }

    /// Within bounds, a held runnable's queue is activated by the next round
    /// or kept runnable by the slice's end, so `hold`'s activation breaks no
    /// property either.
    #[test]
    fn hold_activation_is_not_load_bearing_within_bounds() {
        let report = search_from(Rule::HoldWithoutActivate, bounds(1), &inits(1, 2));
        assert!(report.found.is_empty(), "{report}");
    }

    /// An earlier design's single inline slot: with an inline poll inside an inline
    /// poll, the middle task was polling when the timer fired but only the
    /// innermost and the outermost are held, so the middle one runs ahead of
    /// the task woken in the window.
    #[test]
    fn finds_the_middle_of_nested_inline_polls_with_one_inline_slot() {
        let report = search(Rule::OneInlineSlot, needs(9, 2));
        let t = trace(&report, Property::Bound);
        assert_eq!(count(t, "polling t0>t2>t3"), 2, "{report}");
    }

    /// One busy task and nothing else, so that a search can go deep enough
    /// for nested inline tasks, a task at a reused address behind them, and
    /// the rounds it then waits.
    fn reuse_inits(spawns: u8) -> Vec<State> {
        let budget = Budget {
            panics: 0,
            spawns,
            cancels: 0,
            removes: 0,
            runs: 0,
            may_drop: false,
        };
        vec![initial(&[0], &[seed(0, true)], true, budget)]
    }

    /// Inside `run`, a busy queue whose slices cost it ten times what they
    /// cost the others, beside a busy queue of ordinary shares, with or
    /// without an idle third queue with a parked task.
    fn low_share_inits() -> Vec<State> {
        let budget = Budget {
            panics: 0,
            spawns: 1,
            cancels: 0,
            removes: 0,
            runs: 0,
            may_drop: false,
        };
        let mut v = Vec::new();
        for (queue_v, seeds) in [
            (&[0u8, 0][..], &[seed(0, true), seed(1, true)][..]),
            (
                &[0, 0, 0][..],
                &[seed(0, true), seed(1, true), seed(2, false)][..],
            ),
        ] {
            let mut s = initial(queue_v, seeds, true, budget);
            s.queues[0].weight = 10;
            v.push(s);
        }
        v
    }

    /// Two or three queues inside `run`: a busy one charged 4, and one or two
    /// idle ones, each with a parked task, charged 0, 4, or 7, which is what
    /// a preempted slice brings the busy one to. The window can place an
    /// idle queue by its charge, below the cached least vruntime, and its
    /// task can then wake the other; or place it level with the preempted
    /// queue's charge.
    fn placement_inits() -> Vec<State> {
        placement_inits_with(1)
    }

    fn placement_inits_with(spawns: u8) -> Vec<State> {
        let budget = Budget {
            panics: 0,
            spawns,
            cancels: 0,
            removes: 0,
            runs: 0,
            may_drop: false,
        };
        let mut v = Vec::new();
        for q1 in [0, 4, 7] {
            v.push(initial(
                &[4, q1],
                &[seed(0, true), seed(1, false)],
                true,
                budget,
            ));
            for q2 in [0, 4, 7] {
                v.push(initial(
                    &[4, q1, q2],
                    &[seed(0, true), seed(1, false), seed(2, false)],
                    true,
                    budget,
                ));
            }
        }
        v
    }

    /// Events per sequence for the address reuse searches, which are small
    /// enough to run this deep in a debug build too.
    const REUSE_EVENTS: usize = 30;

    /// A window that releases its runnables at the front lets the tasks it
    /// released after a reused one overtake it.
    #[test]
    fn finds_a_reused_task_overtaken() {
        let report = search_from(
            Rule::RequeueAtFront,
            Bounds {
                max_events: REUSE_EVENTS,
                continue_past: true,
                ..bounds(2)
            },
            &reuse_inits(3),
        );
        trace(&report, Property::AddressReuseOvertaken);
    }

    fn reuse_search(inline_depth: u8, spawns: u8) -> Report {
        search_from(
            Rule::Current,
            Bounds {
                max_events: REUSE_EVENTS,
                continue_past: true,
                ..bounds(inline_depth)
            },
            &reuse_inits(spawns),
        )
    }

    /// Without nested inline polls, a task held at a reused address waits at
    /// most one slice of the inline task released ahead of it.
    #[test]
    fn current_rule_address_reuse_costs_one_slice() {
        let report = reuse_search(1, 3);
        assert!(report.found.is_empty(), "{report}");
    }

    /// With nested inline polls, every inline task released ahead of it can
    /// spend a slice first, each once: one slice per inline task, not one in
    /// all. Nothing overtakes it.
    #[test]
    fn current_rule_address_reuse_costs_a_slice_per_nested_inline_task() {
        let report = reuse_search(2, 3);
        let found: Vec<_> = report.found.keys().copied().collect();
        assert_eq!(found, [Property::AddressReuseSlices], "{report}");
        let t = trace(&report, Property::AddressReuseSlices);
        assert_eq!(count(t, "polling t0>t1>t2"), 2, "{report}");
    }

    /// An earlier design recorded an inline child its first poll finished. Its
    /// address could then go to a sibling spawned later in the slice, which
    /// was held in its place, behind tasks scheduled after it.
    #[test]
    fn finds_a_finished_child_address_taken_by_a_sibling() {
        let report = search(
            Rule::RecordFinishedChildren,
            Bounds {
                continue_past: true,
                ..needs(9, 1)
            },
        );
        let t = trace(&report, Property::Fifo);
        let done = t
            .iter()
            .position(|l| l.contains("inline] completes"))
            .unwrap_or_else(|| panic!("{report}"));
        assert!(
            t[done + 1..]
                .iter()
                .any(|l| l.contains("slice: t0] spawn_locals")),
            "{report}"
        );
        trace(&report, Property::InlineAddressReused);
    }

    /// An earlier design armed a slice's guard after its shares callback, so a panic
    /// there left the slice marked running after `run` unwound.
    #[test]
    fn finds_a_slice_left_running_by_a_panic_at_its_start() {
        let report = search_from(Rule::GuardAfterPrepare, needs(2, 1), &one_queue(1, 2, 1));
        let t = trace(&report, Property::OutsideRun);
        assert!(last_event(t).contains("slice start] panics"), "{report}");
    }

    /// An earlier design checked whether a queue's only runnable was held only when
    /// `need_preempt` ended its slice. When a slice unwinds, `run` unwinds
    /// with it and its guard requeues what the window held, so the model
    /// finds nothing for this one.
    #[test]
    fn holds_check_on_unwind_is_not_load_bearing_within_bounds() {
        let report = search_from(
            Rule::HoldsCheckOnlyWhenPreempted,
            bounds(1),
            &inits_with(1, 2, 1),
        );
        assert!(report.found.is_empty(), "{report}");
    }

    /// An earlier design left the cached least vruntime above a queue the window
    /// placed by its charge, so a queue that queue woke after the window
    /// landed behind the preempted one.
    #[test]
    fn finds_a_queue_placed_behind_a_stale_baseline() {
        let report = search_from(Rule::NoBaselineRefresh, needs(6, 1), &placement_inits());
        let t = trace(&report, Property::Placement);
        assert!(
            t.iter().any(|l| l.contains("q1 slice: t1] wakes t2")),
            "{report}"
        );
    }

    /// The current rule with panics in shares callbacks, polls and wakers,
    /// including a panicked future whose destructor spawns as its slice
    /// unwinds.
    #[test]
    fn current_rule_with_panics() {
        let report = search_from(Rule::Current, full_bounds(1), &inits_with(1, 2, 1));
        assert!(report.notes.contains_key(&Note::UnwindSpawn), "{report}");
        assert_only_known(&report);
    }

    fn three_queue_bounds() -> Bounds {
        let bounds = full_bounds(1);
        Bounds {
            max_events: bounds.max_events.max(12),
            ..bounds
        }
    }

    /// An earlier design placed the queue whose slice opened the window like the
    /// queues it woke. A busy task that awaits I/O when its slice is
    /// preempted leaves its queue idle; if another queue is runnable at the
    /// least vruntime, the window placed both that queue and an idle queue
    /// woken with it at that vruntime plus one, since the earliest and the
    /// usual place coincide. They tie, and the heap may run the preempted
    /// task first.
    #[test]
    fn finds_the_preempted_queue_tied_with_a_woken_one() {
        let report = search_from(
            Rule::PreemptedQueueLikeWoken,
            reactivation_bounds(),
            &placement_inits_with(0),
        );
        let t = trace(&report, Property::BoundTie);
        assert!(
            t.iter()
                .any(|l| l.contains("an event wakes t0") && l.contains("last t0 held")),
            "{report}"
        );
    }

    /// A preempted queue reactivated in its window with its own charge
    /// carries the debt of the slice it just spent, which for a queue with
    /// few shares is many slices of the others.
    #[test]
    fn finds_a_low_share_preempted_queue_held_back_by_its_charge() {
        let report = search_from(
            Rule::PreemptedQueueUnclamped,
            three_queue_bounds(),
            &low_share_inits(),
        );
        trace(&report, Property::WindowPlacement);
    }

    /// Bounds under which a preempted task cannot queue itself again: it
    /// neither yields nor is woken by a child, so its queue comes back only
    /// if the window or a later wake reactivates it.
    fn reactivation_bounds() -> Bounds {
        Bounds {
            yields: false,
            ..three_queue_bounds()
        }
    }

    /// An earlier design placed a preempted queue reactivated in its window
    /// at its own charge, so a queue the window woke that was charged the
    /// same tied with it, and the heap could run the preempted task first.
    #[test]
    fn finds_an_equal_charge_tie_with_the_reactivated_preempted_queue() {
        let report = search_from(
            Rule::PreemptedQueueAtItsCharge,
            reactivation_bounds(),
            &placement_inits_with(0),
        );
        let t = trace(&report, Property::BoundTie);
        assert!(
            t.iter()
                .any(|l| l.contains("an event wakes t0") && l.contains("last t0 held")),
            "{report}"
        );
    }

    /// An earlier design charged no step at the slice end. A preempted task
    /// that yields keeps its queue runnable at its bare charge, and a queue
    /// the window wakes that was charged the same is placed level with it.
    #[test]
    fn finds_the_yield_case_tie_without_a_slice_end_step() {
        let report = search_from(
            Rule::NoSliceEndStep,
            three_queue_bounds(),
            &placement_inits(),
        );
        let t = trace(&report, Property::BoundTie);
        assert_eq!(events_in(t), 6, "{report}");
        assert!(t.iter().any(|l| l.contains("yields")), "{report}");
    }

    /// The current rule places a reactivated preempted queue one behind its
    /// own charge: when it can come back only that way, nothing ties or is
    /// reversed in the window. What remains happens after the window.
    #[test]
    fn current_rule_reactivated_preempted_queue() {
        let report = search_from(
            Rule::Current,
            reactivation_bounds(),
            &placement_inits_with(0),
        );
        assert_after_window_only(&report);
    }

    /// The current rule over two and three queues, everything allowed. The
    /// slice end's step puts the preempted queue behind any queue the window
    /// wakes that was charged the same, so nothing ties or is reversed in the
    /// window. After it, a preempted task woken only then is placed as usual,
    /// just behind the least vruntime, which closing the window lowered to a
    /// queue it placed at the earliest mark. A queue it placed at the usual
    /// mark can then sit behind it, a strict reversal: from seed `[4, 0, 7]`,
    /// the queue charged 7 is placed at the usual mark, the one charged 0 at
    /// the earliest, and the preempted task, woken by the latter's task,
    /// lands ahead of the former. Level with it, the queue placed at the
    /// usual mark entered the heap first and wins the tie.
    ///
    /// A tie remains only where a slice of that queue is preempted before
    /// its first poll: the model charges such a slice nothing, and the queue
    /// goes back into the heap behind the preempted one. The code charges
    /// the time the slice took, which makes that a strict reversal too.
    #[test]
    fn current_rule_three_queues() {
        let report = search_from(Rule::Current, three_queue_bounds(), &placement_inits());
        assert_after_window_only(&report);
        assert!(
            report.found.contains_key(&Property::BoundAfterWindow),
            "{report}"
        );
    }

    /// The current rule over two and three queues with no slice preempted
    /// before its first poll: no tie at all, after the window or in it.
    #[test]
    fn current_rule_three_queues_without_empty_slices() {
        let bounds = Bounds {
            early_preempt: false,
            ..three_queue_bounds()
        };
        let report = search_from(Rule::Current, bounds, &placement_inits());
        let found: Vec<_> = report.found.keys().copied().collect();
        assert_eq!(found, [Property::BoundAfterWindow], "{report}");
    }

    /// Before queues of equal vruntime ran in the order they entered the
    /// heap, the heap could also run the preempted task ahead of a queue
    /// placed level with it at the usual mark.
    #[test]
    fn ties_either_way_three_queues() {
        let report = search_from(
            Rule::TiesEitherWay,
            three_queue_bounds(),
            &placement_inits(),
        );
        let found: Vec<_> = report.found.keys().copied().collect();
        assert_eq!(
            found,
            [Property::BoundAfterWindow, Property::BoundAfterWindowTie],
            "{report}"
        );
        let t = trace(&report, Property::BoundAfterWindowTie);
        assert!(last_event(t).contains("wins a vruntime tie"), "{report}");
    }

    /// Ties that go to the queue that entered the heap last run the preempted
    /// task, reactivated after the window, ahead of the queue placed level
    /// with it.
    #[test]
    fn finds_the_after_window_tie_when_the_last_entered_wins() {
        let bounds = Bounds {
            early_preempt: false,
            ..three_queue_bounds()
        };
        let report = search_from(Rule::LastEnteredFirst, bounds, &placement_inits());
        trace(&report, Property::BoundAfterWindowTie);
    }

    /// The current rule with a queue whose slices cost it ten times as much:
    /// a reversal after the window, and nothing else.
    #[test]
    fn current_rule_low_share_queue() {
        let report = search_from(Rule::Current, three_queue_bounds(), &low_share_inits());
        assert_after_window_only(&report);
    }

    /// The same before ties went by heap entry. The tie takes nine events;
    /// the strict reversal takes thirteen, past the debug bound.
    #[test]
    fn ties_either_way_low_share_queue() {
        let report = search_from(
            Rule::TiesEitherWay,
            three_queue_bounds(),
            &low_share_inits(),
        );
        assert!(
            report.found.contains_key(&Property::BoundAfterWindowTie),
            "{report}"
        );
        assert!(
            report.found.keys().all(|p| matches!(
                p,
                Property::BoundAfterWindow | Property::BoundAfterWindowTie
            )),
            "{report}"
        );
    }

    /// Fails on any violation but the two after the window, which it has no
    /// say in, and on a tie there that a slice preempted before its first
    /// poll did not bring about.
    fn assert_after_window_only(report: &Report) {
        assert!(
            report.found.keys().all(|p| matches!(
                p,
                Property::BoundAfterWindow | Property::BoundAfterWindowTie
            )),
            "{report}"
        );
        if let Some((_, t)) = report.found.get(&Property::BoundAfterWindowTie) {
            assert!(
                t.iter()
                    .any(|l| l.contains("slice start] the preempt timer fires")),
                "{report}"
            );
        }
    }

    /// Fails on any violation but the one nested inline polls are known to
    /// cause: a task at a reused address waits a slice per nested inline task.
    fn assert_only_known(report: &Report) {
        let unknown: Vec<_> = report
            .found
            .keys()
            .filter(|p| **p != Property::AddressReuseSlices)
            .collect();
        assert!(unknown.is_empty(), "{report}");
    }

    /// The preempt timer fires after a slice is picked and before its first
    /// poll: the slice ends having polled nothing, opens no window and is
    /// charged no step.
    #[test]
    fn current_rule_preempted_before_first_poll_opens_no_window() {
        let report = search(Rule::Current, needs(3, 1));
        assert!(
            report.notes.contains_key(&Note::PreemptedBeforeFirstPoll),
            "{report}"
        );
        assert!(report.found.is_empty(), "{report}");
    }

    /// A timer waker runs before the rings are drained, while the preempt
    /// completion of the slice just preempted is still pending: a task it
    /// spawns samples that preemption, and its inline poll opens or extends
    /// no window.
    #[test]
    fn current_rule_pre_drain_spawn_opens_no_window() {
        let report = search(Rule::Current, needs(6, 1));
        assert!(report.notes.contains_key(&Note::PreDrainSpawn), "{report}");
        assert!(report.found.is_empty(), "{report}");
    }

    /// The current rule with one level of inline polls.
    #[test]
    fn current_rule_one_inline_level() {
        assert_only_known(&search_from(Rule::Current, full_bounds(1), &inits(1, 2)));
    }

    /// The current rule with an inline poll inside an inline poll.
    #[test]
    fn current_rule_two_inline_levels() {
        assert_only_known(&search_from(Rule::Current, full_bounds(2), &inits(1, 2)));
    }

    /// The current rule with three nested inline polls, which needs a third
    /// spawn.
    #[test]
    fn current_rule_three_inline_levels() {
        assert_only_known(&search_from(Rule::Current, full_bounds(3), &inits(1, 3)));
    }

    /// Queues tied at the top of the vruntime range are all candidates.
    #[test]
    fn a_tie_at_the_top_of_the_vruntime_range_keeps_every_queue() {
        let budget = Budget::default();
        let mut s = initial(&[0, 0], &[seed(0, true), seed(1, true)], true, budget);
        s.queues[0].v = u8::MAX;
        s.queues[1].v = u8::MAX;
        let model = Model::new(Rule::Current, bounds(1));
        let x = Exec {
            m: &model,
            s,
            viol: Vec::new(),
            notes: 0,
        };
        assert_eq!(x.min_candidates().1, 0b11);
    }

    /// An earlier design cleaned up a wake of a task whose queue is gone
    /// through the window's hold. With two executors that could strand the
    /// default queue: a hold during its slice counts against the removed
    /// queue. With one, a window during a slice remembers only the running
    /// queue's tasks, which cannot be removed while it runs, and a hold after
    /// the slice activates the default queue it was scheduled on. So the path
    /// is reached here and breaks nothing.
    #[test]
    fn cleanup_through_hold_needs_two_executors_to_strand() {
        let report = search_from(Rule::CleanupThroughHold, full_bounds(1), &inits(1, 2));
        assert!(
            report.notes.contains_key(&Note::RememberedCleanup),
            "{report}"
        );
        assert!(report.found.is_empty(), "{report}");
    }

    /// The tripwire: every item the model mirrors still has the source it
    /// had when the model was last brought up to date with it.
    #[test]
    fn mirrored_code_is_unchanged() {
        let print = std::env::var_os("WINDOW_MODEL_PRINT_FINGERPRINTS").is_some();
        let mut failures = Vec::new();
        for (file, start, expected) in MIRRORED {
            let source = SOURCES
                .iter()
                .find(|(name, _)| name == file)
                .map(|(_, source)| *source)
                .expect("a mirrored file is in SOURCES");
            match mirrored_item(source, start) {
                Err(e) => failures.push(format!(
                    "{file}: {e}. The preempted-window rule may have moved: update MIRRORED in \
                     window_model.rs"
                )),
                Ok(item) => {
                    let actual = source_fingerprint(item);
                    if print {
                        println!("    (\"{file}\", \"{start}\", 0x{actual:016x}),");
                    }
                    if actual != *expected {
                        failures.push(format!(
                            "the preempted-window rule changed in `{start}` ({file}): update \
                             window_model.rs to mirror it, rerun the model, then update the \
                             fingerprint (print the new values with \
                             WINDOW_MODEL_PRINT_FINGERPRINTS=1)"
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn fingerprint_ignores_layout_but_not_logic() {
        let source = "impl X {\n    fn f(a: u8, b: u8) -> u8 {\n        a.saturating_add(b).min(1)\n    }\n}\n";
        let reflowed = "impl X {\n    /// Docs do not count.\n    fn f(\n        a: u8,\n        b: u8,\n    ) -> u8 {\n        a.saturating_add(b)\n            .min(1)\n    }\n}\n";
        let changed = "impl X {\n    fn f(a: u8, b: u8) -> u8 {\n        a.saturating_add(b).min(2)\n    }\n}\n";
        let print = |s: &str| source_fingerprint(mirrored_item(s, "fn f(").unwrap());
        assert_eq!(print(source), print(reflowed));
        assert_ne!(print(source), print(changed));
        assert!(mirrored_item(source, "fn g(").is_err());
        assert!(mirrored_item("fn f() {}\nfn f() {}\n", "fn f(").is_err());
        assert_eq!(
            mirrored_item("fn f() { \"}\" }\n", "fn f(").unwrap(),
            "fn f() { \"}\" }"
        );
    }

    /// A brace in a comment or a literal inside an item does not end it.
    #[test]
    fn item_extraction_skips_comments_and_literals() {
        let source = "fn f<'a>(x: &'a str) {\n    /// a doc line with a } in it\n    // a comment with a }\n    let c = '}';\n    let s = \"}\";\n    let r = r#\"}\"#;\n    let b = br#\"a\"}\"#;\n    g(c, s, r, b, x)\n}\nfn h() {}\n";
        let item = mirrored_item(source, "fn f<").unwrap();
        assert!(item.ends_with("g(c, s, r, b, x)\n}"), "{item}");
    }

    /// Literal contents and 1-tuples survive normalization; optional
    /// trailing commas do not.
    #[test]
    fn fingerprint_keeps_literals_and_one_tuples() {
        let print = |s: &str| source_fingerprint(mirrored_item(s, "fn f(").unwrap());
        assert_ne!(
            print("fn f() { g(\"a b\") }"),
            print("fn f() { g(\"ab\") }")
        );
        assert_ne!(print("fn f() { g((x,)) }"), print("fn f() { g((x)) }"));
        assert_eq!(print("fn f() { g(x,) }"), print("fn f() { g(x) }"));
        assert_eq!(print("fn f() { [1, 2,] }"), print("fn f() { [1, 2] }"));
        assert_ne!(
            print("fn f() { let mut x = 1; }"),
            print("fn f() { let mutx = 1; }")
        );
    }
}
