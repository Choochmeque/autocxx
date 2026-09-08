// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Which target we are generating bindings for: telling clang about it in the
//! case where it cannot work that out for itself, and answering the one
//! question about it that the generated C++ has to be told the answer to (see
//! [`expected_wchar_t_size`]).
//!
//! # The problem
//!
//! bindgen parses headers with libclang; the generated C++ is compiled by
//! whatever C++ compiler the build is using. The two halves must agree about
//! the C++ ABI, because bindgen's `#[link_name]` attributes carry mangled
//! symbol names which only that compiler will define, and because the layout
//! bindgen records has to be the layout the compiler produces.
//!
//! Nothing passes the target triple to libclang unless somebody asks for it.
//! bindgen picks a target (`--target=` in the clang args, else `TARGET`, else
//! its own build-time host triple) but deliberately declines to pass it on
//! when it equals its host triple, leaving libclang to use the triple it was
//! *built* for. Almost everywhere those are the same thing.
//!
//! Windows is where they part company. One OS and one architecture there host
//! two mutually unintelligible C++ ABIs - Microsoft's and the Itanium ABI that
//! mingw-w64 uses - and which one a clang uses is fixed when that clang is
//! built. The usual LLVM release for Windows defaults to `*-pc-windows-msvc`,
//! so a `x86_64-pc-windows-gnu` Rust build gets bindings parsed against
//! Microsoft's standard library and mangled in Microsoft's ABI, while g++
//! compiles the generated C++ against libstdc++ and mangles in the Itanium
//! ABI. Most of autocxx survives that, because calls cross the boundary
//! through C++ shims which the same g++ compiles; what does not survive is
//! anything naming a symbol directly, i.e. every C++ variable exposed to Rust
//! (see [`crate::conversion::parse::linkage`], which reads the linkage of a
//! variable out of its mangled name and cannot do so from an ABI the compiler
//! is not using).
//!
//! # What we do about it
//!
//! Pass `--target=` for Windows targets, where the triple says which of the
//! two ABIs to use and clang has no way to guess.
//!
//! Rust and clang spell those triples the same way with one exception, and it
//! is not a spelling clang tolerates: `*-pc-windows-gnullvm` makes clang stop
//! with `version 'llvm' in target triple ... is invalid`. `gnullvm` is a
//! Rust-only name for the mingw-w64 environment built with LLVM's runtime
//! libraries instead of gcc's; LLVM has no such environment, so its triple
//! parser reads the `gnu` it recognises and takes the `llvm` after it for an
//! environment version number, which is not a number. That is not a matter of
//! being behind: LLVM 21 rejects it exactly as LLVM 19 does, and adding the
//! environment is still an open request upstream. So there is no
//! newer-clang spelling to prefer, and one answer serves every clang.
//!
//! The answer is `-gnu`, which is what rustc itself hands LLVM for these
//! targets, and it loses nothing we need: what makes gnullvm gnullvm is which
//! unwinder and compiler runtime get linked and which C runtime the headers
//! come from, none of which a C++ ABI depends on or a triple records. The
//! mangling, the calling convention and the record layout are mingw's either
//! way.
//!
//! This is a fix for the mismatch we have evidence of, not a claim that every
//! other libclang default is right: a libclang whose default triple differs
//! from the target's in some other way - a C library, say - would go wrong the
//! same way, and we would not currently catch it. What stops us being explicit
//! everywhere is that clang's native default triple carries information the
//! Rust triple does not, and overriding it would lose that: the macOS version
//! (`arm64-apple-darwin24.6.0`) availability attributes are checked against,
//! and the vendor field that steers the search for a system GCC installation
//! on Linux. So this stays scoped to the mismatch we have actually seen.

/// The triple autocxx itself was compiled for, captured by `build.rs` because
/// nothing else records it: cargo sets `TARGET` for build scripts only, and
/// rustc's `cfg`s name an architecture family rather than a target.
const COMPILED_TARGET: &str = env!("AUTOCXX_COMPILED_TARGET");

/// The environment variable bindgen takes extra clang arguments from.
const BINDGEN_EXTRA_CLANG_ARGS: &str = "BINDGEN_EXTRA_CLANG_ARGS";

