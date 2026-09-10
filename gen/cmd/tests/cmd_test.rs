// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{fs::File, io::Write, path::Path};

use indexmap::map::IndexMap as HashMap;

use assert_cmd::Command;
use autocxx_integration_tests::{build_from_folder, RsFindMode};
use itertools::Itertools;
use tempfile::{tempdir, TempDir};

static MAIN_RS: &str = concat!(
    include_str!("../../../demo/src/main.rs"),
    "#[link(name = \"autocxx-demo\")]\nextern \"C\" {}"
);
static INPUT_H: &str = include_str!("../../../demo/src/input.h");
static BLANK: &str = "// Blank autocxx placeholder";

static MAIN2_RS: &str = concat!(
    include_str!("data/main2.rs"),
    "#[link(name = \"autocxx-demo\")]\nextern \"C\" {}"
);
static DIRECTIVE1_RS: &str = include_str!("data/directive1.rs");
static DIRECTIVE2_RS: &str = include_str!("data/directive2.rs");
static INPUT2_H: &str = include_str!("data/input2.h");
static INPUT3_H: &str = include_str!("data/input3.h");
static CXX_VOCABULARY_H: &str = include_str!("data/cxx_vocabulary.h");
static CXX_VOCABULARY_RS: &str = include_str!("data/cxx_vocabulary.rs");
static RESERVED_RUST_TYPE_H: &str = include_str!("data/reserved_rust_type.h");
static RESERVED_RUST_TYPE_RS: &str = include_str!("data/reserved_rust_type.rs");

