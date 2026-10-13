// `name` reaches the builder but `make()` never reads it, so the attribute
// rejects it rather than accepting an option it cannot honour.
#[glommio_macros::main(name = "server")]
async fn named() {}

fn main() {}
