// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Builds one generated Rust file on behalf of the integration test harness.
//!
//! The harness needs that build to happen in a process of its own so that the
//! compiler's diagnostics arrive down a pipe it can read; see
//! `autocxx_integration_tests::run_trybuild_child_if_requested` for why. This
//! binary exists so that the process it re-runs is one whose whole job is to
//! build - rather than one of the test binaries, which would have to expose a
//! pseudo-test for the harness to filter on, and would then carry it in every
//! listing of its tests forever.

fn main() {
    if !autocxx_integration_tests::run_trybuild_child_if_requested() {
        eprintln!(
            "This builds a single Rust file on behalf of the autocxx integration \
             test harness, which runs it with AUTOCXX_TRYBUILD_CHILD_RS_PATH set \
             to the file to build. Without that variable there is nothing to do."
        );
        std::process::exit(2);
    }
}
