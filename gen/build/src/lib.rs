// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![forbid(unsafe_code)]

use autocxx_engine::{BuilderContext, RebuildDependencyRecorder};
use indexmap::set::IndexSet as HashSet;
use std::{io::Write, sync::Mutex};

pub use autocxx_engine::{BuilderError, BuilderSuccess};

pub type Builder = autocxx_engine::Builder<'static, CargoBuilderContext>;

/// The environment the codegen reads, which cargo neither sets nor watches, so
/// that changing one of these invalidates the bindings it produced rather than
/// leaving them to be compiled against.
///
/// Cargo's own variables (`TARGET`, `OUT_DIR`, the `CARGO_CFG_*` set) are
/// absent: cargo keys the build directory on them and reruns build scripts
/// itself when they change. `PATH` is absent deliberately, though clang-sys
/// searches it for a clang to ask about include directories: it differs
/// between one shell and the next and between CI steps, so watching it would
/// regenerate bindings constantly and teach people to ignore the rebuild.
const CODEGEN_ENV_VARS: &[&str] = &[
    // engine::get_clang_path
    "CLANG_PATH",
    "CXX",
    // Which libclang clang-sys loads. Loaded at runtime by default
    // (engine's `runtime` feature), so this is decided in the build script
    // and not when the engine was compiled. Same list as engine/build.rs.
    "LIBCLANG_PATH",
    "LIBCLANG_STATIC_PATH",
    "LLVM_CONFIG_PATH",
    // engine::clang_target::bindgen_extra_clang_args
    "BINDGEN_EXTRA_CLANG_ARGS",
    // Read by the clang bindgen runs and by the one clang-sys asks for the
    // C++ include directories, so they change which headers are found.
    "CPATH",
    "CPLUS_INCLUDE_PATH",
    // engine::builder::add_sanitizer_flags
    "AUTOCXX_ASAN",
    // engine's header dumps
    "AUTOCXX_PREPROCESS",
    "AUTOCXX_REPRO_CASE",
    // where the generated Rust is written when no custom_gendir is given
    "AUTOCXX_RS",
];

/// The `rerun-if-env-changed` lines for a build targeting `target`.
///
/// bindgen looks `BINDGEN_EXTRA_CLANG_ARGS` up under three names - suffixed
/// with the target, suffixed with the target's dashes turned into underscores,
/// then bare - and takes the first which is set rather than concatenating them,
/// so setting a suffixed one changes the answer and all three have to be
/// watched.
///
/// Split from the printing so the set can be tested without a test reaching
/// into the process environment.
fn env_directives(target: Option<&str>) -> Vec<String> {
    let mut vars: Vec<String> = CODEGEN_ENV_VARS.iter().map(|v| v.to_string()).collect();
    if let Some(target) = target {
        vars.push(format!("BINDGEN_EXTRA_CLANG_ARGS_{target}"));
        let underscored = target.replace('-', "_");
        if underscored != target {
            vars.push(format!("BINDGEN_EXTRA_CLANG_ARGS_{underscored}"));
        }
    }
    vars.iter()
        .map(|var| format!("cargo:rerun-if-env-changed={var}"))
        .collect()
}

#[doc(hidden)]
pub struct CargoBuilderContext;

impl BuilderContext for CargoBuilderContext {
    fn setup() {
        let _ = env_logger::builder()
            .format(|buf, record| writeln!(buf, "cargo:warning=MESSAGE:{}", record.args()))
            .try_init();
    }
    fn get_dependency_recorder() -> Option<Box<dyn RebuildDependencyRecorder>> {
        Some(Box::new(CargoRebuildDependencyRecorder::new()))
    }
    fn record_environment_dependencies() {
        for directive in env_directives(std::env::var("TARGET").ok().as_deref()) {
            println!("{directive}");
        }
    }
}

#[derive(Debug)]
struct CargoRebuildDependencyRecorder {
    printed_already: Mutex<HashSet<String>>,
}

impl CargoRebuildDependencyRecorder {
    fn new() -> Self {
        Self {
            printed_already: Mutex::new(HashSet::new()),
        }
    }

    /// The line cargo is to be told, or `None` if this file has been recorded
    /// already - a header the preprocessor opens hundreds of times is reported
    /// hundreds of times.
    ///
    /// Split from the printing so the emission can be tested.
    fn line_for(&self, filename: &str) -> Option<String> {
        let mut already = self.printed_already.lock().unwrap();
        already
            .insert(filename.to_string())
            .then(|| format!("cargo:rerun-if-changed={filename}"))
    }
}

impl RebuildDependencyRecorder for CargoRebuildDependencyRecorder {
    fn record_dependency(&self, filename: &str) {
        if let Some(line) = self.line_for(filename) {
            println!("{line}");
        }
    }
}

/// What this crate tells cargo to watch. Emitting any `rerun-if` directive at
/// all replaces cargo's whole-package scan, so an input this crate reads and
/// does not report is an input which no longer reruns the build script.
///
/// The build these tests drive stops after codegen - `build_listing_files`
/// returns a [`cc::Build`] which nothing here calls `compile` on - so no C++
/// compiler is involved, only libclang. What they therefore do *not* cover:
/// that cargo acts on the lines (its own contract), that a second build reuses
/// or regenerates anything, and the directives reaching real stdout, which the
/// functions producing them are tested for instead.
#[cfg(test)]
mod rerun_tests {
    use super::{
        env_directives, BuilderContext, CargoBuilderContext, CargoRebuildDependencyRecorder,
        RebuildDependencyRecorder, CODEGEN_ENV_VARS,
    };
    use std::sync::Mutex;

