// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use assert_cmd::cargo::CommandCargoExt;
use assert_cmd::Command;
use proc_macro2::Span;
use quote::quote;
use std::{
    borrow::Cow,
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
};
use syn::Token;
use tempfile::tempdir;

static INPUT_H: &str = indoc::indoc! {"
    inline int DoMath(int a) {
        return a * 3;
    }

    struct First {
        First() {}
        int foo;
    };

    struct Second {
        Second(const First& a) {}
        int bar;
    };

    struct WithMethods {
        First get_first();
        Second get_second();
        int a;
    };
"};

enum Input {
    Header(String),
    ReproCase(PathBuf),
}

#[test]
fn test_reduce_direct_header() -> Result<(), Box<dyn std::error::Error>> {
    do_reduce(
        "reduce_direct_header",
        |header, _| Ok(Input::Header(header.into())),
        false,
    )
}

#[test]
fn test_reduce_direct_repro_case() -> Result<(), Box<dyn std::error::Error>> {
    do_reduce(
        "reduce_direct_repro_case",
        |header, demo_code_dir| {
            let config = format!(
                "#include \"{header}\" generate_all!() block!(\"First\")
            safety!(unsafe_ffi)"
            );
            let json = serde_json::json!({
                "header": INPUT_H,
                "config": config
            });
            let repropath = demo_code_dir.join("repro.json");
            let f = File::create(&repropath)?;
            serde_json::to_writer(f, &json)?;
            Ok(Input::ReproCase(repropath))
        },
        false,
    )
}

#[test]
#[ignore] // takes absolutely ages but you can run using cargo test -- --ignored
fn test_reduce_preprocessed_repro_case() -> Result<(), Box<dyn std::error::Error>> {
    do_reduce(
        "reduce_preprocessed_repro_case",
        |header, demo_code_dir| {
            write_minimal_rs_code(header, demo_code_dir);
            let repro = demo_code_dir.join("autocxx-repro.json");
            let mut cmd = Command::cargo_bin("autocxx-gen")?;
            cmd.arg("--inc")
                .arg(demo_code_dir.to_str().unwrap())
                .arg(demo_code_dir.join("main.rs"))
                .env("AUTOCXX_REPRO_CASE", repro.to_str().unwrap())
                .arg("--outdir")
                .arg(demo_code_dir.to_str().unwrap())
                .arg("--gen-cpp")
                .arg("--suppress-system-headers")
                .assert()
                .success();
            Ok(Input::ReproCase(repro))
        },
        false,
    )
}

#[test]
#[ignore] // takes absolutely ages but you can run using cargo test -- --ignored
fn test_reduce_preprocessed() -> Result<(), Box<dyn std::error::Error>> {
    do_reduce(
        "reduce_preprocessed",
        |header, demo_code_dir| {
            write_minimal_rs_code(header, demo_code_dir);
            let prepro = demo_code_dir.join("autocxx-preprocessed.h");
            let mut cmd = Command::cargo_bin("autocxx-gen")?;
            cmd.arg("--inc")
                .arg(demo_code_dir.to_str().unwrap())
                .arg(demo_code_dir.join("main.rs"))
                .env("AUTOCXX_PREPROCESS", prepro.to_str().unwrap())
                .arg("--outdir")
                .arg(demo_code_dir.to_str().unwrap())
                .arg("--gen-cpp")
                .arg("--suppress-system-headers")
                .assert()
                .success();
            Ok(Input::Header("autocxx-preprocessed.h".into()))
        },
        false,
    )
}

#[test]
#[ignore] // takes absolutely ages but you can run using cargo test -- --ignored
fn test_reduce_preprocessed_include_cxx_h() -> Result<(), Box<dyn std::error::Error>> {
    do_reduce(
        "reduce_preprocessed_include_cxx_h",
        |header, demo_code_dir| {
            write_minimal_rs_code(header, demo_code_dir);
            let prepro = demo_code_dir.join("autocxx-preprocessed.h");
            let mut cmd = Command::cargo_bin("autocxx-gen")?;
            cmd.arg("--inc")
                .arg(demo_code_dir.to_str().unwrap())
                .arg(demo_code_dir.join("main.rs"))
                .env("AUTOCXX_PREPROCESS", prepro.to_str().unwrap())
                .arg("--outdir")
                .arg(demo_code_dir.to_str().unwrap())
                .arg("--gen-cpp")
                .arg("--suppress-system-headers")
                .assert()
                .success();
            Ok(Input::Header("autocxx-preprocessed.h".into()))
        },
        true,
    )
}

fn write_minimal_rs_code(header: &str, demo_code_dir: &Path) {
    let hexathorpe = Token![#](Span::call_site());
    write_to_file(
        demo_code_dir,
        "main.rs",
        quote! {
            autocxx::include_cpp! {
                #hexathorpe include #header
                generate!("WithMethods")
                block!("First")
                safety!(unsafe_ffi)
            }
        }
        .to_string()
        .as_bytes(),
    );
}

/// Runs a reduction end to end. `label` names the test, and prefixes every
/// line the reduction prints: these tests run in parallel and stream their
/// output as it arrives, so without it nobody could tell whose it was.
fn do_reduce<F>(
    label: &str,
    get_repro_case: F,
    include_cxx_h: bool,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&str, &Path) -> Result<Input, Box<dyn std::error::Error>>,
{
    // Without creduce there is nothing to test, so we skip rather than fail.
    // Say so loudly: a silent `Ok(())` here reports as a passing test that
    // actually ran no assertions at all, which is worse than no test.
    if creduce_is_broken() {
        eprintln!(
            "SKIPPED: creduce is missing or broken, so this test ran no assertions. \
             Install creduce to get any coverage from it."
        );
        return Ok(());
    }
    let tmp_dir = tempdir()?;
    let demo_code_dir = tmp_dir.path().join("demo");
    std::fs::create_dir(&demo_code_dir).unwrap();
    let input_header = if include_cxx_h {
        Cow::Owned(format!("#include \"cxx.h\"\n{INPUT_H}"))
    } else {
        Cow::Borrowed(INPUT_H)
    };
    write_to_file(&demo_code_dir, "input.h", input_header.as_bytes());
    write_to_file(&demo_code_dir, "cxx.h", cxx_gen::HEADER.as_bytes());
    let output_path = tmp_dir.path().join("min.h");
    let repro_case = get_repro_case("input.h", &demo_code_dir)?;
    let mut cmd = std::process::Command::cargo_bin("autocxx-reduce")?;
    let cmd = cmd
        .arg("-o")
        .arg(output_path.to_str().unwrap())
        .arg("-p")
        .arg("type marked as blocked")
        .arg("-k");
    match repro_case {
        Input::Header(header_name) => {
            cmd.arg("file")
                .arg("--inc")
                .arg(demo_code_dir.to_str().unwrap())
                .arg("--header")
                .arg(header_name)
                .arg("-d")
                .arg("generate!(\"WithMethods\")")
                .arg("-d")
                .arg("block!(\"First\")");
        }
        Input::ReproCase(repro_case) => {
            cmd.arg("repro").arg("-r").arg(repro_case);
        }
    }
    eprintln!("Running {cmd:?}");
    // A reduction takes minutes to hours, and creduce reports its progress as
    // it goes, so watch it happen rather than collecting the lot to print
    // afterwards.
    let status = run_streaming_output(label, cmd)?;
    if !status.success() {
        panic!("autocxx-reduce returned non-zero result code");
    }
    let minimized = std::fs::read_to_string(output_path)?;
    assert!(minimized.contains("First"));
    assert!(!minimized.contains("DoMath"));
    Ok(())
}

/// Runs `cmd`, copying everything it prints to this process's own stderr as it
/// arrives, and returns how it exited.
///
/// `Command::output` collects both streams and hands them over once the child
/// has finished, which for a reduction is minutes to hours of nothing at all -
/// and under `cargo test` even that final dump goes into libtest's per-test
/// capture and is shown only if the test fails, so a run that is working and a
/// run that has hung look exactly alike. creduce reports its progress as it
/// goes; this lets that progress be seen.
///
/// Written to `std::io::stderr` rather than through `eprintln!`, because
/// libtest's capture hooks the macros and the whole point is to be visible
/// while the test is still running. stderr for both of the child's streams,
/// rather than stdout for its stdout, so that nothing lands in the middle of
/// libtest's own reporting - which is on stdout, and which a machine may be
/// reading.
fn run_streaming_output(
    label: &str,
    cmd: &mut std::process::Command,
) -> std::io::Result<ExitStatus> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Both streams at once, in threads: reading one to the end before starting
    // on the other would deadlock as soon as the child filled the pipe we
    // weren't reading.
    let mut forwarders = Vec::new();
    let piped = "we asked for this stream to be piped";
    let stdout = child.stdout.take().expect(piped);
    let stderr = child.stderr.take().expect(piped);
    for (stream, source) in [
        ("out", Box::new(stdout) as Box<dyn Read + Send>),
        ("err", Box::new(stderr) as Box<dyn Read + Send>),
    ] {
        let label = format!("{label} {stream}");
        forwarders.push(std::thread::spawn(move || {
            forward_lines(source, &label, std::io::stderr())
        }));
    }
    let status = child.wait()?;
    for forwarder in forwarders {
        // The pipes are closed now the child has gone, so each thread has
        // either finished or is about to. Whatever it hit is worth saying, but
        // it is not the outcome of the test.
        match forwarder.join() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = writeln!(std::io::stderr(), "warning: {label}: {err}");
            }
            Err(_) => {
                let _ = writeln!(std::io::stderr(), "warning: {label}: a forwarder panicked");
            }
        }
    }
    Ok(status)
}

