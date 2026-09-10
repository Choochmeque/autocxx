// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![forbid(unsafe_code)]

mod depfile;

use autocxx_engine::{
    generate_rs_archive, generate_rs_single, get_cxx_header_bytes, parse_file,
    AutocxxgenHeaderNamer, CxxgenHeaderNamer, RebuildDependencyRecorder,
};
use clap::{crate_authors, crate_version, Arg, ArgGroup, Command};
use depfile::Depfile;
use indexmap::IndexMap;
use miette::IntoDiagnostic;
use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::{cell::Cell, fs::File, path::Path};

pub(crate) static BLANK: &str = "// Blank autocxx placeholder";

static LONG_HELP: &str = "
Command line utility to expand the Rust 'autocxx' include_cpp! directive.

This tool can generate both the C++ and Rust side binding code for
a Rust file containing an include_cpp! directive.

If you're using cargo, don't use this: use autocxx_build instead,
which is much easier to include in build.rs build scripts. You'd likely
use this tool only if you're using some non-Cargo build system. If
that's you, read on.

This tool has three modes: generate the C++; or generate
a Rust file which can be included by the autocxx_macro; or generate an archive
containing multiple Rust files to be expanded by different autocxx macros.
You may specify multiple modes, or of course, invoke the tool multiple times.

In any mode, you'll need to pass the source Rust file name and the C++
include path. You may pass multiple Rust files, each of which may contain
multiple include_cpp! or cxx::bridge macros.

There are three basic ways to use this tool, depending on the flexibility
of your build system.

Does your build system require fixed output filenames, or can it enumerate
whatever files are generated?

If it's flexible, then use
  --gen-rs-include --gen-cpp
An arbitrary number of .h, .cc and .rs files will be generated, depending
on how many cxx::bridge and include_cpp macros are encountered and their contents.
When building the rust code, simply ensure that AUTOCXX_RS or OUT_DIR is set to
teach rustc where to find these .rs files.

If your build system needs to be told exactly what C++ files are generated,
additionally use --generate-exact <N> You are then guaranteed to get
exactly 'n' files as follows:
  gen<n>.h
  autocxxgen<n>.h
  gen<n>.cc
Some of them may be blank. If the tool finds too many include_cpp or cxx::bridge
macros to fit within that allowance, the build will fail.

If your build system additionally requires that Rust files have fixed
filenames, then you should use
  --gen-rs-archive
instead of
  --gen-rs-include
and you will need to give AUTOCXX_RS_JSON_ARCHIVE when building the Rust code.
The output filename is named gen.rs.json. AUTOCXX_RS_JSON_ARCHIVE should be set
to the path to gen.rs.json. It may optionally have multiple paths separated the
way as the PATH environment variable for the current platform, see
[`std::env::split_paths`] for details. The first path which is successfully
opened will be used.

This teaches rustc (and the autocxx macro) that all the different Rust bindings
for multiple different autocxx macros have been archived into this single file.
";

