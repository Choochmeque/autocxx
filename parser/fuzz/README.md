Fuzz target for `autocxx-parser`, tracking [issue #1244](https://github.com/google/autocxx/issues/1244).

`fuzz_targets/parse_include_cpp.rs` feeds arbitrary strings to
`autocxx_parser::IncludeCpp`'s `syn::parse::Parse` implementation - the code
that parses the directives inside `include_cpp! { ... }` (`generate!`,
`safety!`, `include!`, and so on). That's the first thing arbitrary macro
input reaches, it's pure in-process token-tree parsing (no clang/bindgen), and
`autocxx-parser` is `#![forbid(unsafe_code)]`, so any crash found here is a
plain logic bug, not memory unsafety.

## Running it

This needs [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz)
(`cargo install cargo-fuzz`) and a nightly toolchain, per cargo-fuzz's own
requirements - neither is installed in this checkout, and this change doesn't
install either, so it hasn't been run through the real `cargo fuzz` CLI. From
`parser/fuzz`:

```
cargo fuzz run parse_include_cpp
```

`fuzz` deliberately isn't a member of the main workspace (see the `Cargo.toml`
here), so it's untouched by `cargo build --workspace`/`cargo test
--workspace`/CI and only comes into play if you cd into this directory.

## What's been verified without `cargo-fuzz`

From this directory, both `cargo check` and `cargo build` (plain stable
Rust, no `cargo-fuzz` and no sanitizer/coverage flags) succeed, and the
resulting binary (`target/debug/parse_include_cpp`) runs correctly as a
libFuzzer harness - `./target/debug/parse_include_cpp -runs=500
some-empty-dir` completes 500 iterations against the real parser with no
crash. It logs that it isn't coverage-instrumented (expected: that
instrumentation is exactly what `cargo fuzz run` adds via nightly-only
flags), so this confirms the harness itself is wired correctly end to end,
not that it fuzzes efficiently - for real coverage-guided fuzzing, use
`cargo fuzz run` as above.
