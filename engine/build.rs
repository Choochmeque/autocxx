// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Builds the vendored copy of bindgen which `src/lib.rs` mounts as
//! `crate::vendored_bindgen`.
//!
//! autocxx needs facts about C++ which bindgen does not report - access
//! specifiers, special members, virtualness, the original C++ spelling of a
//! nested name - so it used to depend on `autocxx-bindgen`, a fork republished
//! under its own name. A fork has to be rebased by hand to pick up anything
//! upstream fixes, and it fell far enough behind to miss the clang-22
//! declaration-canonicalization fix. Instead, the bindgen sources come from a
//! submodule pinned at an upstream tag, and the delta lives in
//! `third_party/patches` where it can be read as a patch series and rebased
//! with `git`. This script applies that series into `OUT_DIR` and rewrites the
//! result into something which compiles as a module rather than a crate.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The submodule, and the directory inside it holding the library crate:
/// rust-bindgen is a workspace, and `bindgen/` is the one member autocxx wants.
const SUBMODULE_LIB: &str = "third_party/rust-bindgen/bindgen";

/// The same sources, flattened, for the published crate.
///
/// `cargo package` prunes every directory below the package root which contains
/// a `Cargo.toml`, treating it as a separate package, and neither `include` nor
/// `exclude` lifts that. bindgen's sources sit under two such manifests, so the
/// submodule cannot reach crates.io as it stands. `AUTOCXX_VENDOR_BINDGEN=1`
/// copies them here with the manifests renamed, and this directory is read only
/// where there is no submodule to read instead - see `bindgen_sources`.
const VENDORED_LIB: &str = "third_party/bindgen-src";

/// What a `Cargo.toml` is called in `VENDORED_LIB`, so that cargo does not read
/// the directory as a package of its own.
const RENAMED_MANIFEST: &str = "Cargo.toml.upstream";

const PATCH_DIR: &str = "third_party/patches";

/// Set to copy `SUBMODULE_LIB` to `VENDORED_LIB` before building. Run before
/// `cargo publish`; see `book/src/contributing.md`.
const VENDOR_ENV: &str = "AUTOCXX_VENDOR_BINDGEN";

/// The bindgen cargo features autocxx resolves for the vendored copy. A module
/// cannot carry its own features - a `feature = "x"` inside it would ask about
/// `autocxx-engine`'s features - so each one is rewritten to a `cfg` which is
/// unconditionally true or false, except the two which autocxx re-exports.
const FEATURES: &[(&str, &str)] = &[
    // bindgen's logging goes through `log`, which autocxx already depends on.
    ("logging", "all()"),
    // `Formatter::Prettyplease`, which autocxx never selects: it asks for
    // `Formatter::Rustfmt` when logging is on and `Formatter::None` otherwise.
    ("prettyplease", "any()"),
    // The CLI, which autocxx does not ship. Keeping it out is what lets the
    // vendored copy avoid depending on `clap`.
    ("__cli", "any()"),
    // `annotate-snippets` diagnostics, off in the published crate too.
    ("experimental", "any()"),
    ("__testing_only_extra_assertions", "any()"),
    ("__testing_only_libclang_16", "any()"),
    ("__testing_only_libclang_20", "any()"),
    ("__testing_only_libclang_22", "any()"),
    // These two are autocxx-engine features of the same name, forwarded to
    // clang-sys, so the vendored code can keep asking about them by name.
    ("runtime", "feature = \"runtime\""),
    ("static", "feature = \"static\""),
    ("libcpp", "any()"),
];

/// Sources copied but never rewritten.
///
/// `codegen/bitfield_unit.rs` is `include_str!`'d into the bindings bindgen
/// generates, so its text is not autocxx's code to rewrite - a `crate::` turned
/// into `crate::vendored_bindgen::` here would land in a user's crate. It is
/// also compiled, under `#[cfg(test)]`, which is why it is copied at all.
///
/// `log_stubs.rs` defines macros named `warn`, `debug` and so on. Rewriting
/// those names to `::log::warn!` would corrupt the definitions. It is compiled
/// only when bindgen's `logging` feature is off, which for autocxx is never.
const VERBATIM: &[&str] = &["codegen/bitfield_unit.rs", "log_stubs.rs"];