/// The tail of the one Rust Windows triple clang refuses to parse, and what to
/// put in its place. See the module documentation for why these are the same
/// target as far as clang is concerned.
const RUST_GNULLVM_TAIL: &str = "-windows-gnullvm";
const CLANG_GNULLVM_TAIL: &str = "-windows-gnu";

/// The `--target=` argument clang needs in order to parse headers the way the
/// C++ compiler will compile them, or `None` where clang's own default is
/// right.
///
/// `rust_target` is a Rust target triple.
fn clang_target_arg_for(rust_target: &str) -> Option<String> {
    // Rust spells its Windows targets `<arch>-<vendor>-windows-<env>`, and
    // clang understands those spellings but for the environment `gnullvm`,
    // which it rejects outright. (Other targets need rewriting for clang -
    // `riscv64gc` and `-espidf` are not things clang has heard of - which is
    // one more reason to leave them alone.)
    if !rust_target.contains("-windows-") {
        return None;
    }
    // The vendor field goes through as it stands. `uwp` and `win7` are no more
    // known to clang than `gnullvm` is, but an unrecognised vendor is a vendor
    // clang has no opinion about rather than a parse error, and on Windows it
    // has no opinion to have: the environment is what picks the ABI. Checked -
    // clang predefines exactly the same macros for `i686-win7-windows-gnu` as
    // for `i686-pc-windows-gnu`, and for `x86_64-uwp-windows-msvc` as for
    // `x86_64-pc-windows-msvc`.
    let clang_target = match rust_target.strip_suffix(RUST_GNULLVM_TAIL) {
        Some(arch_and_vendor) => format!("{arch_and_vendor}{CLANG_GNULLVM_TAIL}"),
        None => rust_target.to_owned(),
    };
    Some(format!("--target={clang_target}"))
}

/// Whether any of these clang arguments already says what target to parse for,
/// in either of the two spellings clang and bindgen accept.
fn args_specify_target<'a>(mut args: impl Iterator<Item = &'a str>) -> bool {
    args.any(|arg| arg == "-target" || arg.starts_with("--target="))
}

/// Which target to tell clang about, if any.
///
/// Split out from its surroundings so that the order of precedence can be
/// tested without a test having to reach into the process environment.
///
/// * `env_target` is cargo's `TARGET`, present when autocxx is being run from
///   a build script.
/// * `compiled_target` is the triple autocxx itself was compiled for.
/// * `caller_args` are the extra clang arguments autocxx was given.
/// * `bindgen_env_args` are the extra clang arguments bindgen will take from
///   the environment, already looked up and split.
fn choose_clang_target_arg(
    env_target: Option<&str>,
    compiled_target: &str,
    caller_args: &[&str],
    bindgen_env_args: &[String],
) -> Option<String> {
    // Anyone who has said which target to parse for outranks us, whether they
    // said it to autocxx or to bindgen. Saying it again would leave clang
    // taking the last `--target` and bindgen's own bookkeeping taking the
    // first, which are different answers.
    if args_specify_target(caller_args.iter().copied())
        || args_specify_target(bindgen_env_args.iter().map(String::as_str))
    {
        return None;
    }
    // `TARGET` is the target of the crate being built, which is what we want;
    // `compiled_target` is the target autocxx itself was built for, which is
    // the same thing except in a build script, where autocxx is compiled for
    // the host and `TARGET` is there to say so.
    clang_target_arg_for(env_target.unwrap_or(compiled_target))
}

/// The extra clang arguments bindgen will add from the environment, looked up
/// the way bindgen looks them up and split the way bindgen splits them.
///
/// bindgen appends these *after* the arguments we give it, so a `--target` in
/// here is the one clang would end up obeying.
fn bindgen_extra_clang_args(env_target: Option<&str>) -> Vec<String> {
    // bindgen tries the target-suffixed spellings first and takes the first
    // variable which is set, rather than concatenating them.
    let value = env_target
        .and_then(|target| {
            std::env::var(format!("{BINDGEN_EXTRA_CLANG_ARGS}_{target}"))
                .or_else(|_| {
                    std::env::var(format!(
                        "{BINDGEN_EXTRA_CLANG_ARGS}_{}",
                        target.replace('-', "_")
                    ))
                })
                .ok()
        })
        .or_else(|| std::env::var(BINDGEN_EXTRA_CLANG_ARGS).ok());
    match value {
        None => Vec::new(),
        // As bindgen does: if it will not parse as a shell word list, it is
        // one big argument.
        Some(value) => shlex::split(&value).unwrap_or_else(|| vec![value]),
    }
}