fn main() -> miette::Result<()> {
    let matches = Command::new("autocxx-gen")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Generates bindings files from Rust files that contain include_cpp! macros")
        .long_about(LONG_HELP)
        .arg(
            Arg::new("INPUT")
                .help("Sets the input .rs files to use")
                .required(true)
                .multiple_occurrences(true)
        )
        .arg(
            Arg::new("outdir")
                .short('o')
                .long("outdir")
                .allow_invalid_utf8(true)
                .value_name("PATH")
                .help("output directory path")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::new("inc")
                .short('I')
                .long("inc")
                .multiple_occurrences(true)
                .number_of_values(1)
                .value_name("INCLUDE DIRS")
                .help("include path")
                .takes_value(true),
        )
        .arg(
            Arg::new("cpp-extension")
                .long("cpp-extension")
                .value_name("EXTENSION")
                .default_value("cc")
                .help("C++ filename extension")
                .takes_value(true),
        )
        .arg(
            Arg::new("gen-cpp")
                .long("gen-cpp")
                .help("whether to generate C++ implementation and header files")
        )
        .arg(
            Arg::new("gen-rs-include")
                .long("gen-rs-include")
                .help("whether to generate Rust files for inclusion using autocxx_macro")
        )
        .arg(
            Arg::new("gen-rs-archive")
                .long("gen-rs-archive")
                .help("whether to generate an archive of multiple sets of Rust bindings for use by autocxx_macro (suffix will be .rs.json)")
        )
        .group(ArgGroup::new("mode")
            .required(true)
            .multiple(true)
            .arg("gen-cpp")
            .arg("gen-rs-include")
            .arg("gen-rs-archive")
        )
        .arg(
            Arg::new("generate-exact")
                .long("generate-exact")
                .value_name("NUM")
                .help("assume and ensure there are exactly NUM bridge blocks in the file. Only applies for --gen-cpp or --gen-rs-include")
                .takes_value(true),
        )
        .arg(
            Arg::new("fix-rs-include-name")
                .long("fix-rs-include-name")
                .help("Make the name of the .rs file predictable (suffix will be .include.rs). You must set AUTOCXX_RS_FILE during Rust build time to educate autocxx_macro about your choice.")
                .requires("gen-rs-include")
        )
        .arg(
            Arg::new("auto-allowlist")
                .long("auto-allowlist")
                .help("Dynamically construct allowlist from real uses of APIs.")
        )
        .arg(
            Arg::new("suppress-system-headers")
                .long("suppress-system-headers")
                .help("Do not refer to any system headers from generated code. May be useful for minimization.")
        )
        .arg(
            Arg::new("cxx-impl-annotations")
                .long("cxx-impl-annotations")
                .value_name("ANNOTATION")
                .help("prefix for symbols to be exported from C++ bindings, e.g. __attribute__ ((visibility (\"default\")))")
                .takes_value(true),
        )
        .arg(
            Arg::new("generate-cxx-h")
                .long("generate-cxx-h")
                .help("whether to generate cxx.h header file. If you already knew where to find cxx.h, consider using --cxx-h-path")
        )
        .arg(
            Arg::new("cxx-h-path")
                .long("cxx-h-path")
                .value_name("PREFIX")
                .help("prefix for path to cxx.h (from the cxx crate) within #include statements. Must end in /")
                .takes_value(true),
        )
        .arg(
            Arg::new("cxxgen-h-path")
                .long("cxxgen-h-path")
                .value_name("PREFIX")
                .help("prefix for path to cxxgen.h (which we generate into the output directory) within #include statements. Must end in /")
                .takes_value(true),
        )
        .arg(
            Arg::new("depfile")
                .long("depfile")
                .value_name("DEPFILE")
                .help("A .d file to write")
                .takes_value(true),
        )
        .arg(
            Arg::new("clang-args")
                .last(true)
                .multiple_occurrences(true)
                .help("Extra arguments to pass to Clang"),
        )
        .get_matches();

    env_logger::builder().init();
    let incs = matches
        .values_of("inc")
        .unwrap_or_default()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let extra_clang_args: Vec<_> = matches
        .values_of("clang-args")
        .unwrap_or_default()
        .collect();
    let suppress_system_headers = matches.is_present("suppress-system-headers");
    let desired_number = matches
        .value_of("generate-exact")
        .map(|s| {
            s.parse::<usize>().map_err(|err| {
                miette::Report::msg(format!("--generate-exact requires a number: {err}"))
            })
        })
        .transpose()?;
    let autocxxgen_header_counter = Cell::new(0);
    let autocxxgen_header_namer = if desired_number.is_some() {
        AutocxxgenHeaderNamer(Box::new(|_| {
            let r = name_autocxxgen_h(autocxxgen_header_counter.get());
            autocxxgen_header_counter.set(autocxxgen_header_counter.get() + 1);
            r
        }))
    } else {
        Default::default()
    };
    let cxxgen_header_counter = Cell::new(0);
    let cxxgen_header_namer = if desired_number.is_some() {
        CxxgenHeaderNamer(Box::new(|| {
            let r = name_cxxgen_h(cxxgen_header_counter.get());
            cxxgen_header_counter.set(cxxgen_header_counter.get() + 1);
            r
        }))
    } else {
        Default::default()
    };
    let cpp_codegen_options = autocxx_engine::CppCodegenOptions {
        suppress_system_headers,
        cxx_impl_annotations: get_option_string("cxx-impl-annotations", &matches),
        path_to_cxx_h: get_option_string("cxx-h-path", &matches),
        path_to_cxxgen_h: get_option_string("cxxgen-h-path", &matches),
        autocxxgen_header_namer,
        cxxgen_header_namer,
    };
    let codegen_options = autocxx_engine::CodegenOptions {
        cpp_codegen_options,
        ..Default::default()
    };
    let depfile = match matches.value_of("depfile") {
        None => None,
        Some(depfile_path) => {
            let depfile_path = PathBuf::from(depfile_path);
            Some(Rc::new(RefCell::new(
                Depfile::new(&depfile_path).into_diagnostic()?,
            )))
        }
    };
    let auto_allowlist = matches.is_present("auto-allowlist");

    let mut parsed_files = Vec::new();
    for input in matches.values_of("INPUT").expect("No INPUT was provided") {
        // Parse all the .rs files we're asked to process, first.
        // Spot any fundamental parsing or command line problems before we start
        // to do the complex processing.
        let parsed_file = parse_file(input, auto_allowlist)?;
        // The dependency recorder below hears only about headers, from the
        // preprocessor; these are the other half of what codegen read, and a
        // depfile which lists one and not the other describes a rule that can
        // go stale without being rebuilt.
        if let Some(depfile) = &depfile {
            depfile.borrow_mut().add_dependency(&PathBuf::from(input));
        }
        // Kept alongside the parsed file so a name two inputs both claim can be
        // reported against the inputs which claimed it.
        parsed_files.push((input.to_string(), parsed_file));
    }

    for (_, parsed_file) in parsed_files.iter_mut() {
        // Now actually handle all the include_cpp directives we found,
        // which is the complex bit where we interpret all the C+.
        let dep_recorder: Option<Box<dyn RebuildDependencyRecorder>> = depfile
            .as_ref()
            .map(|rc| get_dependency_recorder(rc.clone()));
        parsed_file.resolve_all(
            incs.clone(),
            &extra_clang_args,
            dep_recorder,
            &codegen_options,
        )?;
    }

    // Finally start to write the C++ and Rust out.
    let outdir: PathBuf = matches.value_of_os("outdir").unwrap().into();

    if !outdir.exists() {
        use miette::WrapErr as _;
        std::fs::create_dir_all(&outdir)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to create `outdir` '{}'", outdir.display()))?;
    }

    let mut writer = FileWriter {
        depfile: &depfile,
        outdir: &outdir,
        pending: IndexMap::new(),
    };
    if matches.is_present("gen-cpp") {
        let cpp = matches.value_of("cpp-extension").unwrap();
        let name_cc_file = |counter| format!("gen{counter}.{cpp}");
        let mut counter = 0usize;
        for (input, include_cxx) in parsed_files.iter().flat_map(|(input, file)| {
            file.get_cpp_buildables()
                .map(move |buildable| (input.as_str(), buildable))
        }) {
            // This is where cxx rejects the bridge we built for it. Some of
            // those rejections are our user's C++ tripping over cxx's reserved
            // vocabulary - a C++ class called `String`, for instance - which
            // autocxx still can't handle: https://github.com/google/autocxx/issues/1371.
            // Until it can, say so rather than panicking at the user.
            let generations = include_cxx
                .generate_h_and_cxx(&codegen_options.cpp_codegen_options)
                .map_err(report_cxx_rejection)?;
            for pair in generations.0 {
                let cppname = name_cc_file(counter);
                writer.add_output(
                    cppname,
                    pair.implementation.unwrap_or_default(),
                    Some(input),
                )?;
                writer.add_output(pair.header_name, pair.header, Some(input))?;
                counter += 1;
            }
        }
        drop(codegen_options);
        // Add placeholders to ensure we always make exactly 'n' of each file type.
        writer.add_placeholders(counter, desired_number, name_cc_file)?;
        writer.add_placeholders(
            cxxgen_header_counter.into_inner(),
            desired_number,
            name_cxxgen_h,
        )?;
        writer.add_placeholders(
            autocxxgen_header_counter.into_inner(),
            desired_number,
            name_autocxxgen_h,
        )?;
    }

    if matches.is_present("generate-cxx-h") {
        writer.add_output(
            "cxx.h".to_string(),
            get_cxx_header_bytes(suppress_system_headers),
            None,
        )?;
    }

    if matches.is_present("gen-rs-include") {
        if !matches.is_present("fix-rs-include-name") && desired_number.is_some() {
            return Err(miette::Report::msg(
                "gen-rs-include and generate-exact requires fix-rs-include-name.",
            ));
        }
        let mut counter = 0usize;
        let rust_buildables = parsed_files.iter().flat_map(|(input, parsed_file)| {
            parsed_file
                .get_rs_outputs()
                .map(move |output| (input.as_str(), output))
        });
        for (input, include_cxx) in rust_buildables {
            let rs_code = generate_rs_single(include_cxx);
            let fname = if matches.is_present("fix-rs-include-name") {
                name_include_rs(counter)
            } else {
                rs_code.filename
            };
            writer.add_output(fname, rs_code.code.into_bytes(), Some(input))?;
            counter += 1;
        }
        writer.add_placeholders(counter, desired_number, name_include_rs)?;
    }
    if matches.is_present("gen-rs-archive") {
        let rust_buildables = parsed_files
            .iter()
            .flat_map(|(_, parsed_file)| parsed_file.get_rs_outputs());
        let json = generate_rs_archive(rust_buildables).into_diagnostic()?;
        writer.add_output("gen.rs.json".into(), json.into_bytes(), None)?;
    }
    writer.write_all()?;
    if let Some(depfile) = depfile {
        depfile.borrow_mut().write().into_diagnostic()?;
    }
    Ok(())
}