/// Copies `source` to `sink` a line at a time, prefixing each with `label` and
/// flushing as it goes, so that a reader sees each line as soon as it is
/// written rather than whenever a buffer happens to fill.
fn forward_lines(
    source: impl Read,
    label: &str,
    mut sink: impl Write,
) -> Result<(), std::io::Error> {
    for line in BufReader::new(source).lines() {
        writeln!(sink, "[{label}] {}", line?)?;
        sink.flush()?;
    }
    Ok(())
}

/// A `Write` which records what it had been given at each flush, so a test can
/// see not just what came out but when.
#[derive(Default)]
struct FlushRecorder {
    written: Vec<u8>,
    at_each_flush: Vec<String>,
}

impl Write for FlushRecorder {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.at_each_flush
            .push(String::from_utf8_lossy(&self.written).into_owned());
        Ok(())
    }
}

/// The plumbing around [`forward_lines`], on a child that finishes at once:
/// it runs, what it prints comes back through the pipes without deadlocking,
/// and its exit status is reported. Needs no creduce, so unlike the
/// reductions themselves this runs everywhere.
#[test]
fn test_run_streaming_output_runs_the_child() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = std::process::Command::cargo_bin("autocxx-reduce")?;
    cmd.arg("--version");
    assert!(run_streaming_output("version", &mut cmd)?.success());
    Ok(())
}

