// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use autocxx_parser::file_locations::FileLocationStrategy;
use miette::Diagnostic;
use thiserror::Error;

use crate::{generate_rs_single, CodegenOptions};
use crate::{get_cxx_header_bytes, CppCodegenOptions, ParseError, RebuildDependencyRecorder};
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

/// Errors returned during creation of a [`cc::Build`] from an include_cxx
/// macro.
#[derive(Error, Diagnostic, Debug)]
#[cfg_attr(feature = "nightly", doc(cfg(feature = "build")))]
pub enum BuilderError {
    #[error("cxx couldn't handle our generated bindings - could be a bug in autocxx: {0}")]
    InvalidCxx(cxx_gen::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    ParseError(ParseError),
    #[error("we couldn't write the generated code to disk at {1}: {0}")]
    FileWriteFail(std::io::Error, PathBuf),
    #[error("no include_cpp! macro was found")]
    NoIncludeCxxMacrosFound,
    #[error("could not create a directory {1}: {0}")]
    UnableToCreateDirectory(std::io::Error, PathBuf),
    #[error("this build links cxx {cxx_version}, but autocxx generates its C++ with a cxx-gen which names symbols for {cxx_gen_mangling}. Since cxx 1.0.189 the patch level is part of every generated symbol name, so the two halves of each function would not find each other and the link would fail. Update the lockfile so that cxx and cxx-gen agree - `cargo update -p cxx -p cxx-gen` normally does it, since both crates are released together.")]
    CxxVersionMismatch {
        cxx_version: String,
        cxx_gen_mangling: String,
    },
}

#[cfg_attr(feature = "nightly", doc(cfg(feature = "build")))]
pub type BuilderBuild = cc::Build;

/// The flags cl.exe needs for the C++ autocxx generates, and none where cl is
/// not the compiler.
///
/// `/Zc:__cplusplus`: cl reports `__cplusplus` as 199711L whatever `/std:` says
/// unless this is passed, so a header gating declarations on the standard macro
/// (the portable idiom) is parsed by libclang one way, clang reporting it
/// truthfully, and compiled by cl another. Microsoft recommends passing it, and
/// it only makes the macro agree with the standard the build already selected.
///
/// `/EHsc`: without an `/EH` model cl warns (C4530) and gives no standard unwind
/// guarantees, while the shims generated for `throws!` contain try/catch. cc adds
/// no `/EH` flag itself. A user's `/EHa` in `CXXFLAGS` still wins, per MSVC's own
/// override rules.
///
/// Both are passed with `flag`, not `flag_if_supported`. Both are required
/// wherever cl is the compiler, so a dropped one is a quietly mis-built library
/// rather than a missing nicety, and `flag_if_supported` decides by running a
/// probe compile whose failure to *run* it reads as "unsupported" (cc's
/// `is_flag_supported_inner(..).unwrap_or(false)`), dropping the flag and naming
/// nothing in the log.
///
/// `msvc_like` comes from the target ([`crate::clang_target::build_target_is_msvc`]),
/// which is how the integration harness decides `/WX` and for the same reason:
/// the target cannot fail to answer. Asking cc which compiler family it picked
/// would be better informed, and is what `cc::Build::std` itself uses to choose
/// between `-std:` and `-std=`, but obtaining it is a spawn and a
/// scratch-directory write of cc's own; when that fails cc guesses from the
/// compiler's filename, does not cache the guess, and may answer differently when
/// `compile()` asks again - so one transient failure could leave cl compiling with
/// `-std:` and without these two, which is the shape this is getting away from.
///
/// A statement about the target, then, and not a promise about the compiler:
/// `CXX` can point an MSVC target at a gcc-driver clang, which would be handed a
/// `/Zc:` it reads as a filename and would stop. Checked: clang-cl takes both
/// flags, a gcc-driver clang takes neither. That is a loud failure in a
/// configuration nothing here uses, bought against a silent one in the
/// configuration everything uses.
///
/// One case stays silent, and it is the cost of not probing: the decision is
/// taken while `build()` runs, from the target known then, so a caller which
/// afterwards points the returned `cc::Build` at a *different* target gets the
/// flags chosen for the first one. Only an in-process caller can do that - a
/// build script's target is settled before it runs - and the one in this repo,
/// the integration harness, sets the target autocxx was compiled for, which is
/// the same answer. Following the final configuration instead is what a probe
/// does, and why it runs too late to be told about.
///
/// Split from the call site so the decision can be tested on its own.
fn msvc_flags(msvc_like: bool) -> &'static [&'static str] {
    if msvc_like {
        &["/Zc:__cplusplus", "/EHsc"]
    } else {
        // gcc and clang read `/Zc:__cplusplus` as the name of a file to compile.
        &[]
    }
}