/// The `--target=` argument to add to a clang invocation, given the extra
/// clang args the caller supplied.
///
/// `None` means leave the target alone: either somebody has already said what
/// it is, or clang's default is right.
pub(crate) fn extra_clang_target_arg(extra_args: &[&str]) -> Option<String> {
    let env_target = std::env::var("TARGET").ok();
    choose_clang_target_arg(
        env_target.as_deref(),
        COMPILED_TARGET,
        extra_args,
        &bindgen_extra_clang_args(env_target.as_deref()),
    )
}

/// How many bytes `wchar_t` is on the target, for the generated C++ to hold its
/// compiler to.
///
/// `autocxx::c_wchar_t` wraps `autocxx::wchar_t`, which is a `cfg` over the
/// target, and a C++ compiler can be told to disagree with the target:
/// `-fshort-wchar` makes `wchar_t` two bytes where the platform says four.
/// cxx checks that an extern type is trivial but never that it is the size Rust
/// thinks, so a disagreement is not caught anywhere - every `c_wchar_t` would
/// simply be read from the wrong bytes. A `static_assert` in the generated
/// header turns that into a compile error.
///
/// Only the width matters to a layout, so this is the two-way split behind
/// `autocxx::wchar_t`'s four arms rather than the arms themselves, and it is
/// derived from a target name rather than from `#[cfg]`, which here would
/// describe the machine autocxx was compiled for. The predicates were checked
/// against every triple `rustc --print target-list` names: all twenty Windows
/// targets contain `-windows-`, Cygwin and UEFI are the two other 16-bit ABIs,
/// and `avr` and `msp430` are 16-bit because their `int` is. A name is the
/// weaker of the two sources - see [`wchar_t_size_from_cargo_cfg`], which is
/// asked first.
///
/// The name may also be one a caller gave clang rather than one rustc knows,
/// and clang spells the mingw-w64 environment `x86_64-w64-mingw32` as well as
/// `x86_64-pc-windows-gnu`. Same target, same two-byte `wchar_t`, no `windows`
/// in the name.
fn wchar_t_size_for_target(rust_target: &str) -> u32 {
    let arch = rust_target.split('-').next().unwrap_or_default();
    if rust_target.contains("-windows-")
        || rust_target.contains("mingw")
        || rust_target.contains("-cygwin")
        || rust_target.contains("-uefi")
        || matches!(arch, "avr" | "msp430")
    {
        2
    } else {
        4
    }
}

/// The same, read off cargo's own description of the target rather than its
/// name, or `None` where cargo is not the one asking.
///
/// This is what the crate's `build.rs` does for the C++ it compiles itself, and
/// it outranks the triple because it is the only source which is right for a
/// [custom target specification], whose name is a file and says nothing about
/// the machine. `CARGO_CFG_*` describes the target being built for, not the host
/// the build script runs on, so it is also the right answer while
/// cross-compiling.
///
/// [custom target specification]: https://doc.rust-lang.org/rustc/targets/custom.html
fn wchar_t_size_from_cargo_cfg() -> Option<u32> {
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").ok()?;
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    Some(
        if std::env::var_os("CARGO_CFG_WINDOWS").is_some()
            || matches!(target_os.as_str(), "cygwin" | "uefi")
            || matches!(target_arch.as_str(), "avr" | "msp430")
        {
            2
        } else {
            4
        },
    )
}

