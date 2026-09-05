// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Detection of a `cxx`/`cxx-gen` symbol naming mismatch.
//!
//! Since cxx 1.0.189, every symbol a `#[cxx::bridge]` produces carries the
//! patch level of the crate which produced it - `cxxbridge1$199$my_function` -
//! from `const CXXVERSION: &str = env!("CARGO_PKG_VERSION_PATCH")` in cxx's own
//! `syntax::mangle`. Before that release the same symbol was plain
//! `cxxbridge1$my_function`. The Rust half of each of those symbols is emitted
//! by the `cxx` crate the crate under construction links against; the C++ half
//! is emitted by the `cxx-gen` that `autocxx-engine` was built with. If the two
//! disagree - on the number, or on whether there is a number at all - every
//! generated function is a link error naming a symbol which appears in nobody's
//! source.
//!
//! Cargo resolves the two independently, and no manifest can require them to
//! match: "the same patch level as that other crate" is not something a version
//! requirement can say. Exact pins (`=1.0.199`) would say it, at the price of
//! stopping every downstream crate from choosing any other cxx, which is why
//! upstream cxx uses them only between crates it publishes in lockstep (`cxx`
//! depends on `cxxbridge-macro = "=1.0.199"`). autocxx cannot do the same: the
//! `cxx` in question belongs to the user's crate, not to us.
//!
//! Both requirements are ranges which normally resolve to the newest release,
//! so they normally agree. A lockfile which updates one and not the other does
//! not, and that is what this catches.

use once_cell::sync::OnceCell;
use quote::quote;

/// The cxx release which began putting the patch level into symbol names, and
/// so the point either side of which two cxx-shaped crates cannot link
/// together. Our declared floors are far below it (`cxx = "1.0.136"`,
/// `cxx-gen = "0.7.136"`), so a build straddling this line is reachable.
const FIRST_VERSIONED_MANGLING_PATCH: u32 = 189;

/// How a given cxx-shaped crate names the symbols it generates.
#[derive(Debug, PartialEq, Eq)]
enum Mangling {
    /// cxx 1.0.189 and later: `cxxbridge1$199$f`, carrying this patch level.
    Versioned(String),
    /// cxx before 1.0.189: `cxxbridge1$f`, carrying no version at all.
    Unversioned,
}

/// A `cxx`/`cxx-gen` symbol naming mismatch.
#[derive(Debug)]
pub(crate) struct CxxVersionSkew {
    /// Full version of the `cxx` crate the crate being built links against.
    pub(crate) cxx_version: String,
    /// How the `cxx-gen` we generate C++ with names its symbols, described for
    /// a human rather than for a parser.
    pub(crate) cxx_gen_mangling: String,
}

/// Reports a mismatch between the `cxx` being linked and the `cxx-gen` we
/// generate C++ with, when there is one we can see.
///
/// Returns `None` when they agree, and also when either is unknowable. We learn
/// which `cxx` is in play only from Cargo, and only when the crate being built
/// depends on `cxx` directly - which the autocxx tutorial has it do, since the
/// generated code names `::cxx`. A vendored or `[patch]`ed cxx is not in a
/// registry directory and so has no version to read, and `autocxx-gen` driven
/// by a build system other than Cargo has no `DEP_` variables at all. Saying
/// nothing is the only honest answer in those cases; the linker will still say
/// something, just less usefully.
pub(crate) fn detect_cxx_version_skew() -> Option<CxxVersionSkew> {
    // cxx declares `links = "cxxbridge1"` and its build script emits the path
    // of its own cxx.h, which Cargo passes to the build scripts of crates that
    // depend on cxx directly.
    let cxx_header = std::env::var("DEP_CXXBRIDGE1_HEADER").ok()?;
    skew_between(&cxx_header, cxx_gen_mangling()?)
}

/// The mismatch, if any, between the cxx at `cxx_header` and a `cxx-gen` which
/// names symbols as `cxx_gen_mangling` describes.
fn skew_between(cxx_header: &str, cxx_gen_mangling: &Mangling) -> Option<CxxVersionSkew> {
    let cxx_version = cxx_version_from_header_path(cxx_header)?;
    let cxx_patch = cxx_patch_level(&cxx_version)?;
    let agree = match cxx_gen_mangling {
        Mangling::Versioned(cxx_gen_patch) => cxx_patch.to_string() == *cxx_gen_patch,
        // A cxx-gen from before symbol versioning writes `cxxbridge1$f` where
        // any cxx from 1.0.189 on expects `cxxbridge1$189$f`. Neither carries a
        // number to compare, which is exactly why this case needs stating
        // separately: without it the most damaging skew of all would look like
        // nothing to report.
        Mangling::Unversioned => cxx_patch < FIRST_VERSIONED_MANGLING_PATCH,
    };
    if agree {
        return None;
    }
    Some(CxxVersionSkew {
        cxx_version,
        cxx_gen_mangling: match cxx_gen_mangling {
            Mangling::Versioned(patch) => format!("patch level {patch}"),
            Mangling::Unversioned => {
                "no patch level at all, being older than cxx 1.0.189".to_string()
            }
        },
    })
}

