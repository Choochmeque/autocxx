// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Major versions we'll go looking for. Distributions suffix the binary with
/// the major version (`llvm-config-18`); Homebrew and source builds leave it
/// unsuffixed, so the bare name is tried last - by which point a suffixed one
/// would have won, and on a machine with several LLVMs installed that's the
/// one whose headers we can name with certainty.
const MAJOR_VERSIONS: std::ops::RangeInclusive<u32> = 13..=25;

/// The header src/lib.rs asks for. We use it to tell a working `llvm-config`
/// apart from one belonging to an LLVM whose development headers aren't
/// installed - a common state of affairs, since the binary and the headers
/// come from different packages.
const SENTINEL_HEADER: &str = "llvm/Support/MemoryBuffer.h";

fn llvm_config(binary: &str, arg: &str) -> Option<String> {
    let output = Command::new(binary).arg(arg).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn candidates() -> Vec<String> {
    let mut candidates = Vec::new();
    candidates.extend(MAJOR_VERSIONS.rev().map(|v| format!("llvm-config-{v}")));
    candidates.push("llvm-config".to_owned());
    candidates
}

/// Extra `-I` directories and the `-std=` the installed LLVM wants. Its
/// headers stopped building as C++14 several releases ago, so taking the
/// standard from `llvm-config` rather than guessing is worth the parsing.
fn cxxflags(binary: &str) -> (Vec<PathBuf>, Option<String>) {
    let mut includes = Vec::new();
    let mut std_flag = None;
    if let Some(flags) = llvm_config(binary, "--cxxflags") {
        for flag in flags.split_whitespace() {
            if let Some(dir) = flag.strip_prefix("-I") {
                includes.push(PathBuf::from(dir));
            } else if flag.starts_with("-std=") {
                std_flag = Some(flag.to_owned());
            }
        }
    }
    (includes, std_flag)
}

fn main() -> miette::Result<()> {
    println!("cargo:rerun-if-env-changed=LLVM_CONFIG_PATH");
    println!("cargo:rerun-if-changed=src/lib.rs");

    let probe = |binary: &str| -> Option<(String, PathBuf)> {
        let includedir = llvm_config(binary, "--includedir")?;
        Path::new(&includedir)
            .join(SENTINEL_HEADER)
            .exists()
            .then(|| (binary.to_owned(), PathBuf::from(includedir)))
    };
    // An explicit override is authoritative: if the user named an
    // llvm-config and it doesn't work, say why rather than silently
    // building against whichever other LLVM happens to be installed.
    // Same spelling llvm-sys uses, so anyone who has already set it for
    // one LLVM crate doesn't have to learn a second name.
    let (binary, includedir) = match std::env::var("LLVM_CONFIG_PATH") {
        Ok(from_env) => probe(&from_env).unwrap_or_else(|| {
            panic!(
                "LLVM_CONFIG_PATH is set to {from_env}, but running it for \
                 --includedir failed or {SENTINEL_HEADER} was not under the \
                 directory it reported. Fix or unset it - it is not \
                 overridden by other installed LLVMs."
            )
        }),
        Err(_) => {
            let tried = candidates();
            tried.iter().find_map(|b| probe(b)).unwrap_or_else(|| {
                panic!(
                    "No llvm-config with usable headers found - looked for {}, \
                     and wanted {SENTINEL_HEADER} under the --includedir each \
                     reported. Install your distribution's llvm-<version>-dev \
                     package (on Windows, official LLVM installers may not \
                     ship llvm-config at all - build one from a package \
                     manager like MSYS2 that does), or point LLVM_CONFIG_PATH \
                     at an llvm-config of your own.",
                    tried.join(", ")
                )
            })
        }
    };

    let (extra_includes, std_flag) = cxxflags(&binary);
    let mut includes = vec![includedir];
    includes.extend(extra_includes);
    let std_flag = std_flag.unwrap_or_else(|| "-std=c++17".to_owned());

    // bindgen parses the headers separately from the compiler that later
    // builds the generated code, and it has to be told the same standard:
    // LLVM's headers are full of C++17 that a C++14 parse turns into a wall of
    // errors about std::optional not existing.
    let mut b = autocxx_build::Builder::new("src/lib.rs", &includes)
        .extra_clang_args(&[&std_flag])
        .build()?;
    b.flag_if_supported(&std_flag).compile("llvm");
    Ok(())
}
