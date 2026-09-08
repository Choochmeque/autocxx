// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Applies `engine/third_party/patches` to the bindgen sources and stops at the
//! first hunk which does not apply, without compiling what comes out.
//!
//! Two patches can each apply to pristine bindgen and still break each other -
//! both adding the same method, say - with nothing for git to conflict on,
//! because they are separate files in the series directory and a series is only
//! a series once it is applied in order. Until this existed, that happened
//! nowhere but inside `autocxx-engine`'s build, so the first thing to report it
//! was a build failure minutes into a job, on a branch which had merged green.
//!
//! This says only that the series applies. Whether the patched bindgen compiles
//! is a question for the jobs which build it.

use std::path::PathBuf;

/// The engine's build script, mounted as a module rather than copied, so that
/// what is checked here is what the build does - the same hunk matching, the
/// same line-ending normalisation, the same order.
///
/// Most of it builds bindgen rather than patching it, which is dead code from
/// here and live where it lives.
#[path = "../../../engine/build.rs"]
#[allow(dead_code)]
mod engine_build;

fn main() {
    // Canonical so that the path in a failure message is the one a reader can
    // open, rather than this crate's route to it.
    let engine = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../engine")
        .canonicalize()
        .expect("the engine directory sits two levels above this tool");
    let source = engine_build::bindgen_sources(&engine);
    let patches = engine.join(engine_build::PATCH_DIR);
    let count = engine_build::patch_series(&patches).len();
    let files = engine_build::apply_series(&source, &patches);
    println!("{count} patches apply to {} bindgen sources", files.len());
}
