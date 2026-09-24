#[glommio_macros::main]
async fn generic<T: Default>() -> T {
    T::default()
}

fn main() {}
