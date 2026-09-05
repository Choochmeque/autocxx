// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{Read, Write},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::Mutex,
};

use autocxx_engine::{
    Builder, BuilderBuild, BuilderContext, BuilderError, RebuildDependencyRecorder, HEADER,
};
use log::info;
use once_cell::sync::OnceCell;
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, TokenStreamExt};
use syn::Token;
use tempfile::{tempdir, TempDir};

const KEEP_TEMPDIRS: bool = false;

/// API to run a documentation test. Panics if the test fails.
/// Guarantees not to emit anything to stdout and so can be run in an mdbook context.
pub fn doctest(
    cxx_code: &str,
    header_code: &str,
    rust_code: TokenStream,
    manifest_dir: &OsStr,
) -> Result<(), TestError> {
    std::env::set_var("CARGO_PKG_NAME", "autocxx-integration-tests");
    std::env::set_var("CARGO_MANIFEST_DIR", manifest_dir);
    do_run_test_manual(cxx_code, header_code, rust_code, None, None)
}

fn configure_builder(b: &mut BuilderBuild) -> &mut BuilderBuild {
    let target = rust_info::get().target_triple.unwrap();
    b.host(&target)
        .target(&target)
        .opt_level(1)
        // std(), not .flag("-std=c++14"): cc picks the right spelling per
        // tool family (so cl.exe no longer prints D9002 "ignoring unknown
        // option" once per fixture build), and emits it BEFORE per-test
        // modifier flags, so tests appending -std=c++17 still win.
        .std("c++14")
        .flag_if_supported("/GX"); // Enable C++ exceptions for msvc
    if !target.contains("msvc") {
        // -Wall is the curated warning set on gcc/clang, but cl.exe maps it
        // to its audit-mode /Wall, which Microsoft documents as not meant
        // for routine builds — it floods the log with off-by-default
        // diagnostics from system headers (tens of thousands of lines per
        // CI run). MSVC keeps cc's default /W4, the strict curated
        // equivalent. (-Werror has never applied on MSVC: cl rejects the
        // spelling, so the probe fails; the /W4 output is not yet clean
        // enough for /WX — see the C4267 narrowing note in the tracker.)
        b.flag_if_supported("-Wall").flag_if_supported("-Werror");
    }
    b
}

/// Environment variables telling generated code where to find its bindings, as
/// name/value pairs. Applied to the process that builds the code, on top of a
/// cleared set of `RS_FIND_KEYS` (the `AUTOCXX_RS*` variables).
pub type RsFindEnv = Vec<(String, OsString)>;

/// Works out the [`RsFindEnv`] for a build, given the temporary directory the
/// bindings were staged into. See [`RsFindMode::Custom`].
pub type RsFindEnvFn = Box<dyn FnOnce(&Path) -> RsFindEnv>;

/// What environment variables we should set in order to tell rustc how to find
/// the Rust code.
pub enum RsFindMode {
    AutocxxRs,
    AutocxxRsArchive,
    AutocxxRsFile,
    /// Work out the variables with a callback rather than using one of the
    /// fixed layouts above. It receives the path to the temporary directory and
    /// returns the variables to set, which are applied to the process that
    /// builds the code rather than to this one - so two tests using this at the
    /// same time cannot interfere with each other.
    Custom(RsFindEnvFn),
}

/// API to test building pre-generated files.
pub fn build_from_folder(
    folder: &Path,
    main_rs_file: &Path,
    generated_rs_files: Vec<PathBuf>,
    cpp_files: &[&str],
    rs_find_mode: RsFindMode,
) -> Result<(), TestError> {
    let target_dir = folder.join("target");
    std::fs::create_dir(&target_dir).unwrap();
    let mut b = BuilderBuild::new();
    for cpp_file in cpp_files.iter() {
        b.file(folder.join(cpp_file));
    }
    configure_builder(&mut b)
        .out_dir(&target_dir)
        .include(folder)
        .include(folder.join("demo"))
        .try_compile("autocxx-demo")
        .map_err(TestError::CppBuild)?;
    // use the trybuild crate to build the Rust file.
    get_builder()
        .lock()
        .unwrap()
        .build(
            &target_dir,
            "autocxx-demo",
            &folder,
            &["input.h", "cxx.h"],
            main_rs_file,
            generated_rs_files,
            rs_find_mode,
        )
        .map_err(TestError::RsBuild)?;
    Ok(())
}

