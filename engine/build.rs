// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The triple autocxx is being compiled for, recorded verbatim because it
    // is not available any other way at runtime: cargo sets `TARGET` for build
    // scripts and nothing else, and the `cfg`s rustc leaves behind name a
    // family (`arm`, and no vendor at all) rather than the target. `src/
    // clang_target.rs` needs it to tell clang what to parse for when autocxx
    // is not itself being run from a build script.
    println!(
        "cargo:rustc-env=AUTOCXX_COMPILED_TARGET={}",
        std::env::var("TARGET").expect("cargo sets TARGET for every build script")
    );
}