fn apply_msvc_flags(builder: &mut BuilderBuild, msvc_like: bool) {
    for flag in msvc_flags(msvc_like) {
        builder.flag(flag);
    }
}

/// The C++ flags the `AUTOCXX_ASAN` build mode asks for, and none when it is
/// off.
///
/// Split from [`add_sanitizer_flags`] so the decision can be tested without a
/// test reaching into the process environment.
fn sanitizer_flags(asan: bool) -> &'static [&'static str] {
    if asan {
        // gcc, clang and clang-cl's spelling; cl.exe takes `-` for `/` and has
        // had `/fsanitize=address` since VS 16.9.
        &["-fsanitize=address"]
    } else {
        &[]
    }
}

/// Adds to `builder` the C++ flags the `AUTOCXX_ASAN` build mode asks for, and
/// nothing when it is unset.
///
/// Shared rather than restated because autocxx's C++ is compiled through two
/// builders - this crate's, and the one the integration harness makes for
/// already-generated files - and instrumenting the Rust half alone leaves every
/// access the C++ makes itself unchecked.
///
/// `flag`, not `flag_if_supported`: instrumentation is the only thing this mode
/// is for, so a compiler which will not instrument has to say so rather than
/// hand back a build which checks nothing. `flag_if_supported` resolves a flag by
/// running a probe compile of its own and reads any failure to *run* that probe -
/// a spawn lost to a concurrent suite, a probe which merely printed something on
/// stderr - as "unsupported", dropping the flag and naming nothing in the log
/// (cc's `is_flag_supported_inner(..).unwrap_or(false)`).
///
/// Nothing else would notice. Each `cc::Build` carries its own probe cache, so
/// every fixture probes on its own account and a canary elsewhere has no way to
/// observe a flag this build dropped; an `Err` is not even cached, so the loss can
/// be as small as a single translation unit which a later one silently retries.
/// The C++ comes out unchecked and the job reports success.
#[cfg_attr(feature = "nightly", doc(cfg(feature = "build")))]
pub fn add_sanitizer_flags(builder: &mut BuilderBuild) {
    apply_sanitizer_flags(builder, std::env::var_os("AUTOCXX_ASAN").is_some());
}

fn apply_sanitizer_flags(builder: &mut BuilderBuild, asan: bool) {
    for flag in sanitizer_flags(asan) {
        builder.flag(flag);
    }
}

/// For test purposes only, a [`cc::Build`] and lists of Rust and C++
/// files generated.
#[cfg_attr(feature = "nightly", doc(cfg(feature = "build")))]
pub struct BuilderSuccess(pub BuilderBuild, pub Vec<PathBuf>, pub Vec<PathBuf>);

/// Results of a build.
#[cfg_attr(feature = "nightly", doc(cfg(feature = "build")))]
pub type BuilderResult = Result<BuilderSuccess, BuilderError>;

/// The context in which a builder object lives. Callbacks for various
/// purposes.
#[cfg_attr(feature = "nightly", doc(cfg(feature = "build")))]
pub trait BuilderContext {
    /// Perform any initialization specific to the context in which this
    /// builder lives.
    fn setup() {}

    /// Create a dependency recorder, if any.
    fn get_dependency_recorder() -> Option<Box<dyn RebuildDependencyRecorder>>;

    /// Record that this build read the environment, for a build system which
    /// can invalidate on it. Called when a build runs, not when a builder is
    /// constructed: telling a build system about a dependency is what makes it
    /// stop watching everything it was watching by default, so a builder which
    /// is made and never built must not do it.
    fn record_environment_dependencies() {}
}

