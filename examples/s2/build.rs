// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

fn main() -> miette::Result<()> {
    // It's necessary to use an absolute path here because the
    // C++ codegen and the macro codegen appears to be run from different
    // working directories.
    let path = std::path::PathBuf::from("s2geometry/src");
    let path2 = std::path::PathBuf::from("src");
    let mut b = autocxx_build::Builder::new("src/main.rs", &[&path, &path2]).build()?;
    // R2Rect's methods live in a .cc file, not the headers, so every
    // platform needs it compiled. The generated translation unit contains
    // wrappers for methods the demo never calls; the existing ELF and
    // Mach-O link configurations discard those unused wrapper sections,
    // which is why the omission only ever surfaced in the Windows MSVC
    // and GNU CI links.
    b.file("s2geometry/src/s2/r2rect.cc")
        .flag_if_supported("-std=c++14")
        .compile("autocxx-s2-example");
    println!("cargo:rerun-if-changed=s2geometry/src/s2/r2rect.cc");
    println!("cargo:rerun-if-changed=src/main.rs");
    Ok(())
}