/// Which target to answer questions about the generated C++ with, where nothing
/// cargo said is available.
///
/// A `--target` given to clang, by us or through bindgen's environment
/// variable, is the target whose headers are being parsed, and so the target the
/// generated C++ belongs to. Otherwise cargo's `TARGET`, which is set for the
/// build scripts autocxx is almost always run from, and last the triple autocxx
/// itself was compiled for. A build which gets as far as that last one while
/// cross-compiling has told nothing in the chain which target it is for -
/// bindgen parses for its own host too - so it is already generating bindings
/// for the wrong machine, and an assertion which fires is the first thing to say
/// so rather than a false alarm.
fn target_to_describe<'a>(
    env_target: Option<&'a str>,
    compiled_target: &'a str,
    caller_args: &[&'a str],
    bindgen_env_args: &'a [String],
) -> &'a str {
    let args = caller_args
        .iter()
        .copied()
        .chain(bindgen_env_args.iter().map(String::as_str));
    // The last one, which is the one clang obeys; bindgen appends the
    // environment's arguments after ours. Both spellings, because both are
    // accepted - and `-target` takes the triple as the argument after it.
    let mut explicit = None;
    let mut take_next = false;
    for arg in args {
        if std::mem::take(&mut take_next) {
            explicit = Some(arg);
        } else if arg == "-target" {
            take_next = true;
        } else if let Some(target) = arg.strip_prefix("--target=") {
            explicit = Some(target);
        }
    }
    explicit.or(env_target).unwrap_or(compiled_target)
}