/// The patch level a `1.x` cxx will demand in mangled symbols.
///
/// cxx mangles `CARGO_PKG_VERSION_PATCH` into every bridge symbol from
/// 1.0.189 onward, whatever the minor version says - so every 1.x release
/// is judged by its patch component. Judging only `1.0.x` would let a
/// hypothetical `cxx 1.1.0` (demanding `$0$`) skew silently against a
/// `cxx-gen` mangling `$199$`, which is a guaranteed link failure. A major
/// version other than 1 is uncharted: no claim is made.
fn cxx_patch_level(cxx_version: &str) -> Option<u32> {
    let one_x = regex_static::static_regex!(r"^1\.(\d+)\.(\d+)$");
    let caps = one_x.captures(cxx_version)?;
    let minor: u32 = caps.get(1)?.as_str().parse().ok()?;
    let patch: u32 = caps.get(2)?.as_str().parse().ok()?;
    let _ = minor; // every 1.x minor is judged; the capture exists to pin the shape
    Some(patch)
}

/// The cxx version named by the path of the `cxx.h` being linked against.
///
/// cxx's build script emits `<manifest dir>/include/cxx.h`, and for a crate
/// from a registry Cargo's manifest dir is a directory named
/// `<crate>-<version>`. That directory name is the only place the version
/// appears - the header itself carries none.
///
/// The whole shape is therefore required, not just a version-looking path
/// component somewhere along the way: a vendored checkout under a directory
/// which happens to be called `cxx-1.0.190` would otherwise be read as cxx
/// 1.0.190 whatever it actually contains, and a build would be refused over a
/// version nothing in it has. Anything that is not the registry shape yields
/// `None` rather than a guess.
fn cxx_version_from_header_path(cxx_header: &str) -> Option<String> {
    let registry_layout =
        regex_static::static_regex!(r"(?:^|[/\\])cxx-(\d+\.\d+\.\d+)[/\\]include[/\\]cxx\.h$");
    Some(
        registry_layout
            .captures(cxx_header)?
            .get(1)?
            .as_str()
            .to_string(),
    )
}

/// How `cxx-gen` names the symbols it writes, read out of a throwaway bridge
/// rather than from any version string.
///
/// `cxx-gen` publishes neither its version nor its mangling scheme, so the only
/// way to learn either is to look at what it generates. That is also what
/// matters, rather than a version which merely ought to correspond to it.
///
/// `None` means the probe found no symbol it recognised at all, which is not
/// the same as finding an unversioned one: a cxx-gen whose output we cannot
/// read tells us nothing, whereas one which writes `cxxbridge1$f` has told us
/// it predates symbol versioning.
fn cxx_gen_mangling() -> Option<&'static Mangling> {
    static MANGLING: OnceCell<Option<Mangling>> = OnceCell::new();
    MANGLING
        .get_or_init(|| {
            let probe = quote! {
                #[cxx::bridge]
                mod ffi {
                    unsafe extern "C++" {
                        fn autocxx_cxx_version_probe();
                    }
                }
            };
            let generated =
                cxx_gen::generate_header_and_cc(probe, &cxx_gen::Opt::default()).ok()?;
            let cc = String::from_utf8(generated.implementation).ok()?;
            mangling_of_probe_symbol(&cc)
        })
        .as_ref()
}