fn get_builder() -> &'static Mutex<LinkableTryBuilder> {
    static INSTANCE: OnceCell<Mutex<LinkableTryBuilder>> = OnceCell::new();
    INSTANCE.get_or_init(|| Mutex::new(LinkableTryBuilder::new()))
}

/// TryBuild which maintains a directory of libraries to link.
/// This is desirable because otherwise, if we alter the RUSTFLAGS
/// then trybuild rebuilds *everything* including all the dev-dependencies.
/// This object exists purely so that we use the same RUSTFLAGS for every
/// test case.
struct LinkableTryBuilder {
    /// Directory in which we'll keep any linkable libraries
    temp_dir: TempDir,
}

impl LinkableTryBuilder {
    fn new() -> Self {
        LinkableTryBuilder {
            temp_dir: tempdir().unwrap(),
        }
    }

    fn move_items_into_temp_dir<P1: AsRef<Path>>(&self, src_path: &P1, pattern: &str) {
        for item in std::fs::read_dir(src_path).unwrap() {
            let item = item.unwrap();
            if item.file_name().into_string().unwrap().contains(pattern) {
                let dest = self.temp_dir.path().join(item.file_name());
                if dest.exists() {
                    std::fs::remove_file(&dest).unwrap();
                }
                if KEEP_TEMPDIRS {
                    std::fs::copy(item.path(), dest).unwrap();
                } else {
                    std::fs::rename(item.path(), dest).unwrap();
                }
            }
        }
    }

    /// Builds `rs_path` using trybuild. On failure, the `Err` carries the report
    /// trybuild printed - i.e. rustc's diagnostics - because that is the only
    /// place they exist, and without them a build failure (particularly one that
    /// only reproduces on one platform's CI) is impossible to diagnose.
    #[allow(clippy::too_many_arguments)]
    fn build<P1: AsRef<Path>, P2: AsRef<Path>>(
        &self,
        library_path: &P1,
        library_name: &str,
        header_path: &P2,
        header_names: &[&str],
        rs_path: &Path,
        generated_rs_files: Vec<PathBuf>,
        rs_find_mode: RsFindMode,
    ) -> Result<(), String> {
        // Copy all items from the source dir into our temporary dir if their name matches
        // the pattern given in `library_name`.
        self.move_items_into_temp_dir(library_path, library_name);
        for header_name in header_names {
            self.move_items_into_temp_dir(header_path, header_name);
        }
        for generated_rs in generated_rs_files {
            self.move_items_into_temp_dir(
                &generated_rs.parent().unwrap(),
                generated_rs.file_name().unwrap().to_str().unwrap(),
            );
        }
        let temp_path = self.temp_dir.path().to_str().unwrap();
        let mut rustflags = format!("-L {temp_path}");
        if std::env::var_os("AUTOCXX_ASAN").is_some() {
            rustflags.push_str(" -Z sanitizer=address -Clinker=clang++ -Clink-arg=-fuse-ld=lld");
        }
        run_trybuild(
            rs_path,
            &rustflags,
            &rs_find_env(rs_find_mode, self.temp_dir.path()),
        )
    }
}

/// Every variable that can tell generated code where to find its bindings.
/// Whichever of these a build does not want is removed rather than left alone,
/// so a value meant for a different test cannot change what gets built.
const RS_FIND_KEYS: [&str; 3] = ["AUTOCXX_RS", "AUTOCXX_RS_JSON_ARCHIVE", "AUTOCXX_RS_FILE"];

/// The environment variables that tell the generated code where to find the
/// Rust bindings, resolved for a given [`RsFindMode`].
///
/// These used to be applied to this process with `set_var` and left there, where
/// concurrent tests could race over them and stale values could survive into
/// later tests. They are now values handed to the child that does the build.
fn rs_find_env(rs_find_mode: RsFindMode, temp_dir: &Path) -> RsFindEnv {
    let one = |key: &str, value: OsString| vec![(key.to_owned(), value)];
    match rs_find_mode {
        RsFindMode::AutocxxRs => one("AUTOCXX_RS", temp_dir.into()),
        RsFindMode::AutocxxRsArchive => one(
            "AUTOCXX_RS_JSON_ARCHIVE",
            temp_dir.join("gen.rs.json").into(),
        ),
        RsFindMode::AutocxxRsFile => {
            one("AUTOCXX_RS_FILE", temp_dir.join("gen0.include.rs").into())
        }
        RsFindMode::Custom(f) => f(temp_dir),
    }
}

