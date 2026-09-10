// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The one hash function whose results autocxx lets outlive the process.

use std::hash::{Hash, Hasher};

use rustc_stable_hash::hashers::StableSipHasher128;

/// Hash `value` with a fixed algorithm.
///
/// Two things autocxx computes have to mean the same thing to a later, and
/// possibly differently-built, process: the key an `include_cpp!` block's
/// bindings are filed under in a JSON archive, which a separately compiled
/// proc macro looks up, and the identifiers built from
/// [`crate::IncludeCppConfig::uniquify_name_per_mod`], which are written into
/// generated Rust and generated C++ and become linker symbols. `std`'s
/// `DefaultHasher` cannot do either job: its algorithm is explicitly
/// unspecified across releases, so nothing rules out an archive written by one
/// toolchain being unreadable to another, or a rustc upgrade rewriting
/// generated output byte-for-byte - which is what
/// `autocxx_engine::output_generators`'s stable-output note, and the
/// content-addressed build systems behind google/autocxx#888, are about.
///
/// `rustc-stable-hash` fixes the mixing function and normalises endianness and
/// `usize` width. What it cannot fix is the byte stream fed to it, which comes
/// from `#[derive(Hash)]` and hence from `std`'s and `syn`'s `Hash` impls;
/// those are not a frozen format either. `test_config_hash_is_pinned` pins the
/// result of the whole pipeline for two configs, so that a change in what
/// those two produce has to move a literal in a test rather than silently
/// invalidate archives already on disk.
pub fn stable_hash(value: &impl Hash) -> u64 {
    let mut hasher = StableSipHasher128::new();
    value.hash(&mut hasher);
    // Named through the trait because `StableHasher` has an inherent `finish`
    // of its own, which returns the full 128 bits through `FromStableHash`.
    Hasher::finish(&hasher)
}