/// Macros bindgen reaches through `#[macro_use] extern crate`, which only works
/// at a crate root. Invocations are rewritten to absolute paths instead, so
/// that they resolve from any depth inside the vendored module - including the
/// inline `mod`s which a per-file `use` would not reach.
const MACROS: &[(&str, &str)] = &[
    ("bitflags", "::bitflags::bitflags"),
    ("quote", "::quote::quote"),
    ("quote_spanned", "::quote::quote_spanned"),
    ("format_ident", "::quote::format_ident"),
    ("warn", "::log::warn"),
    ("error", "::log::error"),
    ("info", "::log::info"),
    ("debug", "::log::debug"),
    ("trace", "::log::trace"),
];

/// Inner attributes dropped from the vendored sources.
///
/// `recursion_limit` is a crate-root attribute, and bindgen asks for the
/// default value anyway. The rest are upstream's lint policy: a `deny` inside a
/// module beats the `allow` on the module root below, so autocxx would be held
/// to bindgen's lint choices under autocxx's clippy configuration, which is not
/// the same one bindgen's own CI runs.
const DROPPED_ATTRS: &[&str] = &[
    "#![deny(missing_docs)]",
    "#![deny(unused_extern_crates)]",
    "#![deny(clippy::disallowed_methods)]",
    "#![deny(clippy::missing_docs_in_private_items)]",
    "#![recursion_limit = \"128\"]",
];

/// Prepended to the vendored module root.
///
/// The `allow`s are about whose code this is: autocxx denies `unsafe` in its own
/// sources and lints them, but bindgen calls libclang and is linted by bindgen's
/// own CI against bindgen's own configuration. A warning here is not something
/// autocxx can fix without diverging from upstream, which is the one thing this
/// arrangement exists to avoid.
const MODULE_PREAMBLE: &str = "\
#![allow(unsafe_code)]
#![allow(clippy::all, clippy::pedantic)]
#![allow(unused_qualifications)]
// bindgen re-exports its whole public API and autocxx calls a fraction of it;
// as a private module the rest is dead.
#![allow(unused_imports)]
#![allow(dead_code)]
";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The triple autocxx is being compiled for, recorded verbatim because it
    // is not available any other way at runtime: cargo sets `TARGET` for build
    // scripts and nothing else, and the `cfg`s rustc leaves behind name a
    // family (`arm`, and no vendor at all) rather than the target. `src/
    // clang_target.rs` needs it to tell clang what to parse for when autocxx
    // is not itself being run from a build script.
    let target = std::env::var("TARGET").expect("cargo sets TARGET for every build script");
    println!("cargo:rustc-env=AUTOCXX_COMPILED_TARGET={target}");

    // Carried over from the build script bindgen has of its own, which this one
    // replaces: on behalf of clang-sys, rebuild when the configuration naming
    // which libclang to use changes, so that bindings get regenerated rather
    // than kept from a different clang.
    for var in [
        "LLVM_CONFIG_PATH",
        "LIBCLANG_PATH",
        "LIBCLANG_STATIC_PATH",
        "BINDGEN_EXTRA_CLANG_ARGS",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    println!("cargo:rerun-if-env-changed=BINDGEN_EXTRA_CLANG_ARGS_{target}");
    println!(
        "cargo:rerun-if-env-changed=BINDGEN_EXTRA_CLANG_ARGS_{}",
        target.replace('-', "_")
    );

    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    vendor_bindgen(&manifest, &out, &target);
}

fn vendor_bindgen(manifest: &Path, out: &Path, target: &str) {
    println!("cargo:rerun-if-env-changed={VENDOR_ENV}");
    if std::env::var_os(VENDOR_ENV).is_some() {
        flatten_for_publishing(manifest);
    }

    let source = bindgen_sources(manifest);
    let patches = manifest.join(PATCH_DIR);
    let dest = out.join("vendored_bindgen");

    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed={}", patches.display());

    let mut files = BTreeMap::new();
    collect(&source, &source, &mut files);
    for patch in patch_series(&patches) {
        apply_patch(&patch, &mut files);
    }

    if dest.exists() {
        fs::remove_dir_all(&dest).expect("cannot clear the vendored bindgen directory");
    }

    // `#[path]` takes a string literal, so the only place the OUT_DIR path can
    // be spelled is here, where it is known.
    fs::write(
        out.join("vendored_bindgen_mount.rs"),
        format!(
            "#[path = {:?}]\nmod vendored_bindgen;\n",
            dest.join("mod.rs").display().to_string()
        ),
    )
    .expect("cannot write the vendored bindgen mount");

    for (relative, text) in files {
        // A module's children are looked up beside its own file, so the root
        // has to be `mod.rs` for `codegen/`, `ir/` and `options/` to resolve.
        let written = if relative == Path::new("lib.rs") {
            PathBuf::from("mod.rs")
        } else {
            relative.clone()
        };
        let path = dest.join(&written);
        fs::create_dir_all(path.parent().expect("a file has a parent"))
            .expect("cannot create the vendored bindgen directory");
        fs::write(&path, rewrite(&relative, &text, target))
            .expect("cannot write a vendored source");
    }
}