/// The fixture for [`test_gen_archive_with_discovered_extern_rust_fn`]: an
/// ordinary `include_cpp!` plus an `extern_rust_function` outside it, which the
/// codegen discovers and appends to the block's config.
///
/// The archive is keyed on that config, so this is the shape which proves the
/// key the codegen files bindings under is the key the macro looks them up by.
static EXTERN_RUST_FN_RS: &str = concat!(
    "
use autocxx::prelude::*;
include_cpp! {
    #include \"input.h\"
    safety!(unsafe_ffi)
    generate!(\"DoMath\")
}

#[autocxx::extern_rust::extern_rust_function]
pub fn called_from_cpp() {}

fn main() {
    assert_eq!(ffi::DoMath(4), 12);
}
",
    "#[link(name = \"autocxx-demo\")]\nextern \"C\" {}"
);

/// The fixture for [`test_gen_nested_mod`]: an `include_cpp!` written inside a
/// `mod`, which the file's `include!` has to reach.
///
/// The generated file is named after the block, not after where the block sits,
/// so the name a nested block computes is the name the codegen wrote - this is
/// the shape which proves it.
static NESTED_MOD_RS: &str = concat!(
    "
mod inner {
    autocxx::include_cpp! {
        #include \"input.h\"
        safety!(unsafe_ffi)
        generate!(\"DoMath\")
    }
    pub use ffi::DoMath;
}

fn main() {
    assert_eq!(inner::DoMath(4), 12);
}
",
    "#[link(name = \"autocxx-demo\")]\nextern \"C\" {}"
);

/// The fixture for [`test_gen_nested_mod_with_discovered_extern_rust_fn`]: the
/// same nested block, plus an `extern_rust_function` outside it for the codegen
/// to discover.
///
/// A discovery pass which only looked at the top level would conclude this file
/// has no block at all and make one, which is a second block named `ffi` beside
/// the real one - and bindings nothing includes.
static NESTED_MOD_EXTERN_RUST_FN_RS: &str = concat!(
    "
mod inner {
    autocxx::include_cpp! {
        #include \"input.h\"
        safety!(unsafe_ffi)
        generate!(\"DoMath\")
    }
    pub use ffi::DoMath;
}

#[autocxx::extern_rust::extern_rust_function]
pub fn called_from_cpp() {}

fn main() {
    assert_eq!(inner::DoMath(4), 12);
}
",
    "#[link(name = \"autocxx-demo\")]\nextern \"C\" {}"
);

const KEEP_TEMPDIRS: bool = true;

/// The fixture for [`test_asan_working_as_expected_for_cpp_from_folder`]: a
/// header, the translation unit which does the damage, and a `main` which calls
/// it through autocxx.
///
/// The write is in C++, in a file of its own, because that is the half of the
/// build this canary is about. Both `volatile`s are load-bearing: on the size,
/// so that the compiler cannot see the access is out of bounds and refuse it at
/// compile time under `-Werror`; on the pointee, so that a store nothing reads
/// survives the `opt_level(1)` these fixtures compile at - without it the write
/// is deleted as dead and the sanitizer has nothing to report, with or without
/// instrumentation.
static DOOM_H: &str = "
#pragma once
void scribble_past_the_end();
";
static DOOM_CC: &str = "
#include \"input.h\"
#include <cstddef>
static volatile std::size_t one = 1;
void scribble_past_the_end() {
    volatile char* p = new char[one];
    p[one] = 'x';
    delete[] const_cast<char*>(p);
}
";
static DOOM_RS: &str = concat!(
    "
use autocxx::prelude::*;
include_cpp! {
    #include \"input.h\"
    safety!(unsafe_ffi)
    generate!(\"scribble_past_the_end\")
}

fn main() {
    ffi::scribble_past_the_end();
}
",
    "#[link(name = \"autocxx-demo\")]\nextern \"C\" {}"
);

/// The fixture for [`test_wasm_target_keeps_functions`]: a class with an inline
/// method and a free function, which must both survive being parsed for a
/// `wasm32-*` target.
///
/// It includes no system header, because nothing here has a wasi sysroot for
/// clang to find one in: the point is what the triple alone does to a header
/// clang can read either way.
static WASM_H: &str = "
#pragma once

struct Point {
  int x;
  int y;
};

class Rect {
public:
  Point top_left;
  Point bottom_right;
  int width() const { return bottom_right.x - top_left.x; }
};

inline int stretch(int by) { return by * 2; }
";
static WASM_RS: &str = "
use autocxx::prelude::*;
include_cpp! {
    #include \"input.h\"
    safety!(unsafe)
    generate_pod!(\"Rect\")
    generate!(\"stretch\")
}

fn main() {}
";

#[test]
fn test_help() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("autocxx-gen")?;
    cmd.arg("-h").assert().success();
    Ok(())
}

enum RsGenMode {
    Single,
    Archive,
}

fn base_test<F>(
    tmp_dir: &TempDir,
    rs_gen_mode: RsGenMode,
    arg_modifier: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&mut Command),
{
    let mut standard_files = HashMap::new();
    standard_files.insert("input.h", INPUT_H.as_bytes());
    standard_files.insert("main.rs", MAIN_RS.as_bytes());
    let result = base_test_ex(
        tmp_dir,
        rs_gen_mode,
        arg_modifier,
        standard_files,
        vec!["main.rs"],
    );
    assert_contentful(tmp_dir, "gen0.cc");
    result
}

fn base_test_ex<F>(
    tmp_dir: &TempDir,
    rs_gen_mode: RsGenMode,
    arg_modifier: F,
    files_to_write: HashMap<&str, &[u8]>,
    files_to_process: Vec<&str>,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&mut Command),
{
    let demo_code_dir = tmp_dir.path().join("demo");
    std::fs::create_dir(&demo_code_dir).unwrap();
    for (filename, content) in files_to_write {
        write_to_file(&demo_code_dir, filename, content);
    }
    let mut cmd = Command::cargo_bin("autocxx-gen")?;
    arg_modifier(&mut cmd);
    cmd.arg("--inc")
        .arg(demo_code_dir.to_str().unwrap())
        .arg("--outdir")
        .arg(tmp_dir.path().to_str().unwrap())
        .arg("--gen-cpp")
        .arg("--generate-cxx-h");
    cmd.arg(match rs_gen_mode {
        RsGenMode::Single => "--gen-rs-include",
        RsGenMode::Archive => "--gen-rs-archive",
    });
    for file in files_to_process {
        cmd.arg(demo_code_dir.join(file));
    }
    let output = cmd.output();
    if let Ok(output) = output {
        eprintln!("Cmd stdout: {:?}", std::str::from_utf8(&output.stdout));
        eprintln!("Cmd stderr: {:?}", std::str::from_utf8(&output.stderr));
    }
    cmd.assert().success();
    Ok(())
}

#[test]
fn test_gen() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    base_test(&tmp_dir, RsGenMode::Single, |_| {})?;
    std::env::set_var("OUT_DIR", tmp_dir.path().to_str().unwrap());
    let r = build_from_folder(
        tmp_dir.path(),
        &tmp_dir.path().join("demo/main.rs"),
        vec![tmp_dir.path().join("autocxx-ffi-default-gen.rs")],
        &["gen0.cc"],
        RsFindMode::AutocxxRs,
    );
    if KEEP_TEMPDIRS {
        println!("Tempdir: {:?}", tmp_dir.into_path().to_str());
    }
    r.unwrap();
    Ok(())
}

/// The canary for the C++ half of `AUTOCXX_ASAN` on this path, which is the one
/// autocxx's own builder never sees: everything here compiles its C++ with a
/// `cc::Build` [`build_from_folder`] makes itself, so a fixture's C++ was built
/// bare while its Rust was instrumented, and corruption like the below was
/// written and never reported.
///
/// A no-op unless `AUTOCXX_ASAN` is set, exactly as the integration suite's two
/// doom tests are. It insists on a *sanitizer report*, not merely a failure: a
/// fixture which failed to compile would satisfy the second and prove nothing.
///
/// Reading that report needs the `autocxx-trybuild-child` helper, which cargo
/// builds only for a run that includes the `autocxx-integration-tests` package -
/// so `cargo test -p autocxx-gen` on its own leaves it on this process's stderr
/// and fails here saying so.
#[test]
fn test_asan_working_as_expected_for_cpp_from_folder() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("AUTOCXX_ASAN").is_none() {
        return Ok(());
    }
    let tmp_dir = tempdir()?;
    let mut files = HashMap::new();
    files.insert("input.h", DOOM_H.as_bytes());
    files.insert("doom.cc", DOOM_CC.as_bytes());
    files.insert("main.rs", DOOM_RS.as_bytes());
    base_test_ex(&tmp_dir, RsGenMode::Single, |_| {}, files, vec!["main.rs"])?;
    let err = build_from_folder(
        tmp_dir.path(),
        &tmp_dir.path().join("demo/main.rs"),
        vec![tmp_dir.path().join("autocxx-ffi-default-gen.rs")],
        &["gen0.cc", "demo/doom.cc"],
        RsFindMode::AutocxxRs,
    )
    .expect_err("the C++ wrote past the end of a heap allocation and nothing objected");
    let report = format!("{err:?}");
    // The first says trybuild built the fixture and went on to run it, which
    // rules out the whole build-failure path: a C++ compiler's own diagnostics
    // come back through this same error, and a compiler that was itself
    // sanitized could otherwise supply the other two words.
    for expected in [
        "Test case failed at runtime",
        "AddressSanitizer",
        "heap-buffer-overflow",
    ] {
        assert!(
            report.contains(expected),
            "the fixture failed without mentioning {expected:?}, so this says nothing \
             about the sanitizer: {report}"
        );
    }
    Ok(())
}

#[test]
fn test_gen_archive() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    base_test(&tmp_dir, RsGenMode::Archive, |_| {})?;
    let r = build_from_folder(
        tmp_dir.path(),
        &tmp_dir.path().join("demo/main.rs"),
        vec![tmp_dir.path().join("gen.rs.json")],
        &["gen0.cc"],
        RsFindMode::AutocxxRsArchive,
    );
    if KEEP_TEMPDIRS {
        println!("Tempdir: {:?}", tmp_dir.into_path().to_str());
    }
    r.unwrap();
    Ok(())
}

/// An archive build of a file whose `include_cpp!` the codegen augments before
/// writing the archive - here with a discovered `extern_rust_function`.
///
/// The other archive tests all use `demo/src/main.rs`, whose config the codegen
/// leaves exactly as written, so they say nothing about which version of the
/// config the key is taken from.
#[test]
fn test_gen_archive_with_discovered_extern_rust_fn() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    let mut files = HashMap::new();
    files.insert("input.h", INPUT_H.as_bytes());
    files.insert("main.rs", EXTERN_RUST_FN_RS.as_bytes());
    base_test_ex(&tmp_dir, RsGenMode::Archive, |_| {}, files, vec!["main.rs"])?;
    let r = build_from_folder(
        tmp_dir.path(),
        &tmp_dir.path().join("demo/main.rs"),
        vec![tmp_dir.path().join("gen.rs.json")],
        &["gen0.cc"],
        RsFindMode::AutocxxRsArchive,
    );
    if KEEP_TEMPDIRS {
        println!("Tempdir: {:?}", tmp_dir.into_path().to_str());
    }
    r.unwrap();
    Ok(())
}

#[test]
fn test_gen_nested_mod() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    let mut files = HashMap::new();
    files.insert("input.h", INPUT_H.as_bytes());
    files.insert("main.rs", NESTED_MOD_RS.as_bytes());
    base_test_ex(&tmp_dir, RsGenMode::Single, |_| {}, files, vec!["main.rs"])?;
    std::env::set_var("OUT_DIR", tmp_dir.path().to_str().unwrap());
    let r = build_from_folder(
        tmp_dir.path(),
        &tmp_dir.path().join("demo/main.rs"),
        vec![tmp_dir.path().join("autocxx-ffi-default-gen.rs")],
        &["gen0.cc"],
        RsFindMode::AutocxxRs,
    );
    if KEEP_TEMPDIRS {
        println!("Tempdir: {:?}", tmp_dir.into_path().to_str());
    }
    r.unwrap();
    Ok(())
}

