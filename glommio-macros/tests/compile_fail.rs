//! The macro's error messages are the interface a caller meets when they get
//! it wrong, so they are asserted rather than left to drift.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
