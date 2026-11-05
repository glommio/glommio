# Dense registry

A packed registry with drop-based registration handles, randomized insertion,
and deferred mutations during iteration. Slot IDs use `u8`, `u16`, `u32`, or
`usize`; the default `u8` allows 256 simultaneous registrations. Values remain
contiguous for iteration while registrations stay valid when values move.

The registry implementation, ordinary tests, and Kani proof harnesses live in
`src/lib.rs`. The inline `proofs` module is compiled only under `cfg(kani)`.

Install the same verifier version used by the `Registry proofs` CI job (triggered by changes under this crate):

```sh
cargo install --locked kani-verifier --version 0.68.0
cargo kani setup
```

From the repository root, run all harnesses:

```sh
cargo kani -p glommio-dense-registry -Z stubbing --output-format terse --jobs 2
```

To run just one harness, add `--harness batch_does_not_reuse_the_current_slot`.
To run the ordinary tests:

```sh
cargo test -p glommio-dense-registry
```

## Properties and bounds

The harnesses check these specific states and transitions:

| Harnesses | Exhaustive choices and assertions |
| --- | --- |
| `insert_u8`, `insert_u16`, `insert_u32`, `insert_usize` | Every bijection between three dense indices and slots 0–2, and every insertion position for a fourth value. Dense and slot tables remain inverses; each payload keeps its slot and appears once. |
| `remove_u8`, `remove_u16`, `remove_u32`, `remove_usize` | Every such bijection and every live removal. The removed value matches its registration, the remaining payloads keep their slots, and the removed slot becomes the free-list head. The initial free list is empty. |
| `shuffle_preserves_ownership` | Every three-member bijection and every permitted swap destination. The first value moves, the tables remain inverses, and payloads keep their slots. |
| `batch_does_not_reuse_the_current_slot`, `batch_does_not_reuse_a_later_slot` | A two-member batch, either member removed during its first visit, and insertion during that visit. Both original members run exactly once; the new registration gets a distinct slot. After the batch, the removed slot becomes the free-list head and a subsequent insertion reuses it. The tables remain inverses and payloads keep their slots. All permitted random positions are explored. |
| `free_slot_registration_keeps_live_ids` | Two live members in slots 0–1 and a free slot 2. Registration consumes the free slot, preserves existing payload-to-slot associations, and leaves no free slots. All insertion positions are explored. |
| `handles_can_outlive_the_registry` | Dropping a two-member registry before its weak registration handle does not keep the registry alive or access released storage. |
| `default_registry_has_no_live_or_free_slots` | Construction starts with empty tables and no pending mutations or free slots. |

Only the RNG is stubbed: seed creation is deterministic and each generated index
is nondeterministic within the requested range. The stub asserts that the range
fits its symbolic `u8` domain before restricting the index, so expanding a
harness beyond that domain fails verification instead of silently omitting
indices. These proofs make no claim about the RNG's distribution.

Batch harnesses invoke the production removal method and `State::insert` from
inside `for_each`; the free-slot harness also invokes `State::insert` directly.
These check the same state transitions used by registration handles, without
combining them with `Rc` bookkeeping. Ordinary tests exercise the registration
wrapper and handle drops; weak-handle lifetime has its own harness.

Kani's default safety and unwinding checks remain enabled. Each harness declares
its loop-unwinding bound; exceeding it fails verification. Cover assertions check
that the symbolic insertion and removal fixtures admit an execution.

These are bounded proofs of the listed transitions, not an inductive proof for
every capacity or arbitrarily long operation history. Panic unwinding, reentrant
payload destructors, longer mutation sequences, and the full `u8` boundary (256
successful live registrations, followed by a rejected 257th insertion) have
ordinary regression tests. They are not established by these harnesses.