/// Name of the test that a test binary must expose so that this harness can
/// re-enter it as a child process. See [`run_trybuild_child_if_requested`].
pub const TRYBUILD_CHILD_TEST_NAME: &str = "autocxx_trybuild_child";

/// Carries the path of the Rust file to build. Its presence is what puts a
/// re-entered process into child mode.
const TRYBUILD_CHILD_RS_PATH: &str = "AUTOCXX_TRYBUILD_CHILD_RS_PATH";

/// Printed by the child the moment it enters child mode, so that the parent can
/// tell "the build ran and succeeded" apart from "this executable has no
/// re-entry hook, so the child did nothing at all". Without it a missing hook
/// would look exactly like a passing test.
const TRYBUILD_CHILD_SENTINEL: &str = "@@ autocxx trybuild child running @@";

/// Re-entry point for the child process that actually builds the generated Rust
/// code. Returns `true` if it ran, in which case the caller must return
/// immediately and do nothing else.
///
/// Every executable that can reach [`build_from_folder`], [`do_run_test`] and
/// friends should give this a chance to run before doing anything else: a test
/// binary by exposing an `#[ignore]`d test named [`TRYBUILD_CHILD_TEST_NAME`]
/// that calls it, any other binary by calling it at the top of `main`. An
/// executable that does not is still correct - the harness spots that the child
/// did nothing and builds in-process instead - but its build failures come
/// without diagnostics.
pub fn run_trybuild_child_if_requested() -> bool {
    let rs_path = match std::env::var_os(TRYBUILD_CHILD_RS_PATH) {
        Some(rs_path) => PathBuf::from(rs_path),
        None => return false,
    };
    // Before anything is allowed to spawn a process. trybuild's cargo, and every
    // rustc and build script under it, inherit this environment; if one of them
    // happened to be an executable with this same hook - the mdbook preprocessor
    // is exactly that - it would enter child mode and start building instead of
    // doing its job.
    std::env::remove_var(TRYBUILD_CHILD_RS_PATH);
    println!("{TRYBUILD_CHILD_SENTINEL}");
    let test_cases = trybuild::TestCases::new();
    test_cases.pass(rs_path);
    // `TestCases` runs the build - and panics if it fails - when it drops. The
    // panic is deliberately not caught: it is what gives this process the
    // non-zero exit status that tells the parent the build failed.
    drop(test_cases);
    true
}

