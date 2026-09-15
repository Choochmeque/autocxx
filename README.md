# autocxx

[![crates.io](https://img.shields.io/crates/v/autocxx)](https://crates.io/crates/autocxx)
[![docs.rs](https://docs.rs/autocxx/badge.svg)](https://docs.rs/autocxx)
[![CI](https://github.com/Choochmeque/autocxx/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Choochmeque/autocxx/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/autocxx)](#license)

> [!NOTE]
> This is the actively maintained continuation of [google/autocxx](https://github.com/google/autocxx), which is no longer maintained by Google (see [google/autocxx#1507](https://github.com/google/autocxx/issues/1507)). Bug reports, feature requests and pull requests are welcome here.

autocxx generates Rust bindings from existing C++ headers. You name the types and functions you want; the engine runs a patched copy of [bindgen](https://github.com/rust-lang/rust-bindgen) over your headers and lowers the result onto [cxx](https://cxx.rs). Calls cross the language boundary under cxx's safety model, without a hand-written `#[cxx::bridge]` for every API; how much of that surface is `unsafe` on the Rust side is up to the [`safety!`](https://autocxx.dev/safety.html) directive you choose.

```rust,ignore
use autocxx::prelude::*;

include_cpp! {
    #include "url/origin.h"
    safety!(unsafe_ffi)
    generate!("url::Origin")
}

fn main() {
    let o = ffi::url::Origin::CreateFromNormalizedTuple("https",
        "google.com", 443);
    let uri = o.Serialize();
    println!("URI is {}", uri.to_str().unwrap());
}
```

## Getting started

Install `libclang` (autocxx uses it to parse your headers), then add to `Cargo.toml`:

```toml
[dependencies]
autocxx = "0.30"
cxx = "1.0"

[build-dependencies]
autocxx-build = "0.30"
miette = { version = "5", features = ["fancy"] }
```

`build.rs`, which generates the bindings and compiles the C++ side:

```rust,ignore
fn main() -> miette::Result<()> {
    let mut b = autocxx_build::Builder::new("src/main.rs", ["src"]).build()?;
    b.std("c++14").compile("my-crate");
    println!("cargo:rerun-if-changed=src/main.rs");
    Ok(())
}
```

Then an `include_cpp!` block in your Rust source, as in the example above; the headers it includes must sit in the directories passed to `Builder::new`. The [tutorial](https://autocxx.dev/tutorial.html) walks through this in full, including linking against an existing C++ library. [demo/](demo/) is the smallest working project; [examples/](examples/) exercises real codebases such as S2 and LLVM, plus specific techniques such as subclassing and reference wrappers. For non-Cargo builds there is a standalone generator, `autocxx-gen`.

## Documentation

- [The manual](https://autocxx.dev/) covers the directives, the [safety policy](https://autocxx.dev/safety.html), storage of non-trivial C++ types, [C++ exceptions](https://autocxx.dev/exceptions.html), subclassing and the C++ constructs that cannot be represented.
- [docs.rs](https://docs.rs/autocxx) documents the macros and helper types.

## Supported platforms

Linux, macOS and Windows (MSVC and GNU), all tested in CI. The minimum supported Rust version is 1.88.

## Contributing

`cargo test --workspace` runs the main suite; the [contributing chapter](https://autocxx.dev/contributing.html) of the manual describes the code layout and how the engine's phases fit together. For bug reports, `tools/reduce` can shrink a failing preprocessed header to a minimal repro.

## License

This project is not an officially supported Google product.

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT license](LICENSE-MIT) at your option. The one exception is the vendored copy of [bindgen](https://github.com/rust-lang/rust-bindgen) which the published `autocxx-engine` crate carries (and patches at build time): that code alone is BSD-3-Clause, with its license included alongside the sources. Everything written for autocxx itself remains MIT/Apache-2.0.