#[test]
fn test_gen_nested_mod_with_discovered_extern_rust_fn() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    let mut files = HashMap::new();
    files.insert("input.h", INPUT_H.as_bytes());
    files.insert("main.rs", NESTED_MOD_EXTERN_RUST_FN_RS.as_bytes());
    base_test_ex(&tmp_dir, RsGenMode::Single, |_| {}, files, vec!["main.rs"])?;
    // The block these bindings are for is the nested one - there is no other -
    // so the discovered function has reached it. Had it gone to a block
    // synthesised beside it, this file would be that block's and would not
    // name the function.
    let bindings = std::fs::read_to_string(tmp_dir.path().join("autocxx-ffi-default-gen.rs"))?;
    assert!(
        bindings.contains("called_from_cpp"),
        "the discovered function did not reach the block"
    );
    std::env::set_var("OUT_DIR", tmp_dir.path().to_str().unwrap());
    let r = build_from_folder(
        tmp_dir.path(),
        &tmp_dir.path().join("demo/main.rs"),
        vec![tmp_dir.path().join("autocxx-ffi-default-gen.rs")],
        &["gen0.cc"],
        RsFindMode::AutocxxRs,
    );
    if KEEP_TEMPDIRS {
        println!("Tempdir: {:?}", tmp_dir.into_path().to_str());
    }
    r.unwrap();
    Ok(())
}

