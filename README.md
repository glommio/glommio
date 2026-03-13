# glommio

<!--toc:start-->
- [glommio](#glommio)
  - [New Fork](#new-fork)
  - [What is Glommio?](#what-is-glommio)
  - [Supported Rust Versions](#supported-rust-versions)
  - [Supported Linux kernels](#supported-linux-kernels)
  - [Contributing](#contributing)
  - [License](#license)
<!--toc:end-->

## New Fork

Welcome to the new hard fork of Glommio. Some story why it was forked can be found [here](https://github.com/DataDog/glommio/issues/707),
while TL;DR is - in this fork we are going to keep glommio up to date with fresh versions of io_uring and other dependencies.

## What is Glommio?

Glommio (pronounced glo-mee-jow or |glomjəʊ|) is a Cooperative Thread-per-Core crate for Rust & Linux based
on `io_uring`. Like other rust asynchronous crates, it allows one to write asynchronous code that takes advantage of
rust `async`/`await`, but unlike its counterparts, it doesn't use helper threads anywhere.

Using Glommio is not hard if you are familiar with rust async. All you have to do is:

```rust
use glommio::prelude::*;

LocalExecutorBuilder::default().spawn(|| async move {
    /// your async code here
})
.expect("failed to spawn local executor")
.join();
```

For more details check out our [docs page](https://docs.rs/glommio/latest/glommio/) and
an [introductory article.](https://www.datadoghq.com/blog/engineering/introducing-glommio/)

## Supported Rust Versions

Glommio is built against the latest stable release. The minimum supported version is 1.92. The current Glommio version
is not guaranteed to build on Rust versions earlier than the minimum supported version.

## Supported Linux kernels

Glommio requires a kernel with a recent enough `io_uring` support, at least current enough to run discovery probes. The
minimum version at this time is 5.8.

Please also note Glommio requires at least 512 KiB of locked memory for `io_uring` to work. You can increase the
`memlock` resource limit (rlimit) as follows:

```sh
$ vi /etc/security/limits.conf
*    hard    memlock        512
*    soft    memlock        512
```

> Please note that 512 KiB is the minimum needed to spawn a single executor. Spawning multiple executors may require you
> to raise the limit accordingly.

To make the new limits effective, you need to log in to the machine again. You can verify that the limits are updated by
running the following:

```sh
$ ulimit -l
512
```

## Contributing

See [](/CONTRIBUTING.md)

## License

Licensed under either of

* Apache License, Version 2.0 ([](/LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([](/LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
