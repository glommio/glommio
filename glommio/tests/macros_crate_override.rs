//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
#![cfg(feature = "macros")]
//! The `crate = …` escape hatch, from the position a user needs it.
//!
//! The expansion names the runtime by path, and that path is `::glommio` unless
//! told otherwise. Anyone who renames the dependency in their `Cargo.toml`, or
//! reaches glommio through a facade crate, has no `::glommio` to resolve. This
//! file stands in for that caller by renaming the crate here.
//!
//! This is the positive half of the argument's coverage: it shows the override
//! compiles and runs. The negative half is
//! `glommio-macros/tests/ui/crate_override_is_honoured.rs`, which points the
//! argument at a crate that does not exist and requires the compile to fail,
//! proving the path is emitted rather than ignored.

extern crate glommio as glommio_ng;

#[glommio_ng::test(crate = glommio_ng)]
async fn runs_under_an_aliased_crate_name() {
    let answer = glommio_ng::spawn_local(async { 7u32 }).await;
    assert_eq!(answer, 7);
}

#[glommio_ng::test(crate = glommio_ng, placement = Fixed(0))]
async fn accepts_crate_alongside_a_placement() {
    glommio_ng::timer::sleep(std::time::Duration::from_millis(1)).await;
}

#[glommio_ng::main(crate = glommio_ng)]
async fn aliased_main() -> u32 {
    glommio_ng::spawn_local(async { 11u32 }).await
}

/// `main` is an ordinary function once expanded, so a test can call the one
/// above and check the attribute did not swallow its return value.
#[test]
fn main_attribute_takes_the_crate_override_too() {
    assert_eq!(aliased_main(), 11);
}