#[test]
fn test_gen_archive_first_entry() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    base_test(&tmp_dir, RsGenMode::Archive, |_| {})?;
    let r = build_from_folder(
        tmp_dir.path(),
        &tmp_dir.path().join("demo/main.rs"),
        vec![tmp_dir.path().join("gen.rs.json")],
        &["gen0.cc"],
        RsFindMode::Custom(Box::new(|path: &Path| {
            vec![(
                "AUTOCXX_RS_JSON_ARCHIVE".to_string(),
                std::env::join_paths([&path.join("gen.rs.json"), Path::new("/nonexistent")])
                    .unwrap(),
            )]
        })),
    );
    if KEEP_TEMPDIRS {
        println!("Tempdir: {:?}", tmp_dir.into_path().to_str());
    }
    r.unwrap();
    Ok(())
}

#[test]
fn test_gen_archive_second_entry() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    base_test(&tmp_dir, RsGenMode::Archive, |_| {})?;
    let r = build_from_folder(
        tmp_dir.path(),
        &tmp_dir.path().join("demo/main.rs"),
        vec![tmp_dir.path().join("gen.rs.json")],
        &["gen0.cc"],
        RsFindMode::Custom(Box::new(|path: &Path| {
            vec![(
                "AUTOCXX_RS_JSON_ARCHIVE".to_string(),
                std::env::join_paths([Path::new("/nonexistent"), &path.join("gen.rs.json")])
                    .unwrap(),
            )]
        })),
    );
    if KEEP_TEMPDIRS {
        println!("Tempdir: {:?}", tmp_dir.into_path().to_str());
    }
    r.unwrap();
    Ok(())
}

