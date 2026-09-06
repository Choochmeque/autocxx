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

/// Compile the C++ half of the `std::vector` shims which
/// `src/c_type_vectors.rs` declares for the `autocxx::c_*` newtypes. See that
/// module for why this crate, rather than each generated bridge, is the one
/// that has to do it. See google/autocxx#422.
#[cfg(feature = "c-type-vectors")]
fn build_c_type_vector_glue() {
    println!("cargo:rerun-if-changed=src/c_type_vectors.rs");
    println!("cargo:rerun-if-changed=src/c_type_vectors.h");
    cxx_build::bridge("src/c_type_vectors.rs")
        .include("src")
        .std("c++14")
        .compile("autocxx-c-type-vectors");
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
