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
    let s2 = std::path::PathBuf::from("s2geometry/src");
    // The commit this example used to pin vendored its own fork of Abseil
    // under s2/third_party. That is gone: s2geometry now expects a real
    // Abseil on the include path, so we carry one as a second submodule,
    // pinned to the LTS release v0.14.0 asks for.
    let absl = std::path::PathBuf::from("abseil-cpp");
    let local = std::path::PathBuf::from("src");
    // C++17 is s2geometry's floor, and Abseil's policy_checks.h turns
    // anything older into an #error, so both the bindgen parse and the
    // compile need to ask for it. MSVC still defaults to C++14, which is why
    // this is `.std()` rather than an `-std=` flag that only GCC and clang
    // would recognise.
    let mut b = autocxx_build::Builder::new("src/main.rs", &[&s2, &absl, &local])
        .extra_clang_args(&["-std=c++17", "-DNDEBUG", "-DSTRIP_LOG=1"])
        .build()?;
    // R2Rect's methods live in a .cc file, not the headers, so every
    // platform needs it compiled. The generated translation unit contains
    // wrappers for methods the demo never calls; the existing ELF and
    // Mach-O link configurations discard those unused wrapper sections,
    // which is why the omission only ever surfaced in the Windows MSVC
    // and GNU CI links.
    //
    // These two defines are what keep this to two translation units.
    // R1Interval, R2Rect and Vector2_d guard their accessors with
    // ABSL_DCHECK, which resolves to symbols in Abseil's log library, and
    // that library's link closure drags in absl base, strings, str_format,
    // time, synchronization, hash and the debugging/symbolize stack -
    // upwards of a hundred more translation units, plus per-platform system
    // libraries - to satisfy a handful of symbols in assertions this demo
    // never trips.
    //
    // It takes both defines, because they disarm different macros:
    //
    // * NDEBUG turns ABSL_DCHECK_GE and its siblings into
    //   ABSL_LOG_INTERNAL_DCHECK_NOP, which streams into a header-defined
    //   NullStream. That is a real no-op, and it is why
    //   MakeCheckOpString<long long, long long> never comes up.
    //
    // * NDEBUG does not do the same for a bare ABSL_DCHECK(cond). Abseil
    //   rewrites that one to CHECK(true || (cond)), which keeps the whole
    //   failure path - LogMessageFatal, Voidify, LogMessage::Flush - behind
    //   a condition that is only statically false; deleting it is left to
    //   the optimizer. clang and GCC fold it away even at -O0, so Linux,
    //   macOS and the MinGW leg linked. MSVC at /Od does not, and reported
    //   three unresolved externals for the ABSL_DCHECKs at r1interval.h:173
    //   and r2rect.h:177, :183 and :188. STRIP_LOG=1 fixes that in the
    //   preprocessor rather than the optimizer: it swaps LogMessageFatal for
    //   the header-only NullStreamFatal, so no compiler has to be clever.
    //
    // Do not add -DABSL_MIN_LOG_LEVEL, which s2geometry's own CMake sets. It
    // selects a branch that calls absl::log_internal::AbortQuietly, and puts
    // the problem back.
    //
    // The trade is real: s2geometry's bounds and validity assertions are
    // inactive here, and a CHECK that did fail would exit quietly instead of
    // reporting itself. This is not the configuration to copy into a program
    // that leans on either.
    b.file("s2geometry/src/s2/r2rect.cc")
        .std("c++17")
        .define("NDEBUG", None)
        .define("STRIP_LOG", "1")
        .compile("autocxx-s2-example");
    println!("cargo:rerun-if-changed=s2geometry/src/s2/r2rect.cc");
    println!("cargo:rerun-if-changed=src/main.rs");
    Ok(())
}
