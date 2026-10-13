// If `crate = …` reaches the expansion, the emitted path names this crate and
// fails to resolve. If the argument were ignored, this would compile against
// the default path -- so the failure below is the proof that it is honoured.
#[glommio_macros::main(crate = definitely_not_a_real_crate)]
async fn main() {}
