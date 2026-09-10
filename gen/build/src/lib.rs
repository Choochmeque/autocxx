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
/// itself when they change. `PATH` is absent deliberately: clang-sys does
/// search it for the `llvm-config` and the clang it asks about include
/// directories, so a toolchain switched only by reordering `PATH` will not
/// invalidate bindings and has to be rebuilt by hand. It differs between one
/// shell and the next and between CI steps, and autocxx regenerating every
/// binding on that is a cost paid on nearly every build for a case which is
/// rare and visible when it happens.
const CODEGEN_ENV_VARS: &[&str] = &[
    // engine::get_clang_path
    "CLANG_PATH",
    "CXX",
    // Which libclang clang-sys loads, and where it looks for it. Loaded at
    // runtime by default (engine's `runtime` feature), so this is decided in
    // the build script and not when the engine was compiled.
    // `LIBCLANG_STATIC_PATH` is not here: it is read only when linking
    // statically, which is settled when the engine itself is built, and
    // engine/build.rs watches it there.
    "LIBCLANG_PATH",
    "LD_LIBRARY_PATH",
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
/// is compiled, though clang-sys may run a clang to ask it where the system
/// headers are. What they therefore do *not* cover:
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
            "LD_LIBRARY_PATH",
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

/// What two `Builder`s in one build script do to one another's files.
///
/// Cargo hands every builder in a build script the same `OUT_DIR`, so the
/// second one writes into a directory the first one has already filled, and
/// nothing about a `build.rs` says how many builders it holds. These tests
/// drive that shape directly: two input files, one generation directory, both
/// codegens run.
///
/// They stop after codegen, as the tests above do, so what they do *not* cover
/// is the compile and link which would follow - that a bridge whose C++ was
/// taken by another builder fails to link is left to the build systems which
/// would suffer it.
#[cfg(test)]
mod output_collision_tests {
    use super::{BuilderContext, RebuildDependencyRecorder};
    use autocxx_engine::BuilderSuccess;
    use std::path::{Path, PathBuf};

    /// No dependency recording: these tests are about the files codegen
    /// writes, not the ones it reads.
    struct SilentContext;

    impl BuilderContext for SilentContext {
        fn get_dependency_recorder() -> Option<Box<dyn RebuildDependencyRecorder>> {
            None
        }
    }

    const INPUT_H: &str = "
inline int give_four() { return 4; }
inline int give_five() { return 5; }
";

    /// An `include_cpp!` naming one of the two functions, and its block named
    /// or left to default to `ffi`.
    fn input_rs(function: &str, name: Option<&str>) -> String {
        let name = name.map(|n| format!("name!({n})")).unwrap_or_default();
        format!(
            "
use autocxx::prelude::*;
include_cpp! {{
    #include \"input.h\"
    safety!(unsafe_ffi)
    {name}
    generate!(\"{function}\")
}}
"
        )
    }

    /// A directory of C++ and Rust inputs, and the one generation directory
    /// every builder here writes into.
    struct Fixture {
        _tmp: tempfile::TempDir,
        src: PathBuf,
        gendir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let src = tmp.path().join("src");
            std::fs::create_dir(&src).unwrap();
            std::fs::write(src.join("input.h"), INPUT_H).unwrap();
            let gendir = tmp.path().join("gen");
            Self {
                _tmp: tmp,
                src,
                gendir,
            }
        }

        fn input(&self, filename: &str, contents: String) -> PathBuf {
            let path = self.src.join(filename);
            std::fs::write(&path, contents).unwrap();
            path
        }

        fn build(&self, rs_file: &Path) -> Result<BuilderSuccess, autocxx_engine::BuilderError> {
            autocxx_engine::Builder::<SilentContext>::new(rs_file, [&self.src])
                .custom_gendir(self.gendir.clone())
                .build_listing_files()
        }
    }

    fn contents(path: &Path) -> String {
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("{} could not be read back: {e}", path.display()))
    }

    /// The diagnostic a build was supposed to produce.
    /// [`BuilderSuccess`] has no `Debug`, so `expect_err` cannot be used.
    fn refusal(
        result: Result<BuilderSuccess, autocxx_engine::BuilderError>,
        what_it_did_instead: &str,
    ) -> String {
        match result {
            Ok(_) => panic!("{what_it_did_instead}"),
            Err(e) => format!("{e}"),
        }
    }

    /// Two input files whose `include_cpp!` blocks are both left to default to
    /// `ffi` name one generated file between them, `autocxx-ffi-default-gen.rs`.
    /// Both blocks expand to an `include!` of that one path, so whichever
    /// builder runs second decides what *both* of them get - and the first
    /// file's bindings are gone. Only the user can settle it, by naming a
    /// block, so it has to be said rather than silently resolved.
    #[test]
    fn two_builders_cannot_take_one_anothers_bindings() {
        let fixture = Fixture::new();
        let first_rs = fixture.input("first.rs", input_rs("give_four", None));
        let second_rs = fixture.input("second.rs", input_rs("give_five", None));

        let first = fixture.build(&first_rs).expect("first codegen failed");
        let first_bindings = first.1.first().expect("no bindings generated").clone();
        assert!(contents(&first_bindings).contains("give_four"));

        let msg = refusal(
            fixture.build(&second_rs),
            "the second builder wrote over the first builder's bindings",
        );
        // `autocxxgen_ffi.h` rather than `autocxx-ffi-default-gen.rs`: both
        // are named after the block, and the header is written first, so it
        // is the one the two blocks are found to be sharing.
        for expected in [
            "autocxxgen_ffi.h".to_string(),
            first_rs.to_string_lossy().into_owned(),
            second_rs.to_string_lossy().into_owned(),
        ] {
            assert!(
                msg.contains(&expected),
                "the diagnostic does not mention {expected}: {msg}"
            );
        }
        // Refused before the write, not after it: the file the first builder
        // made still says what the first builder generated.
        assert!(
            contents(&first_bindings).contains("give_four"),
            "the first builder's bindings were overwritten anyway"
        );
    }

    /// Named blocks are the supported way to have two of them, and the C++
    /// files have to follow: a builder which is given a name to work with must
    /// not still write its C++ over the C++ of the builder before it, since
    /// those filenames are autocxx's own and no `name!` can separate them.
    ///
    /// That the header name the namer chose is the one that was written falls
    /// out of reading each listed file back: the list holds the name the
    /// namer gave, so a name which was never written is a file which cannot
    /// be read.
    #[test]
    fn two_builders_with_named_blocks_keep_their_own_cpp() {
        let fixture = Fixture::new();
        let first_rs = fixture.input("first.rs", input_rs("give_four", Some("ffi_a")));
        let second_rs = fixture.input("second.rs", input_rs("give_five", Some("ffi_b")));

        let first = fixture.build(&first_rs).expect("first codegen failed");
        let second = fixture.build(&second_rs).expect("second codegen failed");

        for path in &first.2 {
            assert!(
                !second.2.contains(path),
                "{} was written by both builders",
                path.display()
            );
        }
        // Not merely that each builder's C++ mentions its own function
        // somewhere, which a header alone would satisfy: none of the first
        // builder's files may have become the second builder's.
        for (built, own, other) in [
            (&first, "give_four", "give_five"),
            (&second, "give_five", "give_four"),
        ] {
            let implementation = built
                .2
                .iter()
                .find(|path| path.extension().is_some_and(|e| e == "cxx"))
                .expect("no C++ implementation was generated");
            assert!(
                contents(implementation).contains(own),
                "{} does not declare {own}",
                implementation.display()
            );
            for path in &built.2 {
                assert!(
                    !contents(path).contains(other),
                    "{} holds the other builder's code",
                    path.display()
                );
            }
        }
    }

    /// The same collision inside one file, which is the parser's to catch
    /// before any codegen runs. Pinned here because the diagnostic is what
    /// sends a user of the test above towards `name!`.
    #[test]
    fn two_blocks_in_one_file_are_refused_by_name() {
        let fixture = Fixture::new();
        let both = format!(
            "{}{}",
            input_rs("give_four", None),
            input_rs("give_five", None)
        );
        let rs_file = fixture.input("both.rs", both);
        let msg = refusal(
            fixture.build(&rs_file),
            "two blocks in one file both named ffi were accepted",
        );
        assert!(msg.contains("name!"), "{msg}");
    }
}