/// Explain a refusal from cxx, using the same wording as the equivalent
/// failure in `autocxx_build`.
fn report_cxx_rejection(err: impl std::fmt::Display) -> miette::Report {
    miette::Report::msg(format!(
        "cxx couldn't handle our generated bindings - could be a bug in autocxx: {err}"
    ))
}

fn name_autocxxgen_h(counter: usize) -> String {
    format!("autocxxgen{counter}.h")
}

fn name_cxxgen_h(counter: usize) -> String {
    format!("gen{counter}.h")
}

fn name_include_rs(counter: usize) -> String {
    format!("gen{counter}.include.rs")
}

fn get_dependency_recorder(depfile: Rc<RefCell<Depfile>>) -> Box<dyn RebuildDependencyRecorder> {
    Box::new(RecordIntoDepfile(depfile))
}

fn get_option_string(option: &str, matches: &clap::ArgMatches) -> Option<String> {
    let cxx_impl_annotations = matches.value_of(option).map(|s| s.to_string());
    cxx_impl_annotations
}

/// A file we are going to write, held until every output name is known.
struct PendingOutput {
    content: Vec<u8>,
    /// The input `.rs` this came from, where one input owns it.
    origin: Option<String>,
}

/// Collects the whole output set, then writes it.
///
/// None of these files reaches the filesystem until every name is registered,
/// because two inputs claiming one name have to be refused before either is
/// written:
/// `write_all` leaves a file alone whose contents already match, so on a
/// rebuild the first claimant would be skipped and the second would take its
/// filename with nothing to report.
struct FileWriter<'a> {
    depfile: &'a Option<Rc<RefCell<Depfile>>>,
    outdir: &'a Path,
    pending: IndexMap<String, PendingOutput>,
}