/// An object to allow building of bindings from a `build.rs` file.
///
/// It would be unusual to create this directly - see the `autocxx_build` or
/// `autocxx_gen` crates.
///
/// Once you've got one of these objects, you may set some configuration
/// options but then you're likely to want to call the [`build`] method.
///
/// # Setting C++ version
///
/// Ensure you use [`extra_clang_args`] as well as giving an appropriate
/// option to the [`cc::Build`] which you receive from the [`build`] function.
#[cfg_attr(feature = "nightly", doc(cfg(feature = "build")))]
pub struct Builder<'a, BuilderContext> {
    rs_file: PathBuf,
    autocxx_incs: Vec<OsString>,
    extra_clang_args: Vec<String>,
    dependency_recorder: Option<Box<dyn RebuildDependencyRecorder>>,
    custom_gendir: Option<PathBuf>,
    auto_allowlist: bool,
    codegen_options: CodegenOptions<'a>,
    // This member is to ensure that this type is parameterized
    // by a BuilderContext. The goal is to balance three needs:
    // (1) have most of the functionality over in autocxx_engine,
    // (2) expose this type to users of autocxx_build and to
    //     make it easy for callers simply to call Builder::new,
    // (3) ensure that such a Builder does a few tasks specific to its use
    // in a cargo environment.
    ctx: PhantomData<BuilderContext>,
}