/// Builds `rs_path` with trybuild in a child process, and on failure returns the
/// child's output - i.e. rustc's diagnostics as trybuild rendered them.
///
/// The build has to happen in a separate process because of where trybuild's
/// report goes. trybuild prints it through its *own* `println!` macro
/// (`trybuild::term`), which writes to a `termcolor::StandardStream::stderr` and
/// therefore straight to file descriptor 2. libtest's per-test output capture is
/// a thread-local that only `std::io::_print`/`_eprint` - the *std* `print!` and
/// `eprint!` macros - consult, so trybuild's diagnostics bypass it entirely:
/// they land in the raw process stderr, unattributed to any test and nowhere
/// near the `failures:` block that names the test which produced them. On a
/// suite this size that makes a build failure effectively undiagnosable, which
/// is exactly the position a platform-specific CI failure leaves you in.
///
/// Giving the build its own process is what makes capturing it safe. Redirecting
/// this process's own fd 2 would be much less code, but fd 2 is process-global
/// and these tests run in parallel while spawning compiler children constantly,
/// so it would swallow output belonging to unrelated tests. A child's pipe
/// belongs to that child alone.
///
/// The child is this same executable, re-entered via
/// [`run_trybuild_child_if_requested`]; the arguments below select just the
/// re-entry test when it is a test binary, and are ignored by anything else,
/// which checks the hook before it looks at its arguments.
fn run_trybuild(
    rs_path: &Path,
    rustflags: &str,
    rs_find_env: &[(String, OsString)],
) -> Result<(), String> {
    let current_exe = match std::env::current_exe() {
        Ok(current_exe) => current_exe,
        Err(err) => {
            return build_in_process(
                rs_path,
                rustflags,
                rs_find_env,
                &format!("this executable's own path could not be determined ({err})"),
            )
        }
    };
    let mut cmd = std::process::Command::new(current_exe);
    cmd.env(TRYBUILD_CHILD_RS_PATH, rs_path)
        .env("RUSTFLAGS", rustflags)
        .args([
            "--exact",
            TRYBUILD_CHILD_TEST_NAME,
            "--ignored",
            // So that trybuild's report reaches the child's real stderr, and
            // hence our pipe, rather than libtest's own capture buffer.
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Clear the lot before setting what this build wants, so that a variable
    // inherited from our own environment cannot redirect the child.
    for key in RS_FIND_KEYS {
        cmd.env_remove(key);
    }
    for (key, value) in rs_find_env {
        cmd.env(key, value);
    }
    let output = match cmd.output() {
        Ok(output) => output,
        Err(err) => {
            return build_in_process(
                rs_path,
                rustflags,
                rs_find_env,
                &format!("this executable could not be re-run ({err})"),
            )
        }
    };
    let mut report = String::from_utf8_lossy(&output.stdout).into_owned();
    report.push_str(&String::from_utf8_lossy(&output.stderr));
    if !report.contains(TRYBUILD_CHILD_SENTINEL) {
        // The child never reached the re-entry hook, so its exit status says
        // nothing about the build and must not be trusted.
        return build_in_process(
            rs_path,
            rustflags,
            rs_find_env,
            &format!(
                "this executable has no `{TRYBUILD_CHILD_TEST_NAME}` re-entry hook \
                 (see `autocxx_integration_tests::run_trybuild_child_if_requested`)"
            ),
        );
    }
    if output.status.success() {
        Ok(())
    } else {
        Err(summarize_rs_build_failure(&report))
    }
}

/// Last resort for an executable this harness cannot re-enter: build here, the
/// way this harness always used to. The diagnostics go wherever trybuild puts
/// them and cannot be recovered, so the error says why.
fn build_in_process(
    rs_path: &Path,
    rustflags: &str,
    rs_find_env: &[(String, OsString)],
    reason: &str,
) -> Result<(), String> {
    // Unlike the child, this has to go through the process environment, so it
    // is racy if other tests are building at the same time. That is the price of
    // an executable that cannot be re-entered.
    std::env::set_var("RUSTFLAGS", rustflags);
    for key in RS_FIND_KEYS {
        std::env::remove_var(key);
    }
    for (key, value) in rs_find_env {
        std::env::set_var(key, value);
    }
    // `TestCases` runs the build, and panics if it fails, when it drops - so the
    // drop has to happen inside the `catch_unwind`.
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let test_cases = trybuild::TestCases::new();
        test_cases.pass(rs_path);
    }));
    match outcome {
        Ok(()) => Ok(()),
        Err(_) => Err(format!(
            "the generated Rust failed to build. The compiler's diagnostics could \
             not be captured because {reason}, so trybuild printed them to this \
             process's stderr - look for them there."
        )),
    }
}

/// Rust build failures can run to thousands of lines once every warning in the
/// generated code is included. Keep the tail, which is where the errors and
/// trybuild's own summary are.
const MAX_DIAGNOSTIC_LINES: usize = 200;

fn summarize_rs_build_failure(child_output: &str) -> String {
    let mut lines: &[&str] = &child_output
        .lines()
        // Our own handshake with the child, of no interest to whoever is reading
        // the failure.
        .filter(|line| line.trim() != TRYBUILD_CHILD_SENTINEL)
        .skip_while(|line| line.trim().is_empty())
        .collect::<Vec<_>>()[..];
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines = &lines[..lines.len() - 1];
    }
    if lines.is_empty() {
        return "the build failed, but the child process printed nothing.".to_string();
    }
    match lines.len().checked_sub(MAX_DIAGNOSTIC_LINES) {
        None | Some(0) => lines.join("\n"),
        Some(omitted) => format!(
            "[...{omitted} earlier lines omitted...]\n{}",
            lines[omitted..].join("\n")
        ),
    }
}