/// How many bytes the generated C++ is to assert that `wchar_t` is, given the
/// extra clang arguments the caller supplied. See [`wchar_t_size_for_target`].
pub(crate) fn expected_wchar_t_size(extra_clang_args: &[&str]) -> u32 {
    if let Some(size) = wchar_t_size_from_cargo_cfg() {
        return size;
    }
    let env_target = std::env::var("TARGET").ok();
    wchar_t_size_for_target(target_to_describe(
        env_target.as_deref(),
        COMPILED_TARGET,
        extra_clang_args,
        &bindgen_extra_clang_args(env_target.as_deref()),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        args_specify_target, choose_clang_target_arg, clang_target_arg_for, target_to_describe,
        wchar_t_size_for_target,
    };

    const GNU: &str = "x86_64-pc-windows-gnu";
    const MSVC: &str = "x86_64-pc-windows-msvc";
    const LINUX: &str = "x86_64-unknown-linux-gnu";

    fn choose(
        env_target: Option<&str>,
        compiled_target: &str,
        caller_args: &[&str],
        bindgen_env_args: &[&str],
    ) -> Option<String> {
        let bindgen_env_args: Vec<String> =
            bindgen_env_args.iter().map(|s| s.to_string()).collect();
        choose_clang_target_arg(env_target, compiled_target, caller_args, &bindgen_env_args)
    }

    #[test]
    fn windows_targets_are_passed_to_clang() {
        // Both ABIs, not just the one the usual clang gets wrong, so that a
        // clang built for the other default cannot get it wrong either. The
        // sub-architecture and vendor fields have to survive too: they are part
        // of what tells clang which mangling and calling convention to use.
        for target in [
            GNU,
            MSVC,
            "i686-pc-windows-gnu",
            "aarch64-pc-windows-msvc",
            "arm64ec-pc-windows-msvc",
            "thumbv7a-pc-windows-msvc",
            "x86_64-uwp-windows-msvc",
            "i686-win7-windows-gnu",
        ] {
            assert_eq!(
                clang_target_arg_for(target),
                Some(format!("--target={target}")),
                "{target} should be passed to clang verbatim"
            );
        }
    }

    /// The one Windows environment whose Rust spelling clang will not parse.
    /// These are the answers rustc itself gives LLVM for these three targets.
    #[test]
    fn the_gnullvm_environment_is_renamed_for_clang() {
        for (rust_target, llvm_target) in [
            ("x86_64-pc-windows-gnullvm", "x86_64-pc-windows-gnu"),
            ("i686-pc-windows-gnullvm", "i686-pc-windows-gnu"),
            ("aarch64-pc-windows-gnullvm", "aarch64-pc-windows-gnu"),
        ] {
            assert_eq!(
                clang_target_arg_for(rust_target),
                Some(format!("--target={llvm_target}")),
                "{rust_target} should reach clang as {llvm_target}"
            );
        }
    }

    /// `gnullvm` is a suffix of nothing else, but the rewrite is a string
    /// operation, so pin that it does not fire on the environment it is named
    /// after or on an architecture which happens to contain the letters.
    #[test]
    fn only_the_environment_field_is_rewritten() {
        for target in [GNU, "i686-pc-windows-gnu", "aarch64-pc-windows-msvc"] {
            assert_eq!(
                clang_target_arg_for(target),
                Some(format!("--target={target}")),
                "{target} has no gnullvm environment to rewrite"
            );
        }
    }

    #[test]
    fn other_targets_are_left_to_clang() {
        // Notably including the triples clang would reject or misread if we
        // handed it the Rust spelling.
        for target in [
            LINUX,
            "aarch64-apple-darwin",
            "riscv64gc-unknown-linux-gnu",
            "xtensa-esp32-espidf",
            "wasm32-unknown-unknown",
        ] {
            assert_eq!(
                clang_target_arg_for(target),
                None,
                "{target} should be left to clang's own default"
            );
        }
    }

    #[test]
    fn cargos_target_outranks_the_one_we_were_compiled_for() {
        // The build script case: autocxx is compiled for the host and `TARGET`
        // is the only thing which knows what is actually being built.
        assert_eq!(
            choose(Some(GNU), MSVC, &[], &[]),
            Some(format!("--target={GNU}"))
        );
        // ...including when it takes us off Windows altogether, where clang's
        // own default is the better answer.
        assert_eq!(choose(Some(LINUX), GNU, &[], &[]), None);
    }

    #[test]
    fn without_cargos_target_we_use_the_one_we_were_compiled_for() {
        // Everything which is not a build script: the `autocxx-gen` command,
        // our own test suite. autocxx is compiled for the same target as the
        // code it is generating bindings for.
        assert_eq!(choose(None, GNU, &[], &[]), Some(format!("--target={GNU}")));
        assert_eq!(choose(None, LINUX, &[], &[]), None);
    }

    #[test]
    fn a_target_from_the_caller_wins() {
        // Whether they said it to autocxx...
        assert_eq!(
            choose(Some(GNU), GNU, &["--target=i686-pc-windows-gnu"], &[]),
            None
        );
        assert_eq!(
            choose(Some(GNU), GNU, &["-target", "i686-pc-windows-gnu"], &[]),
            None
        );
        // ...or to bindgen, through BINDGEN_EXTRA_CLANG_ARGS, which bindgen
        // appends after ours so clang would obey theirs anyway.
        assert_eq!(
            choose(Some(GNU), GNU, &[], &["--target=i686-pc-windows-gnu"]),
            None
        );
        assert_eq!(
            choose(Some(GNU), GNU, &[], &["-target", "i686-pc-windows-gnu"]),
            None
        );
        // Arguments which are not a target do not count as one.
        assert_eq!(
            choose(
                Some(GNU),
                GNU,
                &["-std=c++17"],
                &["--sysroot=/x", "--target-help"]
            ),
            Some(format!("--target={GNU}"))
        );
    }

    #[test]
    fn nothing_is_mistaken_for_a_target_flag() {
        assert!(args_specify_target(
            ["--target=x86_64-pc-windows-gnu"].into_iter()
        ));
        assert!(args_specify_target(
            ["-target", "x86_64-pc-windows-gnu"].into_iter()
        ));
        assert!(!args_specify_target(
            ["-x", "c++", "-std=c++14"].into_iter()
        ));
        assert!(!args_specify_target(["--target-help"].into_iter()));
        assert!(!args_specify_target(["--targets"].into_iter()));
    }

    /// `build.rs` records this, so a mistake there would otherwise show up
    /// only as clang being told to parse for nothing in particular.
    #[test]
    fn we_know_what_we_were_compiled_for() {
        let target = super::COMPILED_TARGET;
        assert!(
            target.split('-').count() >= 3,
            "{target:?} does not look like a target triple"
        );
    }

    #[test]
    fn a_caller_who_names_a_target_gets_no_second_one() {
        let args: Vec<_> =
            crate::make_clang_args(&[], &["--target=x86_64-pc-windows-gnu"]).collect();
        assert_eq!(
            args.iter()
                .filter(|arg| arg.starts_with("--target"))
                .collect::<Vec<_>>(),
            vec!["--target=x86_64-pc-windows-gnu"]
        );
    }

    /// The bug this module exists for was that a Windows build said nothing
    /// about the target at all, so pin that it now does. Only meaningful on
    /// Windows, which is where the bug was and where CI runs both ABIs.
    #[test]
    #[cfg(target_os = "windows")]
    fn on_windows_the_clang_args_name_a_target() {
        // Unless something in the environment has taken the decision away from
        // us, in which case there is nothing here to assert.
        if std::env::var("TARGET").is_ok_and(|target| !target.contains("-windows-")) {
            return;
        }
        if !super::bindgen_extra_clang_args(std::env::var("TARGET").ok().as_deref()).is_empty() {
            return;
        }
        let args: Vec<_> = crate::make_clang_args(&[], &[]).collect();
        assert!(
            args.iter()
                .any(|arg| arg.starts_with("--target=") && arg.contains("-windows-")),
            "{args:?} should name the Windows target to parse for"
        );
    }

    /// The targets whose `wchar_t` is two bytes, as clang's `__WCHAR_TYPE__`
    /// reports them, and a sample of the four-byte majority. Asserted against
    /// triples rather than `cfg`s so that a cross build's answer is tested too.
    #[test]
    fn wchar_t_is_two_bytes_where_the_abi_says_so() {
        for target in [
            GNU,
            MSVC,
            "i686-pc-windows-gnullvm",
            "aarch64-uwp-windows-msvc",
            "x86_64-win7-windows-gnu",
            "x86_64-pc-cygwin",
            "x86_64-unknown-uefi",
            "aarch64-unknown-uefi",
            "avr-none",
            "avr-unknown-gnu-atmega328",
            "msp430-none-elf",
            // Spellings rustc does not use but clang does, which reach here
            // when a caller names a target in the clang arguments.
            "x86_64-w64-mingw32",
            "i686-pc-mingw32",
        ] {
            assert_eq!(
                wchar_t_size_for_target(target),
                2,
                "{target} has a 16-bit wchar_t"
            );
        }
        for target in [
            LINUX,
            "aarch64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "powerpc64-ibm-aix",
            "wasm32-unknown-unknown",
            "riscv64gc-unknown-linux-gnu",
        ] {
            assert_eq!(
                wchar_t_size_for_target(target),
                4,
                "{target} has a 32-bit wchar_t"
            );
        }
    }

    /// A `--target` the caller gave clang is the target the generated C++
    /// belongs to, whatever the environment says, because it is what the headers
    /// were parsed as. Both spellings clang accepts, and the last one wins, as
    /// clang itself does.
    #[test]
    fn an_explicit_clang_target_outranks_the_environment() {
        let no_env_args: Vec<String> = Vec::new();
        let windows_env_args = vec![format!("--target={MSVC}")];
        for (caller_args, env_args, expected) in [
            (vec![format!("--target={GNU}")], &no_env_args, GNU),
            (
                vec!["-target".to_string(), GNU.to_string()],
                &no_env_args,
                GNU,
            ),
            (
                vec![format!("--target={LINUX}"), format!("--target={GNU}")],
                &no_env_args,
                GNU,
            ),
            // bindgen appends the environment's arguments after ours.
            (vec![format!("--target={LINUX}")], &windows_env_args, MSVC),
            (Vec::new(), &windows_env_args, MSVC),
            // Nobody said: cargo's `TARGET`, then what autocxx was built for.
            (Vec::new(), &no_env_args, LINUX),
        ] {
            let caller_args: Vec<&str> = caller_args.iter().map(String::as_str).collect();
            assert_eq!(
                target_to_describe(Some(LINUX), "aarch64-apple-darwin", &caller_args, env_args),
                expected,
                "{caller_args:?} with environment {env_args:?}"
            );
        }
        assert_eq!(
            target_to_describe(None, MSVC, &[], &no_env_args),
            MSVC,
            "with nothing else to go on, the triple autocxx was compiled for"
        );
    }

    /// A `cfg`-built answer for the machine the tests are running on, which is
    /// the machine whose C++ compiler the suite's generated headers are checked
    /// against. Belt and braces for the triple-matching above.
    #[test]
    fn the_host_wchar_t_size_agrees_with_this_targets_cfgs() {
        let expected = if cfg!(any(windows, target_os = "cygwin", target_os = "uefi"))
            || cfg!(any(target_arch = "avr", target_arch = "msp430"))
        {
            2
        } else {
            4
        };
        assert_eq!(
            wchar_t_size_for_target(super::COMPILED_TARGET),
            expected,
            "{} disagrees with this build's cfgs",
            super::COMPILED_TARGET
        );
    }
}