impl<CTX: BuilderContext> Builder<'_, CTX> {
    /// Create a new Builder object. You'll need to pass in the Rust file
    /// which contains the bindings (typically an `include_cpp!` macro
    /// though `autocxx` can also handle manually-crafted `cxx::bridge`
    /// bindings), and a list of include directories which should be searched
    /// by autocxx as it tries to hunt for the include files specified
    /// within the `include_cpp!` macro.
    ///
    /// Usually after this you'd call [`build`].
    pub fn new(
        rs_file: impl AsRef<Path>,
        autocxx_incs: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> Self {
        CTX::setup();
        Self {
            rs_file: rs_file.as_ref().to_path_buf(),
            autocxx_incs: autocxx_incs
                .into_iter()
                .map(|s| s.as_ref().to_os_string())
                .collect(),
            extra_clang_args: Vec::new(),
            dependency_recorder: CTX::get_dependency_recorder(),
            custom_gendir: None,
            auto_allowlist: false,
            codegen_options: CodegenOptions::default(),
            ctx: PhantomData,
        }
    }

    /// Specify extra arguments for clang. These are used when parsing
    /// C++ headers. For example, you might want to provide
    /// `-std=c++17` to specify C++17.
    ///
    /// Calls accumulate: each one appends to the arguments already given, in
    /// call order, so a builder assembled out of several pieces - a helper which
    /// knows the C++ standard, another which knows the `-D`s - keeps every
    /// piece's flags. What a repeated option then means is clang's to decide; it
    /// takes the last `-std=`.
    pub fn extra_clang_args(mut self, extra_clang_args: &[&str]) -> Self {
        self.extra_clang_args
            .extend(extra_clang_args.iter().map(|s| s.to_string()));
        self
    }

    /// Where to generate the code.
    pub fn custom_gendir(mut self, custom_gendir: PathBuf) -> Self {
        self.custom_gendir = Some(custom_gendir);
        self
    }

    /// Update C++ code generation options. See [`CppCodegenOptions`] for details.
    pub fn cpp_codegen_options<F>(mut self, modifier: F) -> Self
    where
        F: FnOnce(&mut CppCodegenOptions),
    {
        modifier(&mut self.codegen_options.cpp_codegen_options);
        self
    }

    /// Automatically discover uses of the C++ `ffi` mod and generate the allowlist
    /// from that.
    /// This is a highly experimental option, not currently recommended.
    /// It doesn't work in the following cases:
    /// * Static function calls on types within the FFI mod.
    /// * Anything inside a macro invocation.
    /// * You're using a different name for your `ffi` mod
    /// * You're using multiple FFI mods
    /// * You've got usages scattered across files beyond that with the
    ///   `include_cpp` invocation
    /// * You're using `use` statements to rename mods or items. If this
    ///
    /// proves to be a promising or helpful direction, autocxx would be happy
    /// to accept pull requests to remove some of these limitations.
    pub fn auto_allowlist(mut self, do_it: bool) -> Self {
        self.auto_allowlist = do_it;
        self
    }

    #[doc(hidden)]
    /// Whether to force autocxx always to generate extra Rust and C++
    /// side shims. This is only used by the integration test suite to
    /// exercise more code paths - don't use it!
    pub fn force_wrapper_generation(mut self, do_it: bool) -> Self {
        self.codegen_options.force_wrapper_gen = do_it;
        self
    }

    /// Whether to suppress inclusion of system headers (`memory`, `string` etc.)
    /// from generated C++ bindings code. This should not normally be used,
    /// but can occasionally be useful if you're reducing a test case and you
    /// have a preprocessed header file which already contains absolutely everything
    /// that the bindings could ever need.
    pub fn suppress_system_headers(mut self, do_it: bool) -> Self {
        self.codegen_options
            .cpp_codegen_options
            .suppress_system_headers = do_it;
        self
    }

    /// An annotation optionally to include on each C++ function.
    /// For example to export the symbol from a library.
    pub fn cxx_impl_annotations(mut self, cxx_impl_annotations: Option<String>) -> Self {
        self.codegen_options
            .cpp_codegen_options
            .cxx_impl_annotations = cxx_impl_annotations;
        self
    }

    /// Build autocxx C++ files and return a [`cc::Build`] you can use to build
    /// more from a build.rs file.
    ///
    /// The error type returned by this function supports [`miette::Diagnostic`],
    /// so if you use the `miette` crate and its `fancy` feature, then simply
    /// return a `miette::Result` from your main function, you should get nicely
    /// printed diagnostics.
    ///
    /// As this is a [`cc::Build`] there are lots of options you can apply to
    /// the resulting options, but please bear in mind that these only apply
    /// to the build process for the generated code - such options will not
    /// influence autocxx's process for parsing header files.
    ///
    /// For example, if you wish to set the C++ version to C++17, you might
    /// be tempted to use [`cc::Build::flag_if_supported`] to add the
    /// `-std=c++17` flag. However, this won't affect the header parsing which
    /// autocxx does internally (by means of bindgen) so you _additionally_
    /// should call [`extra_clang_args`] with that same option.
    pub fn build(self) -> Result<BuilderBuild, BuilderError> {
        self.build_listing_files().map(|r| r.0)
    }

    /// For use in tests only, this does the build and returns additional information
    /// about the files generated which can subsequently be examined for correctness.
    /// In production, please use simply [`build`].
    pub fn build_listing_files(self) -> Result<BuilderSuccess, BuilderError> {
        let clang_args = &self
            .extra_clang_args
            .iter()
            .map(|s| &s[..])
            .collect::<Vec<_>>();
        rust_version_check();
        cxx_version_check()?;
        let gen_location_strategy = match self.custom_gendir {
            None => FileLocationStrategy::new(),
            Some(custom_dir) => FileLocationStrategy::Custom(custom_dir),
        };
        let incdir = gen_location_strategy.get_include_dir();
        ensure_created(&incdir)?;
        let cxxdir = gen_location_strategy.get_cxx_dir();
        ensure_created(&cxxdir)?;
        let rsdir = gen_location_strategy.get_rs_dir();
        ensure_created(&rsdir)?;
        // We are incredibly unsophisticated in our directory arrangement here
        // compared to cxx. I have no doubt that we will need to replicate just
        // about everything cxx does, in due course...
        // Write cxx.h to that location, as it may be needed by
        // some of our generated code.
        write_to_file(
            &incdir,
            "cxx.h",
            &get_cxx_header_bytes(
                self.codegen_options
                    .cpp_codegen_options
                    .suppress_system_headers,
            ),
        )?;

        let autocxx_inc = build_autocxx_inc(self.autocxx_incs, &incdir);
        gen_location_strategy.set_cargo_env_vars_for_build();

        // The file the whole build is definitionally reading. Recording the
        // headers and not this one is worse than recording nothing: a build
        // system told about any dependency at all stops watching everything
        // else (cargo's `rerun-if-changed` replaces its whole-package scan),
        // so an edit to `include_cpp!` which touches no header would leave
        // the previous generation in place and be compiled against.
        // Recorded before the parse, so that a file which fails to parse is
        // still watched for the edit which fixes it.
        if let Some(dep_recorder) = &self.dependency_recorder {
            dep_recorder.record_dependency(&self.rs_file.to_string_lossy());
        }
        CTX::record_environment_dependencies();
        let mut parsed_file = crate::parse_file(self.rs_file, self.auto_allowlist)
            .map_err(BuilderError::ParseError)?;
        parsed_file
            .resolve_all(
                autocxx_inc,
                clang_args,
                self.dependency_recorder,
                &self.codegen_options,
            )
            .map_err(BuilderError::ParseError)?;
        let mut counter = 0;
        let mut builder = cc::Build::new();
        builder.cpp(true);
        apply_msvc_flags(&mut builder, crate::clang_target::build_target_is_msvc());
        add_sanitizer_flags(&mut builder);
        let mut generated_rs = Vec::new();
        let mut generated_cpp = Vec::new();
        builder.includes(parsed_file.include_dirs());
        for include_cpp in parsed_file.get_cpp_buildables() {
            let generated_code = include_cpp
                .generate_h_and_cxx(&self.codegen_options.cpp_codegen_options)
                .map_err(BuilderError::InvalidCxx)?;
            for filepair in generated_code.0 {
                let fname = format!("gen{counter}.cxx");
                counter += 1;
                if let Some(implementation) = &filepair.implementation {
                    let gen_cxx_path = write_to_file(&cxxdir, &fname, implementation)?;
                    builder.file(&gen_cxx_path);
                    generated_cpp.push(gen_cxx_path);
                }
                write_to_file(&incdir, &filepair.header_name, &filepair.header)?;
                generated_cpp.push(incdir.join(filepair.header_name));
            }
        }

        for rs_output in parsed_file.get_rs_outputs() {
            let rs = generate_rs_single(rs_output);
            generated_rs.push(write_to_file(&rsdir, &rs.filename, rs.code.as_bytes())?);
        }
        if counter == 0 {
            Err(BuilderError::NoIncludeCxxMacrosFound)
        } else {
            Ok(BuilderSuccess(builder, generated_rs, generated_cpp))
        }
    }
}