#[test]
fn test_gen_multiple_in_archive() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;

    let mut files = HashMap::new();
    files.insert("input2.h", INPUT2_H.as_bytes());
    files.insert("input3.h", INPUT3_H.as_bytes());
    files.insert("main.rs", MAIN2_RS.as_bytes());
    files.insert("directive1.rs", DIRECTIVE1_RS.as_bytes());
    files.insert("directive2.rs", DIRECTIVE2_RS.as_bytes());
    base_test_ex(
        &tmp_dir,
        RsGenMode::Archive,
        |cmd| {
            cmd.arg("--generate-exact").arg("8");
        },
        files,
        vec!["directive1.rs", "directive2.rs"],
    )?;
    // We've asked to create 8 C++ files, mostly blank. Build 'em all.
    let cpp_files = (0..7).map(|id| format!("gen{id}.cc")).collect_vec();
    let cpp_files = cpp_files.iter().map(|s| s.as_str()).collect_vec();
    let r = build_from_folder(
        tmp_dir.path(),
        &tmp_dir.path().join("demo/main.rs"),
        vec![tmp_dir.path().join("gen.rs.json")],
        &cpp_files,
        RsFindMode::AutocxxRsArchive,
    );
    if KEEP_TEMPDIRS {
        println!("Tempdir: {:?}", tmp_dir.into_path().to_str());
    }
    r.unwrap();
    Ok(())
}

#[test]
fn test_include_prefixes() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    base_test(&tmp_dir, RsGenMode::Single, |cmd| {
        cmd.arg("--cxx-h-path")
            .arg("foo/")
            .arg("--cxxgen-h-path")
            .arg("bar/")
            .arg("--generate-exact")
            .arg("3")
            .arg("--fix-rs-include-name");
    })?;
    assert_contains(&tmp_dir, "autocxxgen0.h", "foo/cxx.h");
    // Currently we don't test cxxgen-h-path because we build the demo code
    // which doesn't refer to generated cxx header code.
    Ok(())
}

#[test]
fn test_gen_fixed_num() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    let depfile = tmp_dir.path().join("test.d");
    base_test(&tmp_dir, RsGenMode::Single, |cmd| {
        cmd.arg("--generate-exact")
            .arg("2")
            .arg("--fix-rs-include-name")
            .arg("--depfile")
            .arg(depfile);
    })?;
    assert_contentful(&tmp_dir, "gen0.cc");
    assert_contentful(&tmp_dir, "gen0.h");
    assert_not_contentful(&tmp_dir, "gen1.cc");
    assert_contentful(&tmp_dir, "autocxxgen0.h");
    assert_not_contentful(&tmp_dir, "gen1.h");
    assert_not_contentful(&tmp_dir, "autocxxgen1.h");
    assert_contentful(&tmp_dir, "gen0.include.rs");
    assert_contentful(&tmp_dir, "test.d");
    let r = build_from_folder(
        tmp_dir.path(),
        &tmp_dir.path().join("demo/main.rs"),
        vec![tmp_dir.path().join("gen0.include.rs")],
        &["gen0.cc"],
        RsFindMode::AutocxxRsFile,
    );
    if KEEP_TEMPDIRS {
        println!("Tempdir: {:?}", tmp_dir.into_path().to_str());
    }
    r.unwrap();
    Ok(())
}