impl FileWriter<'_> {
    fn add_placeholders<F: FnOnce(usize) -> String + Copy>(
        &mut self,
        mut counter: usize,
        desired_number: Option<usize>,
        filename: F,
    ) -> miette::Result<()> {
        if let Some(desired_number) = desired_number {
            if counter > desired_number {
                return Err(miette::Report::msg(format!("{counter} files were generated. Increase the value passed to --generate-exact or reduce the number of include_cpp! sections.")));
            }
            while counter < desired_number {
                let fname = filename(counter);
                self.add_output(fname, BLANK.as_bytes().to_vec(), None)?;
                counter += 1;
            }
        }
        Ok(())
    }

    fn add_output(
        &mut self,
        filename: String,
        content: Vec<u8>,
        origin: Option<&str>,
    ) -> miette::Result<()> {
        if let Some(existing) = self.pending.get(&filename) {
            // Identical bytes are not a conflict: whichever of the two claims
            // the name, the file says the same thing.
            if existing.content == content {
                return Ok(());
            }
            return Err(miette::Report::msg(match (&existing.origin, origin) {
                (Some(first), Some(second)) => format!("autocxx_gen would write two files entitled '{filename}' which would have conflicting contents, one from {first} and one from {second}. Give each include_cpp! its own name!, or use --generate-exact."),
                _ => format!("autocxx_gen would write two files entitled '{filename}' which would have conflicting contents. Consider using --generate-exact."),
            }));
        }
        self.pending.insert(
            filename,
            PendingOutput {
                content,
                origin: origin.map(str::to_owned),
            },
        );
        Ok(())
    }

    fn write_all(self) -> miette::Result<()> {
        // A second registry, of the files the names reach rather than of the
        // names: a filesystem which ignores case, or a symlink, makes one file
        // out of two names, and the second output would take the first's place
        // exactly as a repeated name would. Only the filesystem knows, so this
        // one cannot be settled before writing starts - but it is settled
        // before the file the two names share is overwritten.
        let mut published: IndexMap<PathBuf, String> = IndexMap::new();
        for (filename, output) in self.pending {
            let path = self.outdir.join(&filename);
            if let Some(depfile) = self.depfile {
                depfile.borrow_mut().add_output(&path);
            }
            let destination = resolve_destination(&path)?;
            if let Some(first) = published.get(&destination) {
                return Err(miette::Report::msg(format!(
                    "autocxx_gen would write both '{first}' and '{filename}' to one file, '{}'. Give each include_cpp! its own name!, or use --generate-exact.",
                    destination.display()
                )));
            }
            published.insert(destination.clone(), filename);
            {
                let f = File::open(&destination);
                if let Ok(mut f) = f {
                    let mut existing_content = Vec::new();
                    let r = f.read_to_end(&mut existing_content);
                    if r.is_ok() && existing_content == output.content {
                        continue; // don't change timestamp on existing file unnecessarily
                    }
                }
            }
            let mut f = File::create(&destination).into_diagnostic()?;
            f.write_all(&output.content).into_diagnostic()?;
        }
        Ok(())
    }
}

