// Copyright 2026 The autocxx maintainers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! True end-to-end tests driving `cargo build` on a fixture crate,
//! for behavior which only manifests in a real build.rs + macro
//! expansion round trip (e.g. `Builder::custom_gendir` path handling,
//! https://github.com/google/autocxx/issues/1499).

use std::fs::{create_dir_all, write};
use std::path::Path;
use std::process::Command;

/// Build a minimal fixture crate using autocxx with the given
/// custom_gendir expression (or none), returning (success, stderr).
fn build_fixture_crate(test_name: &str, custom_gendir_line: &str) -> (bool, String) {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    create_dir_all(root.join("src")).unwrap();
    write(
        root.join("Cargo.toml"),
        format!(
            r#"[package]
name = "gendir-fixture-{test_name}"
version = "0.1.0"
edition = "2021"

[dependencies]
autocxx = {{ path = "{ws}" }}
cxx = "1"

[build-dependencies]
autocxx-build = {{ path = "{ws}/gen/build" }}

[workspace]
"#,
            ws = workspace.display(),
        ),
    )
    .unwrap();
    write(
        root.join("build.rs"),
        format!(
            r#"fn main() {{
    let mut b = autocxx_build::Builder::new("src/main.rs", ["src"])
        {custom_gendir_line}
        .build()
        .unwrap();
    b.flag_if_supported("-std=c++14").compile("gendir-fixture-{test_name}");
    println!("cargo:rerun-if-changed=src/main.rs");
}}
"#
        ),
    )
    .unwrap();
    write(
        root.join("src").join("input.h"),
        r#"#pragma once
#include <cstdint>
inline uint32_t give_int() { return 4; }
"#,
    )
    .unwrap();
    write(
        root.join("src").join("main.rs"),
        r#"use autocxx::prelude::*;
include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("give_int")
}
fn main() {
    assert_eq!(ffi::give_int(), 4);
}
"#,
    )
    .unwrap();
    // Share one target dir across e2e tests so dependency artifacts
    // are compiled once, not per-test.
    let shared_target = workspace.join("target").join("e2e-fixture-target");
    let output = Command::new(env!("CARGO"))
        .arg("build")
        .current_dir(root)
        .env("CARGO_TARGET_DIR", &shared_target)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// https://github.com/google/autocxx/issues/1499: a *relative*
/// custom_gendir generated code correctly but include_cpp! then
/// tried to include it relative to the source file, producing a
/// doubled path (src/src/...) and failing the build.
#[test]
fn test_relative_custom_gendir() {
    let (ok, stderr) =
        build_fixture_crate("relative", r#".custom_gendir("src/generated_here".into())"#);
    assert!(
        ok,
        "cargo build failed with relative custom_gendir; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("src/src/"),
        "doubled path in build output:\n{stderr}"
    );
}

/// Control: an absolute custom_gendir has always worked and must
/// keep working.
#[test]
fn test_absolute_custom_gendir() {
    let tmp = tempfile::tempdir().unwrap();
    let gendir = tmp.path().join("gen");
    let line = format!(r#".custom_gendir("{}".into())"#, gendir.display());
    let (ok, stderr) = build_fixture_crate("absolute", &line);
    assert!(
        ok,
        "cargo build failed with absolute custom_gendir; stderr:\n{stderr}"
    );
}

/// Control: default OUT_DIR-based generation must keep working.
#[test]
fn test_default_gendir() {
    let (ok, stderr) = build_fixture_crate("default", "");
    assert!(
        ok,
        "cargo build failed with default gendir; stderr:\n{stderr}"
    );
}