/// The Rust we write out has to announce itself as machine-written to the tools
/// which look for that, rustfmt among them. rustfmt only scans the first five
/// lines for the `@generated` token and only honours it in a comment, so the
/// marker has to open the file. It also has to name the autocxx which produced
/// the file, so that a stale generated file can be recognised as stale.
#[test]
fn test_gen_rs_has_generated_marker() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    base_test(&tmp_dir, RsGenMode::Single, |_| {})?;
    let rs = std::fs::read_to_string(tmp_dir.path().join("autocxx-ffi-default-gen.rs"))?;
    let first_line = rs.lines().next().unwrap_or_default();
    // The rest of the file is one enormous line of tokens, so quote only enough
    // of it to see what went wrong.
    let opening: String = first_line.chars().take(120).collect();
    assert!(
        first_line.starts_with("// ") && first_line.contains("@generated"),
        "the generated Rust does not open with an @generated comment; it starts: {opening}"
    );
    // autocxx-gen pins autocxx-engine to its own version exactly, so our
    // version is the version of the engine which wrote that file.
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        first_line.contains(version),
        "the @generated marker does not name the autocxx version {version}; it starts: {opening}"
    );
    Ok(())
}

/// Parsing for a `wasm32-*` target must bind the same declarations as parsing
/// for any other, which it did not: clang's WebAssembly driver parses with
/// `-fvisibility=hidden`, bindgen drops every function whose visibility is not
/// `default`, and the result was a POD struct with none of its methods, no
/// function at all and no diagnostic anywhere. See google/autocxx#1508.
///
/// cargo sets `TARGET` for a build script and bindgen reads the triple from
/// there, so setting it is the whole of the repro. Nothing compiles the result,
/// which would need a wasi sysroot; what is being tested is what the triple
/// alone costs the bindings.
#[test]
fn test_wasm_target_keeps_functions() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    let mut files = HashMap::new();
    files.insert("input.h", WASM_H.as_bytes());
    files.insert("main.rs", WASM_RS.as_bytes());
    base_test_ex(
        &tmp_dir,
        RsGenMode::Single,
        |cmd| {
            cmd.env("TARGET", "wasm32-wasip1");
        },
        files,
        vec!["main.rs"],
    )?;
    let rs = std::fs::read_to_string(tmp_dir.path().join("autocxx-ffi-default-gen.rs"))?;
    // A libclang without frontend support for the triple would have failed the
    // command itself, above, rather than reaching either of these.
    assert!(
        rs.contains("fn width"),
        "no method was bound for a wasm32 target; the bindings are {} bytes",
        rs.len()
    );
    assert!(
        rs.contains("fn stretch"),
        "no free function was bound for a wasm32 target; the bindings are {} bytes",
        rs.len()
    );
    Ok(())
}

/// `pretty!()` exists so that a human can read the generated Rust, so the
/// output has to actually be laid out over many lines - and it must not cost
/// the `@generated` marker its place on the first line, nor the run-to-run
/// reproducibility the marker's own test pins. That reproducibility holds
/// for a given resolved prettyplease version; a formatter upgrade may
/// legitimately move whitespace between releases.
#[test]
fn test_gen_rs_pretty() -> Result<(), Box<dyn std::error::Error>> {
    let generate = |pretty: bool| -> Result<String, Box<dyn std::error::Error>> {
        let tmp_dir = tempdir()?;
        let main_rs = if pretty {
            MAIN_RS.replace("safety!(unsafe_ffi)", "safety!(unsafe_ffi)\n    pretty!()")
        } else {
            MAIN_RS.to_string()
        };
        assert_eq!(
            main_rs.contains("pretty!()"),
            pretty,
            "the demo's include_cpp! no longer contains the line this test \
             rewrites, so it is not testing what it thinks it is"
        );
        let mut files = HashMap::new();
        files.insert("input.h", INPUT_H.as_bytes());
        files.insert("main.rs", main_rs.as_bytes());
        base_test_ex(&tmp_dir, RsGenMode::Single, |_| {}, files, vec!["main.rs"])?;
        Ok(std::fs::read_to_string(
            tmp_dir.path().join("autocxx-ffi-default-gen.rs"),
        )?)
    };

    let terse = generate(false)?;
    let pretty = generate(true)?;

    // Without it, the whole mod is one line, under the marker.
    assert!(
        terse.lines().count() <= 2,
        "the bindings already span {} lines without pretty!(), so this test can \
         no longer tell whether pretty!() did anything",
        terse.lines().count()
    );
    assert!(
        pretty.lines().count() > 50,
        "pretty!() did not lay the bindings out over multiple lines; it produced {} lines",
        pretty.lines().count()
    );
    let first_line = pretty.lines().next().unwrap_or_default();
    assert!(
        first_line.starts_with("// ") && first_line.contains("@generated"),
        "pretty!() displaced the @generated marker; the file starts: {first_line}"
    );
    assert!(
        pretty == generate(true)?,
        "pretty!() output differs between two runs over the same input"
    );
    Ok(())
}

