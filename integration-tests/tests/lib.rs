// Copyright 2021 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

mod builder_modifiers;
mod code_checkers;
mod cpprefs_test;
mod integration_test;

/// Re-entry point for the child process the harness uses to build generated Rust
/// code, so that the compiler's diagnostics can be captured and reported. Does
/// nothing unless the harness asked for it; see
/// `autocxx_integration_tests::run_trybuild_child_if_requested`.
#[test]
#[ignore = "not a test: the harness re-runs this binary with this filter"]
fn autocxx_trybuild_child() {
    assert_eq!(
        autocxx_integration_tests::TRYBUILD_CHILD_TEST_NAME,
        "autocxx_trybuild_child",
        "this test's name has to match the one the harness filters on"
    );
    autocxx_integration_tests::run_trybuild_child_if_requested();
}