fn ensure_created(dir: &Path) -> Result<(), BuilderError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| BuilderError::UnableToCreateDirectory(e, dir.to_path_buf()))
}

fn build_autocxx_inc<I, T>(paths: I, extra_path: &Path) -> Vec<PathBuf>
where
    I: IntoIterator<Item = T>,
    T: AsRef<OsStr>,
{
    paths
        .into_iter()
        .map(|p| PathBuf::from(p.as_ref()))
        .chain(std::iter::once(extra_path.to_path_buf()))
        .collect()
}

fn write_to_file(dir: &Path, filename: &str, content: &[u8]) -> Result<PathBuf, BuilderError> {
    let path = dir.join(filename);
    if let Ok(existing_contents) = std::fs::read(&path) {
        // Avoid altering timestamps on disk if the file already exists,
        // to stop downstream build steps recurring.
        if existing_contents == content {
            return Ok(path);
        }
    }
    try_write_to_file(&path, content).map_err(|e| BuilderError::FileWriteFail(e, path.clone()))?;
    Ok(path)
}

fn try_write_to_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(content)
}

fn rust_version_check() {
    if !version_check::is_min_version("1.54.0").unwrap_or(false) {
        panic!("Rust 1.54 or later is required.")
    }
}

/// Refuse to generate C++ which the `cxx` this build links could never link
/// against. See [`crate::cxx_version_parity`] for why the two can disagree and
/// when we can tell.
fn cxx_version_check() -> Result<(), BuilderError> {
    match crate::cxx_version_parity::detect_cxx_version_skew() {
        None => Ok(()),
        Some(skew) => Err(BuilderError::CxxVersionMismatch {
            cxx_version: skew.cxx_version,
            cxx_gen_mangling: skew.cxx_gen_mangling,
        }),
    }
}

/// Tests for the flags this crate puts on the [`cc::Build`] it hands back. They
/// are decided here, so nothing downstream can assert them.
///
/// What these pin is the *delivery*: that a flag which is required arrives
/// without a probe compile having to succeed first, which is the regression that
/// matters and the one `flag_if_supported` caused. Which builds are MSVC is
/// pinned next door by `clang_target`'s `target_is_msvc`. That the pair meet
/// correctly on a real cl is pinned by `test_cpp17` on the MSVC CI leg, which
/// asserts `__cplusplus >= 201703L` in a fixture header and can only pass if
/// `/Zc:__cplusplus` reached the compiler; no unit test here can stand in for it.
#[cfg(test)]
mod flag_tests {
    use super::{
        apply_msvc_flags, apply_sanitizer_flags, msvc_flags, sanitizer_flags, BuilderBuild,
    };