/// The marker must not smuggle a timestamp (or anything else which varies run
/// to run) into the output: content-addressed build systems rebuild everything
/// downstream of a generated file whose bytes changed, even if only a comment
/// moved.
#[test]
fn test_gen_rs_is_reproducible() -> Result<(), Box<dyn std::error::Error>> {
    let read_generated_rs = || -> Result<String, Box<dyn std::error::Error>> {
        let tmp_dir = tempdir()?;
        base_test(&tmp_dir, RsGenMode::Single, |_| {})?;
        Ok(std::fs::read_to_string(
            tmp_dir.path().join("autocxx-ffi-default-gen.rs"),
        )?)
    };
    assert_eq!(read_generated_rs()?, read_generated_rs()?);
    Ok(())
}

#[test]
fn test_gen_preprocess() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    let prepro_path = tmp_dir.path().join("preprocessed.h");
    base_test(&tmp_dir, RsGenMode::Single, |cmd| {
        cmd.env("AUTOCXX_PREPROCESS", prepro_path.to_str().unwrap());
    })?;
    assert_contentful(&tmp_dir, "preprocessed.h");
    // Check that a random thing from one of the headers in
    // `ALL_KNOWN_SYSTEM_HEADERS` is included.
    assert!(std::fs::read_to_string(prepro_path)?.contains("integer_sequence"));
    Ok(())
}

#[test]
fn test_gen_repro() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    let repro_path = tmp_dir.path().join("repro.json");
    base_test(&tmp_dir, RsGenMode::Single, |cmd| {
        cmd.env("AUTOCXX_REPRO_CASE", repro_path.to_str().unwrap());
    })?;
    assert_contentful(&tmp_dir, "repro.json");
    // Check that a random thing from one of the headers in
    // `ALL_KNOWN_SYSTEM_HEADERS` is included.
    assert!(std::fs::read_to_string(repro_path)?.contains("integer_sequence"));
    Ok(())
}

/// Runs `autocxx-gen` over a header and a Rust file from `tests/data`, and
/// answers what it printed and whether it succeeded.
fn run_gen_over_fixture(
    header_name: &str,
    header: &str,
    rust: &str,
) -> Result<(bool, String), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    let demo_code_dir = tmp_dir.path().join("demo");
    std::fs::create_dir(&demo_code_dir).unwrap();
    write_to_file(&demo_code_dir, header_name, header.as_bytes());
    write_to_file(&demo_code_dir, "main.rs", rust.as_bytes());
    let mut cmd = Command::cargo_bin("autocxx-gen")?;
    cmd.arg("--inc")
        .arg(demo_code_dir.to_str().unwrap())
        .arg("--outdir")
        .arg(tmp_dir.path().to_str().unwrap())
        .arg("--gen-cpp")
        .arg("--gen-rs-include")
        .arg(demo_code_dir.join("main.rs"));
    let output = cmd.output()?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    eprintln!("Cmd stderr: {stderr}");
    Ok((output.status.success(), stderr))
}

/// A C++ class called `String` used to collide with cxx's reserved vocabulary
/// and get the whole bridge refused - the bug reported upstream as
/// google/autocxx#1371. The bridge now renames it, so this is an end-to-end
/// check that the command generates bindings for such a class at all.
#[test]
fn test_class_named_after_cxx_vocabulary_generates() -> Result<(), Box<dyn std::error::Error>> {
    let (success, stderr) =
        run_gen_over_fixture("cxx_vocabulary.h", CXX_VOCABULARY_H, CXX_VOCABULARY_RS)?;
    assert!(success, "autocxx-gen failed: {stderr}");
    Ok(())
}

