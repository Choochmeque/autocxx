// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
#![no_main]

use libfuzzer_sys::fuzz_target;
use quote::ToTokens;

// Fuzzes the directive parser for the body of the `include_cpp!` macro
// (the `generate!`, `safety!`, `include!`, `extern_cpp_type!` etc.
// directives a user writes between the braces) and the config object it
// produces. This is the very first thing arbitrary user input goes through -
// well before anything reaches bindgen or clang - so it's a good fuzz
// target:
//
// * it's pure `syn`/`proc_macro2` token-tree parsing, so it's fast and
//   entirely in-process (no filesystem, no subprocess, no clang);
// * what it mostly finds is panics and resource exhaustion, and both
//   matter: a proc macro that panics gives the user an opaque "proc macro
//   panicked" error instead of this crate's normal, specific diagnostics,
//   and one that hangs or allocates without bound is just as broken.
//   `autocxx-parser` forbids unsafe code, so a crash inside *it* is a
//   logic bug rather than memory unsafety - but that says nothing about
//   `syn`, `proc_macro2` or the allocator underneath, which is why the CI
//   job keeps cargo-fuzz's ASan on;
// * it's the one place we can meaningfully fuzz at all without dragging in
//   libclang: the rest of the pipeline (bindgen/clang parsing the actual
//   C++ header) is effectively fuzzing clang itself, which is a different
//   (and far heavier) project.
//
// `data` is treated as source text for the token stream that would
// normally appear inside `include_cpp! { ... }`; `Arbitrary`'s `&str` impl
// takes the longest valid-UTF-8 prefix of the fuzzer's raw bytes, so most
// inputs are lexically nonsensical and are expected to bail out in
// tokenization or in the `Parse` impl with an ordinary `syn::Error` - the
// only thing this harness asserts is that we never panic.
//
// Run with `cargo fuzz run parse_include_cpp` from `parser/fuzz`, which is
// what `.github/workflows/fuzz.yml` does weekly. A committed seed corpus
// lives in `corpus/parse_include_cpp`. See `parser/fuzz/README.md`,
// including for how to get coverage-guided fuzzing out of a plain stable
// toolchain with no `cargo-fuzz`.
fuzz_target!(|data: &str| {
    // `IncludeCppConfig` is what both entry points actually parse:
    // `autocxx_parser::IncludeCpp` (the proc macro's) and
    // `autocxx_engine::IncludeCppEngine` (the codegen tool's) are each a
    // one-line delegate to this `syn::parse::Parse` impl.
    let Ok(mut config) = syn::parse_str::<autocxx_parser::IncludeCppConfig>(data) else {
        return;
    };

    // Parsing alone reaches none of the parser's panics: `IncludeCppConfig`'s
    // `Parse` impl reports every failure as a `syn::Error`. The panics live
    // one step later, in the accessors the engine reads the config through -
    // `bindgen_allowlist`, `is_on_allowlist` and the `ToTokens` impl all
    // `unreachable!()`/`panic!()` if the allowlist is still `Unspecified`,
    // i.e. if the user wrote no `generate!` of any kind.
    //
    // What settles that is `confirm_complete`, which
    // `engine/src/parse_file.rs` calls on every parsed `include_cpp!` before
    // the rest of the pipeline is allowed to look at it. So everything below
    // follows that same order, and only calls accessors the engine really
    // calls. That's what makes a panic found here meaningful: it's one a
    // user could reach by writing an `include_cpp!`, not this harness
    // driving the API into a state no caller would.
    config.confirm_complete();

    // Read straight off the config by the engine and the codegen tools.
    let mod_name = config.get_mod_name();
    let makestring = config.get_makestring_name();
    let _ = config.get_rs_filename();
    let _ = config.uniquify_name_per_mod(&mod_name.to_string());
    let _ = config.is_rust_type(&mod_name);
    let _ = config.exclude_utilities();
    let _ = config.get_pod_requests();
    let _ = config.superclasses().count();
    let _ = config.get_blocklist().count();

    // The allowlist handed to bindgen, and the list of items autocxx has
    // promised the user it will generate.
    let allowlist: Vec<String> = config
        .bindgen_allowlist()
        .map(Iterator::collect)
        .unwrap_or_default();
    let must_generate: Vec<String> = config.must_generate_list().collect();

    // Then, for every C++ item bindgen produced, the engine asks the config
    // whether to keep it. Real bindgen output is out of reach here, so ask
    // about a bounded handful of names taken from the input itself - which
    // is what those names largely are in practice, and it lets the fuzzer
    // steer them. Bounded because these are linear scans, and asking about
    // every name would make the cost quadratic in the size of the input.
    let names = [
        allowlist.first(),
        allowlist.last(),
        must_generate.first(),
        must_generate.last(),
        Some(&makestring),
    ];
    for name in names.into_iter().flatten() {
        let _ = config.is_on_allowlist(name);
        let _ = config.is_on_blocklist(name);
        let _ = config.is_on_constructor_blocklist(name);
        let _ = config.is_on_throws_list(name);
    }

    // Finally the reproduction case autocxx writes out when
    // `AUTOCXX_REPRO_CASE` is set, which renders the config back into the
    // directives it came from.
    let _ = config.to_token_stream();
});