/// Where bindgen's sources are.
///
/// The submodule wins whenever it is checked out, which is the whole of the
/// difference between a git checkout and a published crate. The other way round
/// would be worse than a wrong answer: `flatten_for_publishing` leaves its copy
/// behind, so preferring it would mean that everyone who had once prepared a
/// release silently kept building that copy, and a submodule bumped to a new
/// tag would compile as if nothing had changed.
fn bindgen_sources(manifest: &Path) -> PathBuf {
    let submodule = manifest.join(SUBMODULE_LIB);
    if submodule.join("lib.rs").is_file() {
        return submodule;
    }
    let vendored = manifest.join(VENDORED_LIB);
    assert!(
        vendored.join("lib.rs").is_file(),
        "{} is empty. It is a git submodule holding the bindgen sources \
         autocxx builds against; run `git submodule update --init --recursive` \
         and build again.",
        submodule.display()
    );
    vendored
}

/// Copy the submodule's sources to `VENDORED_LIB` so that `cargo package` can
/// carry them, renaming each `Cargo.toml` so cargo does not prune the directory
/// as a package of its own.
fn flatten_for_publishing(manifest: &Path) {
    let from = manifest.join(SUBMODULE_LIB);
    let to = manifest.join(VENDORED_LIB);
    assert!(
        from.join("lib.rs").is_file(),
        "{VENDOR_ENV} is set but {} is empty; run `git submodule update --init \
         --recursive` first",
        from.display()
    );
    if to.exists() {
        fs::remove_dir_all(&to).expect("cannot clear the flattened bindgen directory");
    }
    copy_renaming_manifests(&from, &to);
    println!(
        "cargo:warning=vendored bindgen sources into {}",
        to.display()
    );
}

fn copy_renaming_manifests(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("cannot create the flattened bindgen directory");
    for entry in fs::read_dir(from).expect("cannot read the bindgen sources") {
        let path = entry.expect("cannot read a bindgen source entry").path();
        let name = path.file_name().expect("a directory entry has a name");
        if path.is_dir() {
            copy_renaming_manifests(&path, &to.join(name));
        } else {
            let name = if name == "Cargo.toml" {
                RENAMED_MANIFEST.as_ref()
            } else {
                name
            };
            fs::copy(&path, to.join(name)).expect("cannot copy a bindgen source");
        }
    }
}

/// Every `.rs` file under `dir`, keyed by its path relative to the source root.
fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, String>) {
    for entry in fs::read_dir(dir).expect("cannot read the bindgen sources") {
        let path = entry.expect("cannot read a bindgen source entry").path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let relative = path
                .strip_prefix(root)
                .expect("walked from the root")
                .to_path_buf();
            let text = fs::read_to_string(&path).expect("cannot read a bindgen source");
            files.insert(relative, text);
        }
    }
}