/// The property the reduce tests need: a line reaches the sink, and is
/// flushed, before the source has been read to the end. Without it there is
/// nothing to watch during a reduction, which is the whole reason for
/// streaming rather than collecting.
#[test]
fn test_forward_lines_delivers_each_line_as_it_arrives() {
    let mut recorder = FlushRecorder::default();
    forward_lines(&b"first\nsecond\n"[..], "x", &mut recorder).unwrap();
    assert_eq!(
        recorder.at_each_flush,
        vec!["[x] first\n", "[x] first\n[x] second\n"]
    );
}

/// A last line with no newline of its own is still delivered - creduce's
/// progress reporting need not end tidily for us to show it.
#[test]
fn test_forward_lines_delivers_an_unterminated_last_line() {
    let mut recorder = FlushRecorder::default();
    forward_lines(&b"halfway"[..], "x", &mut recorder).unwrap();
    assert_eq!(recorder.at_each_flush, vec!["[x] halfway\n"]);
}

fn write_to_file(dir: &Path, filename: &str, content: &[u8]) {
    let path = dir.join(filename);
    let mut f = File::create(path).expect("Unable to create file");
    f.write_all(content).expect("Unable to write file");
}

fn creduce_is_broken() -> bool {
    // On some machines, creduce immediately segfaults
    Command::new("creduce").arg("--version").ok().is_err()
}