fn write_to_file(tdir: &TempDir, filename: &str, content: &str) -> PathBuf {
    let path = tdir.path().join(filename);
    let mut f = File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    path
}

/// A positive test, we expect to pass.
#[track_caller]
pub fn run_test(
    cxx_code: &str,
    header_code: &str,
    rust_code: TokenStream,
    generate: &[&str],
    generate_pods: &[&str],
) {
    do_run_test(
        cxx_code,
        header_code,
        rust_code,
        directives_from_lists(generate, generate_pods, None),
        None,
        None,
        None,
        "unsafe_ffi",
        None,
    )
    .unwrap()
}

// A trait for objects which can check the output of the code creation
// process.
pub trait CodeCheckerFns {
    fn check_rust(&self, _rs: syn::File) -> Result<(), TestError> {
        Ok(())
    }
    fn check_cpp(&self, _cpp: &[PathBuf]) -> Result<(), TestError> {
        Ok(())
    }
    fn skip_build(&self) -> bool {
        false
    }
}

// A function applied to the resultant generated Rust code
// which can be used to inspect that code.
pub type CodeChecker = Box<dyn CodeCheckerFns>;

// A trait for objects which can modify builders for testing purposes.
pub trait BuilderModifierFns {
    fn modify_autocxx_builder<'a>(
        &self,
        builder: Builder<'a, TestBuilderContext>,
    ) -> Builder<'a, TestBuilderContext>;
    fn modify_cc_builder<'a>(&self, builder: &'a mut cc::Build) -> &'a mut cc::Build {
        builder
    }
}

pub type BuilderModifier = Box<dyn BuilderModifierFns>;

/// A positive test, we expect to pass.
#[allow(clippy::too_many_arguments)] // least typing for each test
pub fn run_test_ex(
    cxx_code: &str,
    header_code: &str,
    rust_code: TokenStream,
    directives: TokenStream,
    builder_modifier: Option<BuilderModifier>,
    code_checker: Option<CodeChecker>,
    extra_rust: Option<TokenStream>,
) {
    do_run_test(
        cxx_code,
        header_code,
        rust_code,
        directives,
        builder_modifier,
        code_checker,
        extra_rust,
        "unsafe_ffi",
        None,
    )
    .unwrap()
}

pub fn run_generate_all_test(header_code: &str) {
    run_test_ex(
        "",
        header_code,
        quote! {},
        quote! { generate_all!() },
        None,
        None,
        None,
    );
}

pub fn run_test_expect_fail(
    cxx_code: &str,
    header_code: &str,
    rust_code: TokenStream,
    generate: &[&str],
    generate_pods: &[&str],
) {
    do_run_test(
        cxx_code,
        header_code,
        rust_code,
        directives_from_lists(generate, generate_pods, None),
        None,
        None,
        None,
        "unsafe_ffi",
        None,
    )
    .expect_err("Unexpected success");
}

/// As [`run_test_expect_fail`], but also insists on *why* it failed, so that a
/// test can pin the diagnostic a user will actually see rather than settling
/// for any failure at all.
pub fn run_test_expect_fail_with_error(
    cxx_code: &str,
    header_code: &str,
    rust_code: TokenStream,
    generate: &[&str],
    generate_pods: &[&str],
    expected: &str,
) {
    let err = do_run_test(
        cxx_code,
        header_code,
        rust_code,
        directives_from_lists(generate, generate_pods, None),
        None,
        None,
        None,
        "unsafe_ffi",
        None,
    )
    .expect_err("Unexpected success");
    let reported = format!("{err:?}");
    assert!(
        reported.contains(expected),
        "expected the failure to mention {expected:?}, but it was: {reported}"
    );
}

pub fn run_test_expect_fail_ex(
    cxx_code: &str,
    header_code: &str,
    rust_code: TokenStream,
    directives: TokenStream,
    builder_modifier: Option<BuilderModifier>,
    code_checker: Option<CodeChecker>,
    extra_rust: Option<TokenStream>,
) {
    do_run_test(
        cxx_code,
        header_code,
        rust_code,
        directives,
        builder_modifier,
        code_checker,
        extra_rust,
        "unsafe_ffi",
        None,
    )
    .expect_err("Unexpected success");
}