    /// The arguments a build ends up giving the compiler, for a `cc::Build` which
    /// cannot run a probe compile at all - there is no such compiler. That is the
    /// strongest form of the failure `flag_if_supported` reads as "unsupported",
    /// and the one it leaves no trace of.
    fn args_with_no_compiler_to_probe_with(
        target: &str,
        apply: impl FnOnce(&mut BuilderBuild),
    ) -> Vec<String> {
        let mut b = BuilderBuild::new();
        b.cpp(true)
            .cargo_metadata(false)
            // cc reads these from the environment a build script runs in, and
            // this is a test binary.
            .opt_level(1)
            .target(target)
            .host(target)
            .compiler("/no-such-directory-for-this-test/c++");
        apply(&mut b);
        b.try_get_compiler()
            .expect("cc could not describe the compiler")
            .args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The cl-only flags reach the compiler whether or not cc could run a probe
    /// compile of its own, which is what `flag_if_supported` had made them depend
    /// on. Fails if that mechanism comes back.
    ///
    /// Spelled out rather than read back from [`msvc_flags`], so that emptying
    /// that list cannot leave this passing with nothing to check.
    #[test]
    fn the_msvc_flags_arrive_with_no_compiler_to_probe_with() {
        let args = args_with_no_compiler_to_probe_with("x86_64-pc-windows-msvc", |b| {
            apply_msvc_flags(b, true)
        });
        for flag in ["/Zc:__cplusplus", "/EHsc"] {
            assert!(
                args.iter().any(|a| a == flag),
                "{flag} missing from {args:?}"
            );
        }
    }

    /// Instrumentation is the only thing `AUTOCXX_ASAN` is for, so it arrives on
    /// the same terms: a build whose C++ went uninstrumented has to fail rather
    /// than pass having checked nothing.
    ///
    /// Spelled out, as above.
    #[test]
    fn the_sanitizer_flag_arrives_with_no_compiler_to_probe_with() {
        let args = args_with_no_compiler_to_probe_with("x86_64-unknown-linux-gnu", |b| {
            apply_sanitizer_flags(b, true)
        });
        assert!(
            args.iter().any(|a| a == "-fsanitize=address"),
            "-fsanitize=address missing from {args:?}"
        );
    }

    /// Which flags belong to which condition, as literals, so that the tests
    /// above and the builds themselves cannot drift together into agreeing about
    /// nothing.
    #[test]
    fn each_condition_selects_exactly_its_own_flags() {
        assert_eq!(msvc_flags(true), ["/Zc:__cplusplus", "/EHsc"]);
        assert_eq!(sanitizer_flags(true), ["-fsanitize=address"]);
        // A build which did not ask for the sanitizer is not instrumented behind
        // the caller's back, and gcc and clang read `/Zc:__cplusplus` as the name
        // of a file to compile.
        assert!(msvc_flags(false).is_empty());
        assert!(sanitizer_flags(false).is_empty());
    }
}

#[cfg(test)]
mod clang_arg_tests {
    use super::{Builder, BuilderContext, RebuildDependencyRecorder};

    struct TestContext;

    impl BuilderContext for TestContext {
        fn get_dependency_recorder() -> Option<Box<dyn RebuildDependencyRecorder>> {
            None
        }
    }

    /// [`Builder::extra_clang_args`] appends, so a builder configured by more
    /// than one helper keeps every helper's flags rather than only the last
    /// helper's, in call order.
    #[test]
    fn extra_clang_args_accumulate() {
        let builder = Builder::<TestContext>::new("unused.rs", ["unused"])
            .extra_clang_args(&["-DFIRST", "-std=c++14"])
            .extra_clang_args(&["-DSECOND", "-std=c++17"]);
        assert_eq!(
            builder.extra_clang_args,
            ["-DFIRST", "-std=c++14", "-DSECOND", "-std=c++17"]
        );
    }
}