/// The patch files, in the order their names put them in.
fn patch_series(dir: &Path) -> Vec<PathBuf> {
    let mut patches: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("cannot read a patch entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "patch"))
        .collect();
    patches.sort();
    assert!(
        !patches.is_empty(),
        "no patches in {} - autocxx cannot use bindgen unpatched",
        dir.display()
    );
    patches
}

/// Apply one patch file, which may touch several sources.
///
/// The point of applying a patch rather than committing a patched copy is that a
/// hunk whose surroundings moved stops the build here, naming the file and the
/// patch, rather than compiling something nobody wrote. What that does not catch
/// is a release which changed the code around a hunk without changing the hunk's
/// own context, or which changed something no hunk touches: the series still
/// applies, correctly, to different code. Nothing pins the tag, so diffing
/// generated output across a submodule bump is the check that catches those -
/// see `book/src/contributing.md`.
fn apply_patch(patch: &Path, files: &mut BTreeMap<PathBuf, String>) {
    let text = fs::read_to_string(patch)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", patch.display()));
    for (target, body) in split_per_file(&text, patch) {
        let parsed = diffy::Patch::from_str(&body).unwrap_or_else(|e| {
            panic!(
                "{} does not parse its hunks for {}: {e}",
                patch.display(),
                target.display()
            )
        });
        let before = files.get(&target).unwrap_or_else(|| {
            panic!(
                "{} patches {}, which is not in the bindgen submodule",
                patch.display(),
                target.display()
            )
        });
        let after = diffy::apply(before, &parsed).unwrap_or_else(|e| {
            panic!(
                "{} does not apply to {}: {e}. The patch series is rebased onto \
                 one bindgen tag; check the submodule is at that tag.",
                patch.display(),
                target.display()
            )
        });
        files.insert(target, after);
    }
}

/// Split a multi-file unified diff into one single-file diff per target.
///
/// A section starts at a `--- ` line immediately followed by a `+++ ` line;
/// inside a hunk body a removed line is prefixed with a single `-`, so the only
/// way to fake that pair would be source text which itself begins `-- ` and is
/// followed by source beginning `++ `.
fn split_per_file(text: &str, patch: &Path) -> Vec<(PathBuf, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut sections: Vec<(PathBuf, String)> = Vec::new();
    let mut current: Option<(PathBuf, Vec<&str>)> = None;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let starts_section = line.starts_with("--- ")
            && lines
                .get(i + 1)
                .is_some_and(|next| next.starts_with("+++ "));
        if starts_section {
            if let Some((name, body)) = current.take() {
                sections.push((name, body.join("\n") + "\n"));
            }
            let target = lines[i + 1]["+++ ".len()..].trim();
            let target = target.strip_prefix("b/").unwrap_or(target);
            current = Some((PathBuf::from(target), vec![line, lines[i + 1]]));
            i += 2;
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
        i += 1;
    }
    if let Some((name, body)) = current.take() {
        sections.push((name, body.join("\n") + "\n"));
    }
    assert!(
        !sections.is_empty(),
        "{} contains no file sections",
        patch.display()
    );
    sections
}

/// Turn one bindgen source into something which compiles as a module of
/// autocxx-engine rather than as part of its own crate.
fn rewrite(relative: &Path, text: &str, target: &str) -> String {
    if VERBATIM.contains(&relative.to_string_lossy().replace('\\', "/").as_str()) {
        return text.to_string();
    }

    let mut text = text.to_string();

    // Cargo features of the bindgen crate, which the vendored module is not.
    for (name, replacement) in FEATURES {
        text = text.replace(&format!("feature = \"{name}\""), replacement);
    }

    // `#[macro_use] extern crate` is a crate-root construct; the macros it
    // brought into scope are reached by path below instead.
    for name in ["bitflags", "quote", "log"] {
        text = text.replace(&format!("#[macro_use]\nextern crate {name};\n"), "");
    }

    for (name, path) in MACROS {
        text = replace_macro_invocation(&text, name, path);
    }

    // bindgen's build script writes the host triple to a file next to itself
    // and `lib.rs` reads it back. A module cannot: `env!("OUT_DIR")` inside one
    // reached through `include!` does not resolve, and the value is known right
    // here anyway.
    text = text.replace(
        "include_str!(concat!(env!(\"OUT_DIR\"), \"/host-target.txt\"))",
        &format!("{target:?}"),
    );

    // Every path bindgen writes as `crate::` is now one module deeper.
    text = text.replace("crate::", "crate::vendored_bindgen::");

    for attr in DROPPED_ATTRS {
        text = text.replace(&format!("{attr}\n"), "");
    }

    if relative == Path::new("lib.rs") {
        text = format!("{MODULE_PREAMBLE}{text}");
    }

    text
}

/// Rewrite `name!` to `path!`, leaving `name` alone anywhere it is not a macro
/// invocation - including `macro_rules! name`, which defines rather than calls,
/// and `r#name!`, which is a different identifier.
///
/// This is text, not tokens, so it cannot tell a macro call from the same
/// spelling inside a string literal or inside a `quote!` body destined for a
/// user's crate. No such spelling exists in the bindgen release the submodule is
/// pinned at, and if one arrives in a later release the generated-output diff
/// across the bump is what shows it - which is why that diff is in the rolling
/// checklist rather than optional.
fn replace_macro_invocation(text: &str, name: &str, path: &str) -> String {
    let needle = format!("{name}!");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(&needle) {
        let (before, after) = rest.split_at(at);
        // A macro name is one identifier: if what precedes it could continue an
        // identifier or a path, or makes it a raw identifier, this is some other
        // name.
        let raw = before.ends_with("r#");
        let preceded_by_ident = before
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':');
        let defines = before.trim_end().ends_with("macro_rules!");
        out.push_str(before);
        if raw || preceded_by_ident || defines {
            out.push_str(&needle);
        } else {
            out.push_str(path);
            out.push('!');
        }
        rest = &after[needle.len()..];
    }
    out.push_str(rest);
    out
}