/// Reads the probe function's symbol out of generated C++.
fn mangling_of_probe_symbol(generated_cc: &str) -> Option<Mangling> {
    let symbol = regex_static::static_regex!(r"cxxbridge1\$(\d+\$)?autocxx_cxx_version_probe\b");
    let captures = symbol.captures(generated_cc)?;
    Some(match captures.get(1) {
        Some(patch) => Mangling::Versioned(patch.as_str().trim_end_matches('$').to_string()),
        None => Mangling::Unversioned,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        cxx_gen_mangling, cxx_version_from_header_path, mangling_of_probe_symbol, skew_between,
        Mangling,
    };

    /// If cxx ever stops putting the patch level in its symbols, or renames the
    /// scheme, this is where we find out - rather than by silently checking
    /// nothing from then on.
    #[test]
    fn probe_reads_the_mangling_of_the_cxx_gen_we_build_with() {
        match cxx_gen_mangling().expect("no cxxbridge1 symbol in the probe's generated C++") {
            Mangling::Versioned(patch) => assert!(
                patch.parse::<u32>().is_ok(),
                "mangled patch level {patch:?} is not a number"
            ),
            // Reachable in principle - our floor is cxx-gen 0.7.136 - but not
            // with any cxx-gen this repo has ever locked.
            Mangling::Unversioned => {
                panic!("cxx-gen unexpectedly generated pre-1.0.189 symbol names")
            }
        }
    }

    #[test]
    fn both_symbol_shapes_are_recognised() {
        assert_eq!(
            mangling_of_probe_symbol("void cxxbridge1$199$autocxx_cxx_version_probe() noexcept {"),
            Some(Mangling::Versioned("199".to_string()))
        );
        assert_eq!(
            mangling_of_probe_symbol("void cxxbridge1$autocxx_cxx_version_probe() noexcept {"),
            Some(Mangling::Unversioned)
        );
        assert_eq!(mangling_of_probe_symbol("void unrelated() {}"), None);
    }

    #[test]
    fn version_comes_from_the_registry_directory_name() {
        assert_eq!(
            cxx_version_from_header_path(
                "/home/me/.cargo/registry/src/x/cxx-1.0.199/include/cxx.h"
            )
            .as_deref(),
            Some("1.0.199")
        );
        assert_eq!(
            cxx_version_from_header_path(
                r"C:\Users\me\.cargo\registry\src\x\cxx-1.0.199\include\cxx.h"
            )
            .as_deref(),
            Some("1.0.199")
        );
    }

    #[test]
    fn a_cxx_we_cannot_place_is_not_guessed_at() {
        // A `[patch]`ed or vendored checkout: no version to read, so no claim.
        assert_eq!(
            cxx_version_from_header_path("/home/me/src/cxx/include/cxx.h"),
            None
        );
        assert!(skew_between(
            "/home/me/src/cxx/include/cxx.h",
            &Mangling::Versioned("199".to_string())
        )
        .is_none());
    }

    #[test]
    fn only_the_crate_directory_itself_names_the_version() {
        // A version-looking ancestor of a vendored tree says nothing about the
        // cxx underneath it, and must not be read as if it did.
        assert_eq!(
            cxx_version_from_header_path("/home/me/cxx-1.0.190/vendor/cxx/include/cxx.h"),
            None
        );
        // Nor does a directory whose name merely ends in the crate's.
        assert_eq!(
            cxx_version_from_header_path("/home/me/notcxx-1.0.190/include/cxx.h"),
            None
        );
        // Nor a header which is not the one cxx publishes.
        assert_eq!(
            cxx_version_from_header_path("/home/me/cxx-1.0.190/include/cxx.h.orig"),
            None
        );
        assert_eq!(
            cxx_version_from_header_path("/home/me/cxx-1.0.190/src/cxx.h"),
            None
        );
    }

    #[test]
    fn matching_patch_levels_are_not_a_skew() {
        assert!(skew_between(
            "/x/cxx-1.0.199/include/cxx.h",
            &Mangling::Versioned("199".to_string())
        )
        .is_none());
    }

    #[test]
    fn differing_patch_levels_are_reported_with_both_versions() {
        let skew = skew_between(
            "/x/cxx-1.0.190/include/cxx.h",
            &Mangling::Versioned("199".to_string()),
        )
        .expect("1.0.190 and cxx-gen 0.7.199 mangle different symbols");
        assert_eq!(skew.cxx_version, "1.0.190");
        assert_eq!(skew.cxx_gen_mangling, "patch level 199");
    }

    #[test]
    fn a_generator_predating_symbol_versioning_is_a_skew_against_a_modern_cxx() {
        // The case with no numbers to compare: cxx-gen 0.7.136 writes
        // `cxxbridge1$f` while cxx 1.0.189 looks for `cxxbridge1$189$f`.
        let skew = skew_between("/x/cxx-1.0.189/include/cxx.h", &Mangling::Unversioned)
            .expect("an unversioned generator cannot link against cxx 1.0.189");
        assert_eq!(skew.cxx_version, "1.0.189");
        assert!(
            skew.cxx_gen_mangling.contains("1.0.189"),
            "the message should say what the generator is too old for: {}",
            skew.cxx_gen_mangling
        );
        // ... and is fine against a cxx of its own era.
        assert!(
            skew_between("/x/cxx-1.0.188/include/cxx.h", &Mangling::Unversioned).is_none(),
            "two crates from before symbol versioning agree"
        );
    }

    #[test]
    fn every_1_x_release_is_judged_by_its_patch_component() {
        // cxx mangles CARGO_PKG_VERSION_PATCH into symbols from 1.0.189 on,
        // whatever the minor version - so a hypothetical cxx 1.1.0 demands
        // `$0$` and genuinely skews against a cxx-gen mangling `$199$`.
        // Silently accepting that would be a guaranteed link failure.
        assert!(skew_between(
            "/x/cxx-1.1.0/include/cxx.h",
            &Mangling::Versioned("199".to_string())
        )
        .is_some());
        // A matching patch agrees even across the minor bump.
        assert!(skew_between(
            "/x/cxx-1.1.199/include/cxx.h",
            &Mangling::Versioned("199".to_string())
        )
        .is_none());
        // A major version other than 1 is uncharted: no claim either way.
        assert!(skew_between(
            "/x/cxx-2.0.0/include/cxx.h",
            &Mangling::Versioned("199".to_string())
        )
        .is_none());
    }
}
