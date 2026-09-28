//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
#![cfg(feature = "macros")]
//! `#[glommio::test]` from the position a user writes it in.
//!
//! The unit tests under `src/` cannot use these attributes, because `::glommio`
//! does not resolve inside the crate that defines it, so they keep using
//! `test_executor!`.

use glommio::CpuSet;

/// The CPUs the calling *thread* is allowed to run on, as the kernel sees them.
///
/// `Placement` works by setting the thread's affinity mask, so this is the one
/// check that cannot pass when the placement argument is parsed and then
/// dropped on the floor.
fn thread_cpus_allowed() -> Vec<usize> {
    parse_cpu_list(&status_field("/proc/thread-self/status"))
}

/// The same list for the process, which means its main thread.
///
/// `make()` builds the executor on the calling thread, and libtest gives each
/// test its own thread, so the main thread keeps whatever mask the runner
/// started with. That makes it the baseline to compare a placement against.
fn process_cpus_allowed() -> Vec<usize> {
    parse_cpu_list(&status_field("/proc/self/status"))
}

fn status_field(path: &str) -> String {
    let status = std::fs::read_to_string(path).unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .unwrap_or_else(|| panic!("Cpus_allowed_list missing from {path}"))
        .trim()
        .to_string()
}

/// Parses the `Cpus_allowed_list` format, which is comma-separated singletons
/// and inclusive ranges: `0`, `0-3`, `0,2-4`.
fn parse_cpu_list(list: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in list.split(',') {
        match part.split_once('-') {
            Some((lo, hi)) => {
                let (lo, hi): (usize, usize) = (lo.parse().unwrap(), hi.parse().unwrap());
                cpus.extend(lo..=hi);
            }
            None => cpus.push(part.parse().unwrap()),
        }
    }
    cpus
}

#[glommio::test]
async fn runs_an_async_body() {
    let value = glommio::spawn_local(async { 21u32 * 2 }).await;
    assert_eq!(value, 42, "the body ran on an executor");
}

/// The body runs on the thread the executor was built on, so the kernel's own
/// view of that thread's affinity is what says whether `placement` reached the
/// builder or was quietly accepted and ignored.
///
/// On a single-CPU runner or a cpuset-limited container the baseline is already
/// `[0]`, and an honoured `Fixed(0)` is then indistinguishable from an ignored
/// one, so skip rather than pass for the wrong reason.
#[glommio::test(placement = Fixed(0))]
async fn honours_a_placement() {
    if process_cpus_allowed() == [0] {
        eprintln!(
            "skipping honours_a_placement: this runner is already confined to CPU 0, so \
             Fixed(0) cannot be told apart from Unbound here"
        );
        return;
    }

    assert_eq!(
        thread_cpus_allowed(),
        [0],
        "Fixed(0) did not pin the executor thread"
    );
}

#[glommio::test]
async fn returns_a_result() -> Result<(), std::io::Error> {
    Ok(())
}

/// The tokens after `placement =` are emitted with `::glommio::Placement::`
/// prepended, so the variant is fixed at the attribute and its argument is
/// not. That is what makes choosing exact cores work without the attribute
/// having to know anything about `CpuSet`.
#[glommio::test(placement = Fenced(CpuSet::online().unwrap().filter(|l| l.cpu < 2)))]
async fn fenced_to_chosen_cores() {
    if process_cpus_allowed().len() <= 2 {
        eprintln!(
            "skipping fenced_to_chosen_cores: this runner has no CPU outside the fence, so \
             the fence cannot be told apart from Unbound here"
        );
        return;
    }

    let allowed = thread_cpus_allowed();
    assert!(
        allowed.iter().all(|&cpu| cpu < 2),
        "the fence let the executor thread onto {allowed:?}, which reaches past CPU 1"
    );
}

/// Selection is on a `CpuLocation`, so NUMA node and package work the same way
/// as the cpu index does. Asserting which CPUs that should be would hardcode
/// this machine's topology, so this one only has to build and run.
#[glommio::test(placement = Fenced(CpuSet::online().unwrap().filter(|l| l.numa_node == 0)))]
async fn fenced_to_a_numa_node() {
    glommio::timer::sleep(std::time::Duration::from_millis(1)).await;
}

/// The expansion emits a plain `#[test]`, so the harness attributes compose
/// without this macro knowing anything about them.
#[glommio::test]
#[should_panic(expected = "deliberate")]
async fn should_panic_composes() {
    glommio::timer::sleep(std::time::Duration::from_millis(1)).await;
    panic!("deliberate");
}

#[glommio::test]
#[ignore = "composes with ignore"]
async fn ignore_composes() {
    unreachable!("ignored tests do not run");
}

/// And in the other order, since attribute order is a fair thing to get wrong.
#[should_panic(expected = "deliberate")]
#[glommio::test]
async fn should_panic_composes_either_order() {
    panic!("deliberate");
}

/// A placement that is not a bare variant name reaches the expansion as
/// written, which is what `Fixed` and `Fenced` need when the value is computed
/// rather than spelled out.
fn computed_placement() -> glommio::Placement {
    glommio::Placement::Fixed(0)
}

#[glommio::test(placement = computed_placement())]
async fn a_computed_placement_is_emitted_as_written() {
    assert_eq!(thread_cpus_allowed(), vec![0]);
}

#[glommio::test(placement = glommio::Placement::Fixed(0))]
async fn a_fully_qualified_placement_is_left_alone() {
    assert_eq!(thread_cpus_allowed(), vec![0]);
}

#[glommio::test(placement = Fixed(0))]
async fn a_bare_variant_is_still_shorthand() {
    assert_eq!(thread_cpus_allowed(), vec![0]);
}