/// cxx can still refuse a bridge we generate: the `extern "Rust"` half is
/// named by the user's own Rust type, which we are not free to rename, and
/// cxx applies `check_reserved_name` to it. We must say so rather than
/// panicking with a backtrace.
#[test]
fn test_reports_cxx_rejection_without_panicking() -> Result<(), Box<dyn std::error::Error>> {
    let (success, stderr) = run_gen_over_fixture(
        "reserved_rust_type.h",
        RESERVED_RUST_TYPE_H,
        RESERVED_RUST_TYPE_RS,
    )?;
    assert!(!success, "autocxx-gen unexpectedly succeeded");
    assert!(
        !stderr.contains("panicked at"),
        "autocxx-gen panicked instead of reporting an error"
    );
    assert!(
        stderr.contains("cxx couldn't handle our generated bindings"),
        "autocxx-gen didn't explain why it failed"
    );
    Ok(())
}

#[test]
fn test_reports_bad_generate_exact_without_panicking() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    let demo_code_dir = tmp_dir.path().join("demo");
    std::fs::create_dir(&demo_code_dir).unwrap();
    write_to_file(&demo_code_dir, "input.h", b"inline void foo() {}\n");
    let mut cmd = Command::cargo_bin("autocxx-gen")?;
    cmd.arg("--inc")
        .arg(demo_code_dir.to_str().unwrap())
        .arg("--outdir")
        .arg(tmp_dir.path().to_str().unwrap())
        .arg("--gen-cpp")
        .arg("--generate-exact")
        .arg("nope")
        .arg(demo_code_dir.join("input.h"));
    let output = cmd.output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("Cmd stderr: {stderr}");
    assert!(
        !output.status.success(),
        "autocxx-gen unexpectedly succeeded"
    );
    assert!(
        !stderr.contains("panicked at"),
        "autocxx-gen panicked instead of reporting an error"
    );
    assert!(
        stderr.contains("--generate-exact requires a number"),
        "autocxx-gen didn't explain why it failed"
    );
    Ok(())
}

fn write_to_file(dir: &Path, filename: &str, content: &[u8]) {
    let path = dir.join(filename);
    let mut f = File::create(path).expect("Unable to create file");
    f.write_all(content).expect("Unable to write file");
}

fn assert_contentful(outdir: &TempDir, fname: &str) {
    let p = outdir.path().join(fname);
    if !p.exists() {
        panic!("File {} didn't exist", p.to_string_lossy());
    }
    assert!(
        p.metadata().unwrap().len() > BLANK.len().try_into().unwrap(),
        "File {fname} is empty"
    );
}

fn assert_not_contentful(outdir: &TempDir, fname: &str) {
    let p = outdir.path().join(fname);
    if !p.exists() {
        panic!("File {} didn't exist", p.to_string_lossy());
    }
    assert!(
        p.metadata().unwrap().len() <= BLANK.len().try_into().unwrap(),
        "File {} is not empty; it contains {}",
        fname,
        std::fs::read_to_string(&p).unwrap_or_default()
    );
}

fn assert_contains(outdir: &TempDir, fname: &str, pattern: &str) {
    let p = outdir.path().join(fname);
    let content = std::fs::read_to_string(p).expect(fname);
    eprintln!("content = {content}");
    assert!(content.contains(pattern));
}

/// The `.rs` file autocxx-gen parsed is a dependency of everything it wrote,
/// alongside the headers the preprocessor opened. A depfile which names only
/// the headers describes a rule which an edit to `include_cpp!` leaves stale
/// while the build system believes it is up to date.
#[test]
fn test_depfile_names_the_rust_input() -> Result<(), Box<dyn std::error::Error>> {
    let tmp_dir = tempdir()?;
    let depfile = tmp_dir.path().join("test.d");
    base_test(&tmp_dir, RsGenMode::Single, |cmd| {
        cmd.arg("--depfile").arg(&depfile);
    })?;
    let contents = std::fs::read_to_string(&depfile)?;
    assert!(
        contents.contains("demo/main.rs"),
        "the parsed .rs input is not a dependency; depfile reads:\n{contents}"
    );
    assert!(
        contents.contains("demo/input.h"),
        "the parsed header is not a dependency; depfile reads:\n{contents}"
    );
    Ok(())
}
