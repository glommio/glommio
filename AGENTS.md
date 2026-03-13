# AGENTS

## Build, lint and test commands for the Glommio repository

The following commands are what an agent should use when it needs to compile, lint or run tests.
They match the CI workflow defined in `.github/workflows/ci.yml`, but are expressed as single‑line commands
that can be executed from any shell.  Using these ensures that agents behave exactly the same as a human would
in a continuous‑integration environment.

### General build
- To compile *all* crates (examples, library, benches) and all targets:
```bash
cargo build --all --all-targets
```
- When you want to check for failed builds locally before pushing you can run the same command with the `--verbose` flag which provides more context.

### Formatting & style checks
The project uses `rustfmt`.  Agents should use the checked‑only mode unless they specifically intend to modify the sources.
```bash
# Check formatting without modifying files. Fails if any file is not formatted.
cargo fmt --all -- --check
```

When a format change is required agent can simply run:
```bash
cargo fmt --all
```
which rewrites everything in place.

### Linting with Clippy
Clippy catches many subtle bugs and code‑style issues.  The CI does not enable the `-D warnings` flag, but agents should be
encouraged to treat warnings as errors so that potential problems are caught early:
```bash
cargo clippy --all-targets -- -D warnings
```
If you need to auto‑apply certain lint hints you can use the `--fix` option.

### Tests
#### Run all tests
Agent needs a quick sanity check after a change:
```bash
cargo test
```
The CI runs tests with multiple threads, which is the default behaviour.

#### Run a single test
For pinpointing regressions you can run an individual test by matching its name.  The pattern matcher is case‑sensitive and supports globbing:
```bash
# Run the first test named "does_something" in any module.
cargo test does_something
```
If there are multiple tests with that partial match Cargo will list them or run all of them, depending on the version.  For an exhaustive match use the fully‑qualified name:
```bash
cargo test glommio::task::tests::test_waker_fn_ready
```
This runs only the `test_waker_fn_ready` test found in `glommio/src/task/tests.rs`.

#### Run tests with verbose output
When debugging failing tests, it is useful to see stack traces and test names:
```bash
cargo test -- --nocapture
```
The `--nocapture` flag tells the test harness not to swallow `stdout`/`stderr`, so you can read logs directly.

### Benchmarks (optional)
If an agent wants to measure performance changes it may run the built‑in benches:
```bash
cargo bench --bench executor  # runs benchmark in benches/executor.rs
```
The CI does not currently build the benchmarks, but agents are free to do so locally.

## Code style guidelines
Below we summarise conventions that agents should follow when they edit Rust sources. These are derived from the repository’s CI configuration, `deny.toml`, and common Rust idioms.

### 1. Imports & modules
-   **Grouping order** – `extern crate`s first (if any), then `std::` imports, third‑party crates, finally local relative imports (`use super::*;`).
-   **Alphabetically sorted within each section** and separated by a blank line.
-   **Avoid glob imports** (`use foo::*`) unless they are well documented.

### 2. Formatting & whitespace
-   The repository enforces `rustfmt`.  Indent with **4 spaces**, no tabs.
-   Trailing commas are encouraged in function arguments and macro calls.
-   Long lines should be wrapped keeping Rust's style guide in mind (no line longer than ~80–100 chars when practical).

### 3. Naming conventions
-   **Functions, methods & fields** – `snake_case`.
-   **Constants & statics** – `UPPER_SNAKE_CASE`.
-   **Types & traits** – `PascalCase`.
-   **Module names** – `snake_case` and usually short but descriptive (e.g. `task`, `timer`).

### 4. Error handling
-   Prefer returning `Result<T, E>` or using the `?` operator instead of panicking.
-   In non‑public API code (`#[cfg(test)]` and examples) it is acceptable to use `.unwrap()` or `.expect("…")` when a failure represents a bug in the repository itself.  Use descriptive panic messages.
-   For unrecoverable situations (invalid arguments that should never happen on valid inputs), a `panic!` with a clear message is fine.

### 5. Documentation & comments
-   Public items (`pub`) must have doc‑comments (`///`).
-   Module docs go at the start of each `mod.rs` or `lib.rs`. They should briefly describe the module’s purpose.
-   Inline comments are allowed but should be concise; avoid duplicating information that docs already provide.

### 6. Testing helpers
-   Tests live in files ending with `_tests.rs` (see `glommio/src/task/tests.rs`).
-   Use `#[test]` functions and small helper functions marked with `#[cfg(test)]` for test‑only utilities.
-   Keep tests deterministic – do not rely on environment state or external resources unless absolutely necessary.

### 7. Performance hints
-   Avoid unnecessary allocations: prefer stack where possible and use `&[T]` over owned collections in low‑level APIs.
-   When a function is used as a callback (e.g., an async closure) annotate it with the correct lifetimes; agents should never create `Box<dyn Fn(...)>` unless required for dynamic dispatch.

### 8. Cargo / dependency hygiene
-   New dependencies must appear in `[dependencies]` in `glommio/Cargo.toml`. They should have a minimal feature set enabled.
-   If a crate is only used for dev‑time or tests, it belongs under `[dev-dependencies]`.
-   Run `cargo deny check licenses` regularly; the project already allows MIT/Apache‑2.0/etc. Avoid any disallowed licenses.

## Cursor and Copilot rules
The repository contains **no** `.cursor/rules`, `/ .cursorrules`, or `.github/copilot-instructions.md`. If such files are added later, agents should follow the provided YAML format; until then standard Rust conventions apply.

---

> *This file has been written to assist automated agents in performing reliable builds, tests and code‑style compliance in line with the repository’s CI pipeline.*
