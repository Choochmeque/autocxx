// Copyright 2023 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// It would be nice to use the rustversion crate here instead,
// but that doesn't work with inner attributes.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(nightly)");
    if let Some(ver) = rustc_version() {
        if ver.contains("nightly") {
            println!("cargo:rustc-cfg=nightly")
        }
    }
    build_c_type_vector_glue();
}

/// Compile the C++ half of the `std::vector`, `std::unique_ptr`,
/// `std::shared_ptr` and `std::weak_ptr` shims which `src/c_type_vectors.rs`
/// declares for the `autocxx::c_*` newtypes. See that module for why this
/// crate, rather than each generated bridge, is the one that has to do it.
/// See google/autocxx#422.
/// This C++ is not instrumented by the `AUTOCXX_ASAN` mode, so the sanitizer
/// job checks the accesses these shims make no more than it checks any other
/// uninstrumented C++. The decision lives in one place on purpose
/// (`autocxx_engine::add_sanitizer_flags`), and reaching it from here would mean
/// taking autocxx-engine, and so the vendored bindgen, as a build dependency of
/// this crate. cc does not carry `-Zsanitizer` over from `RUSTFLAGS` either -
/// its `inherit_rustflags` translates a fixed list of codegen flags and ignores
/// the rest.
#[cfg(feature = "c-type-vectors")]
fn build_c_type_vector_glue() {
    println!("cargo:rerun-if-changed=src/c_type_vectors.rs");
    println!("cargo:rerun-if-changed=src/c_type_vectors.h");
    cxx_build::bridge("src/c_type_vectors.rs")
        .include("src")
        .std("c++14")
        .define("AUTOCXX_WCHAR_T_SIZE", expected_wchar_t_size())
        .compile("autocxx-c-type-vectors");
}

/// How many bytes `autocxx::wchar_t` - and so `autocxx::c_wchar_t`, and so a
/// container of one - takes on the target, for `c_type_vectors.h` to hold the
/// C++ compiler to.
///
/// Only the width matters to a layout, so this is the two-way split behind
/// that alias's four arms rather than the arms themselves. It is written out
/// here rather than read off the alias because a build script is compiled for
/// the host: `#[cfg]` in this file would describe the wrong machine, and
/// `CARGO_CFG_*` describes the right one.
///
/// A C++ compiler may be told to disagree - `-fshort-wchar` makes `wchar_t`
/// two bytes where the platform says four - and nothing in cxx would catch it:
/// a `wchar_t` read out of a vector would then be read as two bytes more than
/// C++ put there.
#[cfg(feature = "c-type-vectors")]
fn expected_wchar_t_size() -> &'static str {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some()
        || matches!(target_os.as_str(), "cygwin" | "uefi")
        || matches!(target_arch.as_str(), "avr" | "msp430")
    {
        "2"
    } else {
        "4"
    }
}

/// Without the feature this crate compiles no C++ of its own, so a consumer
/// which only parses, or which uses pre-generated bindings, need not pull in
/// `cxx-build`.
#[cfg(not(feature = "c-type-vectors"))]
fn build_c_type_vector_glue() {}

fn rustc_version() -> Option<String> {
    let rustc = std::env::var_os("RUSTC")?;
    let output = std::process::Command::new(rustc)
        .arg("--version")
        .output()
        .ok()?;
    let version = String::from_utf8(output.stdout).ok()?;
    Some(version)
}