/// In the future maybe the tests will distinguish the exact type of failure expected.
pub enum TestError {
    AutoCxx(BuilderError),
    CppBuild(cc::Error),
    /// The generated Rust code failed to build. Carries rustc's diagnostics as
    /// trybuild rendered them, truncated to the last `MAX_DIAGNOSTIC_LINES`
    /// lines.
    RsBuild(String),
    NoRs,
    RsFileOpen(std::io::Error),
    RsFileRead(std::io::Error),
    RsFileParse(syn::Error),
    RsCodeExaminationFail(String),
    CppCodeExaminationFail,
}

/// Hand-written rather than derived so that `RsBuild`'s diagnostics come out
/// verbatim. The derived `Debug` would escape every newline, turning rustc's
/// output into one unreadable line in the panic message from `.unwrap()` - which
/// is precisely where a human needs to read it.
impl std::fmt::Debug for TestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestError::AutoCxx(err) => write!(f, "AutoCxx({err:?})"),
            TestError::CppBuild(err) => write!(f, "CppBuild({err:?})"),
            TestError::RsBuild(diagnostics) => {
                write!(
                    f,
                    "RsBuild: the generated Rust failed to build:\n{diagnostics}"
                )
            }
            TestError::NoRs => write!(f, "NoRs"),
            TestError::RsFileOpen(err) => write!(f, "RsFileOpen({err:?})"),
            TestError::RsFileRead(err) => write!(f, "RsFileRead({err:?})"),
            TestError::RsFileParse(err) => write!(f, "RsFileParse({err:?})"),
            TestError::RsCodeExaminationFail(msg) => write!(f, "RsCodeExaminationFail({msg:?})"),
            TestError::CppCodeExaminationFail => write!(f, "CppCodeExaminationFail"),
        }
    }
}

