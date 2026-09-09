// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Puts the harness's staging directory on the fixture's link search path.
//!
//! Each fixture links a `libautocxx-demo` built for that one test, which the
//! harness stages into a directory of its own. rustc has to be told where that
//! is, and the obvious channel - a `-L` in the flags the fixture builds with -
//! is the wrong one: cargo fingerprints those flags, so a directory that
//! differs between runs (it is a temporary one, and there is a fresh one per
//! process) invalidates every dependency of every fixture, every time, and
//! cargo rebuilds the lot before it compiles a line of the test.
//!
//! A link search path emitted by a build script is not part of any other
//! crate's fingerprint. It reaches the final link because cargo passes a
//! dependency's `rustc-link-search` along to the crates that depend on it,
//! which is the same route a `-sys` crate's library takes. So the flags stay
//! identical for every fixture in every process - the dependencies are built
//! once and stay built - and only this crate's build script re-runs when the
//! directory changes.

/// Names the directory to search. Set by the integration-test harness, which
/// declares the same name; nothing else sets it.
const LINK_SEARCH_VAR: &str = "AUTOCXX_FIXTURE_LINK_SEARCH";

fn main() {
    // Without this cargo would not re-run the build script when the harness
    // hands out a different directory, and the fixture would look in the
    // previous process's.
    println!("cargo:rerun-if-env-changed={LINK_SEARCH_VAR}");
    let dir = match std::env::var(LINK_SEARCH_VAR) {
        Ok(dir) => dir,
        // Unset means nobody is building a fixture - `cargo build` over the
        // workspace, say - and there is nothing to search.
        Err(std::env::VarError::NotPresent) => return,
        Err(std::env::VarError::NotUnicode(dir)) => panic!(
            "{LINK_SEARCH_VAR} has to be printable to be told to cargo, and it \
             is not valid Unicode: {dir:?}"
        ),
    };
    // A build script speaks to cargo in lines, so a newline in the path would
    // silently truncate the directive and leave the search path short - which
    // would show up much later as a fixture that cannot find its library.
    // Spaces are fine: the value is one argument the whole way down.
    assert!(
        !dir.contains(['\n', '\r']),
        "{LINK_SEARCH_VAR} cannot contain a line break, because that is what \
         separates one instruction to cargo from the next: {dir:?}"
    );
    // No `native=` kind, so this searches for whatever the `-L` it replaced
    // would have found.
    println!("cargo:rustc-link-search={dir}");
}