/// The file a name reaches: the spelling the filesystem itself gives it, since
/// two spellings it treats as one name one output, and the far end of a
/// symlink, since `File::create` wrote through one and a rename would replace
/// the link instead.
fn resolve_destination(path: &Path) -> miette::Result<PathBuf> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Ok(resolved);
    }
    // Not there to be resolved, which is the ordinary case of an output not
    // written yet. Its directory is, and a link whose target does not exist
    // yet still has to be followed.
    let path = follow_links(path)?;
    let resolved = path
        .parent()
        .zip(path.file_name())
        .and_then(|(parent, name)| Some(std::fs::canonicalize(parent).ok()?.join(name)));
    Ok(resolved.unwrap_or(path))
}

/// A symlink chain walked by hand, which is what `canonicalize` will not do
/// for a link whose target does not exist yet.
fn follow_links(path: &Path) -> miette::Result<PathBuf> {
    let mut path = path.to_path_buf();
    // Linux gives up at 40 too; a chain longer than that is a loop.
    for _ in 0..40 {
        let Ok(target) = std::fs::read_link(&path) else {
            return Ok(path); // not a link, or not there at all
        };
        path = match path.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target,
        };
    }
    Err(miette::Report::msg(format!(
        "the output '{}' is a symlink which leads back to itself",
        path.display()
    )))
}

struct RecordIntoDepfile(Rc<RefCell<Depfile>>);

impl RebuildDependencyRecorder for RecordIntoDepfile {
    fn record_dependency(&self, filename: &str) {
        self.0.borrow_mut().add_dependency(&PathBuf::from(filename))
    }
}

impl std::fmt::Debug for RecordIntoDepfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<depfile>")
    }
}