pub fn directives_from_lists(
    generate: &[&str],
    generate_pods: &[&str],
    extra_directives: Option<TokenStream>,
) -> TokenStream {
    let generate = generate.iter().map(|s| {
        quote! {
            generate!(#s)
        }
    });
    let generate_pods = generate_pods.iter().map(|s| {
        quote! {
            generate_pod!(#s)
        }
    });
    quote! {
        #(#generate)*
        #(#generate_pods)*
        #extra_directives
    }
}

#[allow(clippy::too_many_arguments)] // least typing for each test
pub fn do_run_test(
    cxx_code: &str,
    header_code: &str,
    rust_code: TokenStream,
    directives: TokenStream,
    builder_modifier: Option<BuilderModifier>,
    rust_code_checker: Option<CodeChecker>,
    extra_rust: Option<TokenStream>,
    safety_policy: &str,
    module_attributes: Option<TokenStream>,
) -> Result<(), TestError> {
    let hexathorpe = Token![#](Span::call_site());
    let safety_policy = format_ident!("{}", safety_policy);
    let unexpanded_rust = quote! {
            #module_attributes

            use autocxx::prelude::*;

            include_cpp!(
                #hexathorpe include "input.h"
                safety!(#safety_policy)
                #directives
            );

            #extra_rust

            fn main() {
                #rust_code
            }

    };
    do_run_test_manual(
        cxx_code,
        header_code,
        unexpanded_rust,
        builder_modifier,
        rust_code_checker,
    )
}

/// The [`BuilderContext`] used in autocxx's integration tests.
pub struct TestBuilderContext;

impl BuilderContext for TestBuilderContext {
    fn get_dependency_recorder() -> Option<Box<dyn RebuildDependencyRecorder>> {
        None
    }
}

pub fn do_run_test_manual(
    cxx_code: &str,
    header_code: &str,
    mut rust_code: TokenStream,
    builder_modifier: Option<BuilderModifier>,
    rust_code_checker: Option<CodeChecker>,
) -> Result<(), TestError> {
    let builder_modifier = consider_forcing_wrapper_generation(builder_modifier);

    const HEADER_NAME: &str = "input.h";
    // Step 2: Write the C++ header snippet to a temp file
    let tdir = tempdir().unwrap();
    write_to_file(&tdir, HEADER_NAME, &format!("#pragma once\n{header_code}"));
    write_to_file(&tdir, "cxx.h", HEADER);

    rust_code.append_all(quote! {
        #[link(name="autocxx-demo")]
        extern "C" {}
    });
    info!("Unexpanded Rust: {}", rust_code);

    let write_rust_to_file = |ts: &TokenStream| -> PathBuf {
        // Step 3: Write the Rust code to a temp file
        let rs_code = format!("{ts}");
        write_to_file(&tdir, "input.rs", &rs_code)
    };

    let target_dir = tdir.path().join("target");
    std::fs::create_dir(&target_dir).unwrap();

    let rs_path = write_rust_to_file(&rust_code);

    info!("Path is {:?}", tdir.path());
    let builder = Builder::<TestBuilderContext>::new(&rs_path, [tdir.path()])
        .custom_gendir(target_dir.clone());
    let builder = if let Some(builder_modifier) = &builder_modifier {
        builder_modifier.modify_autocxx_builder(builder)
    } else {
        builder
    };
    let build_results = builder.build_listing_files().map_err(TestError::AutoCxx)?;
    let mut b = build_results.0;
    let generated_rs_files = build_results.1;

    if let Some(code_checker) = &rust_code_checker {
        let mut file = File::open(generated_rs_files.first().ok_or(TestError::NoRs)?)
            .map_err(TestError::RsFileOpen)?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(TestError::RsFileRead)?;

        let ast = syn::parse_file(&content).map_err(TestError::RsFileParse)?;
        code_checker.check_rust(ast)?;
        code_checker.check_cpp(&build_results.2)?;
        if code_checker.skip_build() {
            return Ok(());
        }
    }

    if !cxx_code.is_empty() {
        // Step 4: Write the C++ code snippet to a .cc file, along with a #include
        //         of the header emitted in step 5.
        let cxx_code = format!("#include \"input.h\"\n#include \"cxxgen.h\"\n{cxx_code}");
        let cxx_path = write_to_file(&tdir, "input.cxx", &cxx_code);
        b.file(cxx_path);
    }

    let b = configure_builder(&mut b).out_dir(&target_dir);
    let b = if let Some(builder_modifier) = builder_modifier {
        builder_modifier.modify_cc_builder(b)
    } else {
        b
    };
    b.include(tdir.path())
        .try_compile("autocxx-demo")
        .map_err(TestError::CppBuild)?;
    if KEEP_TEMPDIRS {
        println!("Generated .rs files: {generated_rs_files:?}");
    }
    // Step 8: use the trybuild crate to build the Rust file.
    let r = get_builder().lock().unwrap().build(
        &target_dir,
        "autocxx-demo",
        &tdir.path(),
        &["input.h", "cxx.h"],
        &rs_path,
        generated_rs_files,
        RsFindMode::AutocxxRs,
    );
    if KEEP_TEMPDIRS {
        println!("Tempdir: {:?}", tdir.into_path().to_str());
    }
    r.map_err(TestError::RsBuild)?;
    Ok(())
}

/// If AUTOCXX_FORCE_WRAPPER_GENERATION is set, always force both C++
/// and Rust side shims, for extra testing of obscure code paths.
fn consider_forcing_wrapper_generation(
    existing_builder_modifier: Option<BuilderModifier>,
) -> Option<BuilderModifier> {
    if std::env::var("AUTOCXX_FORCE_WRAPPER_GENERATION").is_err() {
        existing_builder_modifier
    } else {
        Some(Box::new(ForceWrapperGeneration(existing_builder_modifier)))
    }
}

struct ForceWrapperGeneration(Option<BuilderModifier>);

impl BuilderModifierFns for ForceWrapperGeneration {
    fn modify_autocxx_builder<'a>(
        &self,
        builder: Builder<'a, TestBuilderContext>,
    ) -> Builder<'a, TestBuilderContext> {
        let builder = builder.force_wrapper_generation(true);
        if let Some(modifier) = &self.0 {
            modifier.modify_autocxx_builder(builder)
        } else {
            builder
        }
    }
    fn modify_cc_builder<'a>(&self, builder: &'a mut cc::Build) -> &'a mut cc::Build {
        if let Some(modifier) = &self.0 {
            modifier.modify_cc_builder(builder)
        } else {
            builder
        }
    }
}