    /// Lines recorded by [`CapturingRecorder`]. A static because
    /// [`BuilderContext::get_dependency_recorder`] is handed no context of its
    /// own; only [`the_rust_input_and_its_headers_are_all_watched`] uses it.
    static RECORDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Keeps the real recorder's answers instead of printing them, so the
    /// assertions below are made against the directives cargo would receive
    /// rather than a paraphrase of them.
    #[derive(Debug)]
    struct CapturingRecorder(CargoRebuildDependencyRecorder);

    impl RebuildDependencyRecorder for CapturingRecorder {
        fn record_dependency(&self, filename: &str) {
            if let Some(line) = self.0.line_for(filename) {
                RECORDED.lock().unwrap().push(line);
            }
        }
    }

    struct CapturingContext;

    impl BuilderContext for CapturingContext {
        fn get_dependency_recorder() -> Option<Box<dyn RebuildDependencyRecorder>> {
            Some(Box::new(CapturingRecorder(
                CargoRebuildDependencyRecorder::new(),
            )))
        }
    }

    const INPUT_H: &str = "inline int give_int() { return 4; }\n";

    const MAIN_RS: &str = "
use autocxx::prelude::*;
include_cpp! {
    #include \"input.h\"
    safety!(unsafe_ffi)
    generate!(\"give_int\")
}
fn main() {
    assert_eq!(ffi::give_int(), 4);
}
";

    /// Both halves of the input: the Rust file naming the headers, and the
    /// header it names. Losing either one is a build which reuses bindings that
    /// no longer describe the C++ or the macro that asked for them.
    #[test]
    fn the_rust_input_and_its_headers_are_all_watched() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("input.h"), INPUT_H).unwrap();
        let rs_file = src.join("main.rs");
        std::fs::write(&rs_file, MAIN_RS).unwrap();

        RECORDED.lock().unwrap().clear();
        autocxx_engine::Builder::<CapturingContext>::new(&rs_file, [&src])
            .custom_gendir(tmp.path().join("gen"))
            .build_listing_files()
            .expect("codegen failed");
        let recorded = RECORDED.lock().unwrap().clone();

        let rs_line = format!("cargo:rerun-if-changed={}", rs_file.to_string_lossy());
        assert!(
            recorded.contains(&rs_line),
            "the Rust file autocxx parsed is not watched: {rs_line} missing from {recorded:?}"
        );
        assert!(
            recorded.iter().any(
                |line| line.ends_with("input.h") && line.starts_with("cargo:rerun-if-changed=")
            ),
            "the header autocxx parsed is not watched: {recorded:?}"
        );
    }

    /// A file reported twice produces one line: cargo accepts repeats, but a
    /// build script whose log is thousands of identical lines is one nobody
    /// reads.
    #[test]
    fn a_file_reported_twice_is_announced_once() {
        let recorder = CargoRebuildDependencyRecorder::new();
        assert_eq!(
            recorder.line_for("/tmp/a.h").as_deref(),
            Some("cargo:rerun-if-changed=/tmp/a.h")
        );
        assert_eq!(recorder.line_for("/tmp/a.h"), None);
        assert_eq!(
            recorder.line_for("/tmp/b.h").as_deref(),
            Some("cargo:rerun-if-changed=/tmp/b.h")
        );
    }

    /// The environment the codegen reads, spelled out rather than read back
    /// from [`CODEGEN_ENV_VARS`], so that emptying that list cannot leave this
    /// passing with nothing to check.
    #[test]
    fn the_environment_the_codegen_reads_is_watched() {
        let directives = env_directives(Some("aarch64-apple-darwin"));
        for var in [
            "CLANG_PATH",
            "CXX",
            "LIBCLANG_PATH",
            "LIBCLANG_STATIC_PATH",
            "LLVM_CONFIG_PATH",
            "BINDGEN_EXTRA_CLANG_ARGS",
            "BINDGEN_EXTRA_CLANG_ARGS_aarch64-apple-darwin",
            "BINDGEN_EXTRA_CLANG_ARGS_aarch64_apple_darwin",
            "CPATH",
            "CPLUS_INCLUDE_PATH",
            "AUTOCXX_ASAN",
            "AUTOCXX_PREPROCESS",
            "AUTOCXX_REPRO_CASE",
            "AUTOCXX_RS",
        ] {
            let directive = format!("cargo:rerun-if-env-changed={var}");
            assert!(
                directives.contains(&directive),
                "{directive} missing from {directives:?}"
            );
        }
        // A target which is already spelled with underscores is not watched
        // twice, and a build which does not know its target still watches the
        // bare spelling.
        assert_eq!(
            env_directives(Some("wasm32")).len(),
            CODEGEN_ENV_VARS.len() + 1
        );
        assert_eq!(env_directives(None).len(), CODEGEN_ENV_VARS.len());
    }

    /// The context this crate actually installs is the one under test above:
    /// [`CapturingContext`] stands in only for the printing.
    #[test]
    fn the_cargo_context_records_dependencies_at_all() {
        assert!(CargoBuilderContext::get_dependency_recorder().is_some());
    }
}
