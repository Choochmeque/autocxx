// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
#![no_main]

use libfuzzer_sys::fuzz_target;

// Fuzzes the directive parser for the body of the `include_cpp!` macro
// (`autocxx_parser::IncludeCpp`, i.e. the `generate!`, `safety!`,
// `include!`, `extern_cpp_type!` etc. directives a user writes between the
// braces). This is the very first thing arbitrary user input goes through -
// well before anything reaches bindgen or clang - so it's a good fuzz
// target:
//
// * it's pure `syn`/`proc_macro2` token-tree parsing, so it's fast and
//   entirely in-process (no filesystem, no subprocess, no clang);
// * `autocxx-parser` is `#![forbid(unsafe_code)]`, so any crash found here
//   is a pure logic bug (a `panic!`/`unwrap`/`unreachable!`/out-of-bounds
//   index in this crate), never memory unsafety - a panic is still a real
//   bug, though, since a proc macro that panics gives the user an opaque
//   "proc macro panicked" error instead of the crate's normal, specific
//   diagnostics;
// * it's the one place we can meaningfully fuzz at all without dragging in
//   libclang: the rest of the pipeline (bindgen/clang parsing the actual
//   C++ header) is effectively fuzzing clang itself, which is a different
//   (and far heavier) project.
//
// `data` is treated as source text for the token stream that would
// normally appear inside `include_cpp! { ... }`; `Arbitrary`'s `&str` impl
// takes the longest valid-UTF-8 prefix of the fuzzer's raw bytes, so most
// inputs are lexically nonsensical and are expected to bail out in
// tokenization or in `IncludeCpp::parse` with an ordinary `syn::Error` -
// the only thing this harness asserts is that we never panic.
//
// Run with `cargo fuzz run parse_include_cpp` from `parser/fuzz`. That
// needs `cargo-fuzz` (`cargo install cargo-fuzz`) and a nightly toolchain;
// neither is installed in this checkout, so this target has only been
// verified with `cargo check` here - see `parser/fuzz/README.md` for what
// that did and didn't confirm.
fuzz_target!(|data: &str| {
    let _ = syn::parse_str::<autocxx_parser::IncludeCpp>(data);
});
