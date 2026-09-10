// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::ast_discoverer::{Discoveries, DiscoveryErr};
use crate::output_generators::RsOutput;
use crate::{
    cxxbridge::CxxBridge, Error as EngineError, GeneratedCpp, IncludeCppEngine,
    RebuildDependencyRecorder,
};
use crate::{proc_macro_span_to_miette_span, CodegenOptions, CppCodegenOptions, LocatedSynError};
use autocxx_parser::directive_names::SUBCLASS;
use autocxx_parser::{AllowlistEntry, RustPath, Subclass, SubclassAttrs};
use indexmap::map::IndexMap as HashMap;
use miette::{Diagnostic, SourceSpan};
use proc_macro2::Ident;
use quote::ToTokens;
use std::{io::Read, path::PathBuf};
use std::{panic::UnwindSafe, path::Path, rc::Rc};
use syn::spanned::Spanned;
use syn::Item;
use thiserror::Error;

/// Errors which may occur when parsing a Rust source file to discover
/// and interpret include_cxx macros.
#[derive(Error, Diagnostic, Debug)]
pub enum ParseError {
    #[error("unable to open the source file containing your autocxx bindings. (This filename is usually specified within your build.rs file.): {0}")]
    FileOpen(std::io::Error),
    #[error("the .rs file couldn't be read: {0}")]
    FileRead(std::io::Error),
    #[error("syntax error interpreting Rust code: {0}")]
    #[diagnostic(transparent)]
    Syntax(LocatedSynError),
    #[error("generate!/generate_ns! was used at the same time as generate_all!")]
    ConflictingAllowlist,
    #[error("the subclass attribute couldn't be parsed: {0}")]
    #[diagnostic(transparent)]
    SubclassSyntax(LocatedSynError),
    #[error("the subclass attribute macro with a superclass attribute requires the Builder::auto_allowlist option to be specified (probably in your build script). This is not recommended - instead you can specify subclass! within your include_cpp!.")]
    SubclassSuperclassWithoutAutoAllowlist(#[source_code] String, #[label("here")] SourceSpan),
    /// The include CPP macro could not be expanded into
    /// Rust bindings to C++, because of some problem during the conversion
    /// process. This could be anything from a C++ parsing error to some
    /// C++ feature that autocxx can't yet handle and isn't able to skip
    /// over. It could also cover errors in your syntax of the `include_cpp`
    /// macro or the directives inside.
    #[error("the include_cpp! macro couldn't be expanded into Rust bindings to C++: {0}")]
    #[diagnostic(transparent)]
    AutocxxCodegenError(EngineError),
    /// There are two or more `include_cpp` macros with the same
    /// mod name.
    #[error("two include_cpp! blocks are both named {name} (one in {first}, one in {second}). The generated bindings file and C++ header are named after the block, and a macro cannot be named after the mod it sits in - it isn't told which mod that is - so each block needs its own name!(...)")]
    ConflictingModNames {
        name: String,
        first: String,
        second: String,
    },
    #[error("this file has more than one include_cpp! block which generates bindings, so there is no telling which of them the #[extern_rust_function], #[extern_rust_type] or #[subclass] items outside them were meant to join. Move the discovered items into a file with a single block, or list them in one block with extern_rust_fun!/subclass!")]
    MultipleModsForDynamicDiscovery,
    #[error("the #[subclass] struct {subclass} is written in {item_scope}, but the include_cpp! block it would join is in {block_scope}. autocxx generates the struct's C++ peer beside that block and refers to the struct as a neighbour of it, so the two have to share a mod.")]
    SubclassOutsideBlockScope {
        subclass: String,
        item_scope: String,
        block_scope: String,
    },
    #[error("a problem occurred while discovering C++ APIs used within the Rust: {0}")]
    Discovery(DiscoveryErr),
}

/// Parse a Rust file, and spot any include_cpp macros within it.
pub fn parse_file<P1: AsRef<Path>>(
    rs_file: P1,
    auto_allowlist: bool,
) -> Result<ParsedFile, ParseError> {
    let mut source_code = String::new();
    let mut file = std::fs::File::open(rs_file).map_err(ParseError::FileOpen)?;
    file.read_to_string(&mut source_code)
        .map_err(ParseError::FileRead)?;
    proc_macro2::fallback::force();
    let source = syn::parse_file(&source_code)
        .map_err(|e| ParseError::Syntax(LocatedSynError::new(e, &source_code)))?;
    parse_file_contents(source, auto_allowlist, &source_code)
}

fn parse_file_contents(
    source: syn::File,
    auto_allowlist: bool,
    file_contents: &str,
) -> Result<ParsedFile, ParseError> {
    #[derive(Default)]
    struct State {
        auto_allowlist: bool,
        results: Vec<Segment>,
        /// Each `#[subclass]` struct discovered outside a block, with the
        /// `mod` scope it was written in.
        extra_superclasses: Vec<(String, Subclass)>,
        discoveries: Discoveries,
    }
    let file_contents = Rc::new(file_contents.to_string());
    impl State {
        fn parse_item(
            &mut self,
            item: Item,
            mod_path: Option<RustPath>,
            file_contents: Rc<String>,
        ) -> Result<(), ParseError> {
            let result = match item {
                Item::Macro(mac)
                    if mac
                        .mac
                        .path
                        .segments
                        .last()
                        .map(|s| s.ident == "include_cpp")
                        .unwrap_or(false) =>
                {
                    Segment::Autocxx(
                        crate::IncludeCppEngine::new_from_syn(mac.mac, file_contents)
                            .map_err(ParseError::AutocxxCodegenError)?,
                    )
                }
                Item::Mod(itm)
                    if itm.attrs.iter().any(|attr| {
                        attr.path().to_token_stream().to_string() == "cxx :: bridge"
                    }) =>
                {
                    Segment::Cxx(CxxBridge::from(itm))
                }
                Item::Mod(itm) => {
                    if let Some((_, items)) = itm.content {
                        let mut mod_state = State {
                            auto_allowlist: self.auto_allowlist,
                            ..Default::default()
                        };
                        let mod_path = match &mod_path {
                            None => RustPath::new_from_ident(itm.ident.clone()),
                            Some(mod_path) => mod_path.append(itm.ident.clone()),
                        };
                        for item in items {
                            mod_state.parse_item(
                                item,
                                Some(mod_path.clone()),
                                file_contents.clone(),
                            )?
                        }
                        self.extra_superclasses.extend(mod_state.extra_superclasses);
                        self.discoveries.extend(mod_state.discoveries);
                        Segment::Mod(itm.ident, mod_state.results)
                    } else {
                        Segment::Other
                    }
                }
                Item::Struct(ref its) => {
                    let attrs = &its.attrs;
                    let is_superclass_attr = attrs.iter().find(|attr| {
                        attr.path()
                            .segments
                            .last()
                            .map(|seg| seg.ident == "is_subclass" || seg.ident == SUBCLASS)
                            .unwrap_or(false)
                    });
                    if let Some(is_superclass_attr) = is_superclass_attr {
                        if is_superclass_attr.meta.require_path_only().is_err() {
                            let subclass = its.ident.clone();
                            let args: SubclassAttrs =
                                is_superclass_attr.parse_args().map_err(|e| {
                                    ParseError::SubclassSyntax(LocatedSynError::new(
                                        e,
                                        &file_contents,
                                    ))
                                })?;
                            if let Some(superclass) = args.superclass {
                                if !self.auto_allowlist {
                                    return Err(
                                        ParseError::SubclassSuperclassWithoutAutoAllowlist(
                                            file_contents.to_string(),
                                            proc_macro_span_to_miette_span(&its.span()),
                                        ),
                                    );
                                }
                                self.extra_superclasses.push((
                                    scope_of(&mod_path),
                                    Subclass {
                                        superclass,
                                        subclass,
                                    },
                                ))
                            }
                        }
                    }
                    self.discoveries
                        .search_item(&item, mod_path)
                        .map_err(ParseError::Discovery)?;
                    Segment::Other
                }
                _ => {
                    self.discoveries
                        .search_item(&item, mod_path)
                        .map_err(ParseError::Discovery)?;
                    Segment::Other
                }
            };
            self.results.push(result);
            Ok(())
        }
    }
    let mut state = State {
        auto_allowlist,
        ..Default::default()
    };
    for item in source.items {
        state.parse_item(item, None, file_contents.clone())?
    }
    let State {
        auto_allowlist,
        mut results,
        mut extra_superclasses,
        mut discoveries,
    } = state;

    let must_handle_discovered_things = discoveries.found_rust()
        || !extra_superclasses.is_empty()
        || (auto_allowlist && discoveries.found_allowlist());

    // We do not want to enter this 'if' block unless the above conditions are true,
    // since we may emit errors.
    if must_handle_discovered_things {
        // A `parse_only!` block generates nothing, so a discovered item has no
        // bridge to join there, and its config is frozen against writes in any
        // case. It is not a candidate.
        let generating_blocks = all_blocks(&results)
            .filter(|block| !block.get_config().parse_only)
            .count();
        if generating_blocks > 1 {
            return Err(ParseError::MultipleModsForDynamicDiscovery);
        }
        if generating_blocks == 0 && all_blocks(&results).next().is_none() {
            // Discovered items with no block at all to join: make one for them.
            results.push(Segment::Autocxx(IncludeCppEngine::new_for_autodiscover()));
        }
        // Nothing to attach to when every block in the file only parses; that
        // file generates nothing either way.
        let mut candidates = Vec::new();
        all_blocks_mut_with_scopes(&mut results, &[], &mut candidates);
        if let Some((scope, engine)) = candidates
            .into_iter()
            .find(|(_, block)| !block.get_config().parse_only)
        {
            // Discovered paths are relative to the file. The generated `use`
            // for one climbs a single mod, out of the generated bindings and
            // into the mod holding the block, so a block written inside mods
            // has to climb back out of those too.
            let depth = scope.len();
            for fun in &mut discoveries.extern_rust_funs {
                fun.path = fun.path.from_within_mods(depth);
            }
            for path in &mut discoveries.extern_rust_types {
                *path = path.from_within_mods(depth);
            }
            // A subclass's peer is generated beside the block and names the
            // struct as a plain `super::` neighbour, with no path to climb, so
            // the two really do have to share a mod.
            let block_scope = scope.join("::");
            if let Some((item_scope, sc)) = extra_superclasses
                .iter()
                .find(|(item_scope, _)| *item_scope != block_scope)
            {
                return Err(ParseError::SubclassOutsideBlockScope {
                    subclass: sc.subclass.to_string(),
                    item_scope: describe_scope(item_scope),
                    block_scope: describe_scope(&block_scope),
                });
            }
            engine
                .config_mut()
                .subclasses
                .extend(extra_superclasses.drain(..).map(|(_, sc)| sc));
            if auto_allowlist {
                for cpp in discoveries.cpp_list {
                    engine
                        .config_mut()
                        .allowlist
                        .push(AllowlistEntry::Item(cpp))
                        .map_err(|_| ParseError::ConflictingAllowlist)?;
                }
            }
            engine
                .config_mut()
                .extern_rust_funs
                .append(&mut discoveries.extern_rust_funs);
            engine
                .config_mut()
                .rust_types
                .append(&mut discoveries.extern_rust_types);
        }
    }
    for block in all_blocks_mut(&mut results) {
        block.config.confirm_complete();
    }
    Ok(ParsedFile(results))
}

/// A Rust file parsed by autocxx. May contain zero or more autocxx 'engines',
/// i.e. the `IncludeCpp` class, corresponding to zero or more include_cpp
/// macros within this file. Also contains `syn::Item` structures for all
/// the rest of the Rust code, such that it can be reconstituted if necessary.
pub struct ParsedFile(Vec<Segment>);

#[allow(clippy::large_enum_variant)]
enum Segment {
    Autocxx(IncludeCppEngine),
    Cxx(CxxBridge),
    /// An inline `mod`, keeping its name so a diagnostic can say which one an
    /// `include_cpp!` was written in.
    Mod(Ident, Vec<Segment>),
    Other,
}

/// Every `include_cpp!` block in these segments, at whatever `mod` depth.
/// Everything which acts on the blocks of a file walks them through this: a
/// pass which stopped at the top level would treat a block inside a `mod` as
/// absent, and the two halves would then disagree about how many blocks the
/// file has.
fn all_blocks(segments: &[Segment]) -> impl Iterator<Item = &IncludeCppEngine> {
    segments
        .iter()
        .flat_map(|s| -> Box<dyn Iterator<Item = &IncludeCppEngine>> {
            match s {
                Segment::Autocxx(includecpp) => Box::new(std::iter::once(includecpp)),
                Segment::Mod(_, segments) => Box::new(all_blocks(segments)),
                _ => Box::new(std::iter::empty()),
            }
        })
}

fn all_blocks_mut(segments: &mut [Segment]) -> impl Iterator<Item = &mut IncludeCppEngine> {
    segments
        .iter_mut()
        .flat_map(|s| -> Box<dyn Iterator<Item = &mut IncludeCppEngine>> {
            match s {
                Segment::Autocxx(includecpp) => Box::new(std::iter::once(includecpp)),
                Segment::Mod(_, segments) => Box::new(all_blocks_mut(segments)),
                _ => Box::new(std::iter::empty()),
            }
        })
}

/// Every block paired with the `mod` scope it was written in, mutably.
fn all_blocks_mut_with_scopes<'a>(
    segments: &'a mut [Segment],
    scope: &[String],
    found: &mut Vec<(Vec<String>, &'a mut IncludeCppEngine)>,
) {
    for segment in segments.iter_mut() {
        match segment {
            Segment::Autocxx(includecpp) => found.push((scope.to_vec(), includecpp)),
            Segment::Mod(ident, segments) => {
                let mut inner = scope.to_vec();
                inner.push(ident.to_string());
                all_blocks_mut_with_scopes(segments, &inner, found)
            }
            _ => {}
        }
    }
}

/// The `mod` scope an item was found in, as a path from the file's root.
fn scope_of(mod_path: &Option<RustPath>) -> String {
    match mod_path {
        None => String::new(),
        Some(path) => path
            .segments()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join("::"),
    }
}

/// Every block paired with the `mod` path it was written in, for diagnostics.
fn all_blocks_with_scopes<'a>(
    segments: &'a [Segment],
    scope: &str,
    found: &mut Vec<(String, &'a IncludeCppEngine)>,
) {
    for segment in segments {
        match segment {
            Segment::Autocxx(includecpp) => found.push((scope.to_string(), includecpp)),
            Segment::Mod(ident, segments) => {
                let inner = if scope.is_empty() {
                    ident.to_string()
                } else {
                    format!("{scope}::{ident}")
                };
                all_blocks_with_scopes(segments, &inner, found)
            }
            _ => {}
        }
    }
}

/// How to name a block's location in a diagnostic.
fn describe_scope(scope: &str) -> String {
    if scope.is_empty() {
        "the top level of the file".to_string()
    } else {
        format!("mod {scope}")
    }
}

pub trait CppBuildable {
    fn generate_h_and_cxx(
        &self,
        cpp_codegen_options: &CppCodegenOptions,
    ) -> Result<GeneratedCpp, cxx_gen::Error>;
}

impl ParsedFile {
    /// Get all the autocxx `include_cpp` macros found in this file.
    pub fn get_autocxxes(&self) -> impl Iterator<Item = &IncludeCppEngine> {
        all_blocks(&self.0)
    }

    /// Get all the areas of Rust code which need to be built for these bindings.
    /// A shortcut for `get_autocxxes()` then calling `get_rs_output` on each.
    pub fn get_rs_outputs(&self) -> impl Iterator<Item = RsOutput<'_>> {
        self.get_autocxxes().map(|autocxx| autocxx.get_rs_output())
    }

    /// Get all items which can result in C++ code
    pub fn get_cpp_buildables(&self) -> impl Iterator<Item = &dyn CppBuildable> {
        fn do_get_cpp_buildables(segments: &[Segment]) -> impl Iterator<Item = &dyn CppBuildable> {
            segments
                .iter()
                .flat_map(|s| -> Box<dyn Iterator<Item = &dyn CppBuildable>> {
                    match s {
                        Segment::Autocxx(includecpp) => {
                            Box::new(std::iter::once(includecpp as &dyn CppBuildable))
                        }
                        Segment::Cxx(cxxbridge) => {
                            Box::new(std::iter::once(cxxbridge as &dyn CppBuildable))
                        }
                        Segment::Mod(_, segments) => Box::new(do_get_cpp_buildables(segments)),
                        _ => Box::new(std::iter::empty()),
                    }
                })
        }

        do_get_cpp_buildables(&self.0)
    }

    fn get_autocxxes_mut(&mut self) -> impl Iterator<Item = &mut IncludeCppEngine> {
        all_blocks_mut(&mut self.0)
    }

    /// Refuse two blocks which would generate over each other's output.
    ///
    /// The generated file is named after the block (`name!`, defaulting to
    /// `ffi`), and so is its C++ header. A block cannot be named after the
    /// `mod` it sits in instead: the macro half of autocxx has to compute the
    /// same name to `include!`, and a macro is not told which `mod` it was
    /// written in. Two same-named blocks therefore collide however Rust scopes
    /// them.
    fn check_mod_names(&self) -> Result<(), ParseError> {
        let mut blocks = Vec::new();
        all_blocks_with_scopes(&self.0, "", &mut blocks);
        let mut seen: HashMap<String, String> = HashMap::new();
        for (scope, block) in blocks {
            let name = block.get_mod_name();
            if let Some(first) = seen.insert(name.clone(), scope.clone()) {
                return Err(ParseError::ConflictingModNames {
                    name,
                    first: describe_scope(&first),
                    second: describe_scope(&scope),
                });
            }
        }
        Ok(())
    }

    /// Determines the include dirs that were set for each include_cpp, so they can be
    /// used as input to a `cc::Build`.
    #[cfg(any(test, feature = "build"))]
    pub(crate) fn include_dirs(&self) -> impl Iterator<Item = &PathBuf> {
        fn do_get_include_dirs(segments: &[Segment]) -> impl Iterator<Item = &PathBuf> {
            segments
                .iter()
                .flat_map(|s| -> Box<dyn Iterator<Item = &PathBuf>> {
                    match s {
                        Segment::Autocxx(includecpp) => Box::new(includecpp.include_dirs()),
                        Segment::Mod(_, segments) => Box::new(do_get_include_dirs(segments)),
                        _ => Box::new(std::iter::empty()),
                    }
                })
        }

        do_get_include_dirs(&self.0)
    }

    pub fn resolve_all(
        &mut self,
        autocxx_inc: Vec<PathBuf>,
        extra_clang_args: &[&str],
        dep_recorder: Option<Box<dyn RebuildDependencyRecorder>>,
        codegen_options: &CodegenOptions,
    ) -> Result<(), ParseError> {
        // Before generating anything: a name collision is a property of the
        // file, and reporting it after the first block has been generated would
        // bury it behind whatever that generation had to say.
        self.check_mod_names()?;
        let inner_dep_recorder: Option<Rc<dyn RebuildDependencyRecorder>> =
            dep_recorder.map(Rc::from);
        for include_cpp in self.get_autocxxes_mut() {
            #[allow(clippy::manual_map)] // because of dyn shenanigans
            let dep_recorder: Option<Box<dyn RebuildDependencyRecorder>> = match &inner_dep_recorder
            {
                None => None,
                Some(inner_dep_recorder) => Some(Box::new(CompositeDepRecorder::new(
                    inner_dep_recorder.clone(),
                ))),
            };
            include_cpp
                .generate(
                    autocxx_inc.clone(),
                    extra_clang_args,
                    dep_recorder,
                    codegen_options,
                )
                .map_err(ParseError::AutocxxCodegenError)?
        }
        Ok(())
    }
}

/// Shenanigans required to share the same RebuildDependencyRecorder
/// with all of the include_cpp instances in this one file.
#[derive(Debug, Clone)]
struct CompositeDepRecorder(Rc<dyn RebuildDependencyRecorder>);

impl CompositeDepRecorder {
    fn new(inner: Rc<dyn RebuildDependencyRecorder>) -> Self {
        CompositeDepRecorder(inner)
    }
}

impl UnwindSafe for CompositeDepRecorder {}

impl RebuildDependencyRecorder for CompositeDepRecorder {
    fn record_dependency(&self, filename: &str) {
        self.0.record_dependency(filename);
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_file_contents, ParseError, ParsedFile};
    use autocxx_parser::IncludeCpp;
    use proc_macro2::Span;
    use quote::ToTokens;
    use syn::parse_quote;

    fn parse(src: &str) -> ParsedFile {
        parse_file_contents(syn::parse_file(src).unwrap(), false, src).expect("parse failed")
    }

    fn parse_err(src: &str) -> ParseError {
        parse_file_contents(syn::parse_file(src).unwrap(), false, src)
            .err()
            .expect("parse unexpectedly succeeded")
    }

    /// The key the codegen files an archive entry under has to be the key the
    /// macro looks it up by, and the codegen augments its copy of the config -
    /// `confirm_complete`, discovered `extern_rust_function`s - before it
    /// writes the archive.
    #[test]
    fn archive_key_survives_augmentation() {
        let src = r#"
            autocxx::include_cpp! {
                #include "a.h"
            }

            #[autocxx::extern_rust::extern_rust_function]
            pub fn called_from_cpp() {}
        "#;
        let parsed = parse(src);
        let engine = parsed
            .get_autocxxes()
            .next()
            .expect("no include_cpp! found");
        // The config really was augmented, so this is not a vacuous comparison.
        assert!(!engine.get_config().extern_rust_funs.is_empty());
        let hexathorpe = syn::token::Pound(Span::call_site());
        let macro_side: IncludeCpp = parse_quote! {
            #hexathorpe include "a.h"
        };
        assert_eq!(engine.config_hash(), macro_side.config_hash());
    }

    /// A block inside a `mod` has to have its config completed like any other:
    /// an allowlist left `Unspecified` is a state `bindgen_allowlist` treats as
    /// impossible, and it panics rather than returning.
    #[test]
    fn nested_block_config_is_completed() {
        let src = r#"
            mod inner {
                autocxx::include_cpp! {
                    #include "a.h"
                }
            }
        "#;
        let parsed = parse(src);
        let engine = parsed
            .get_autocxxes()
            .next()
            .expect("no include_cpp! found");
        assert!(engine.get_config().bindgen_allowlist().is_some());
    }

    /// Discovered items belong to the file's one block wherever it sits. A
    /// second, synthesised block would generate a bridge nothing includes.
    #[test]
    fn discoveries_reach_a_nested_block() {
        let src = r#"
            mod inner {
                autocxx::include_cpp! {
                    #include "a.h"
                    generate_all!()
                }
            }

            #[autocxx::extern_rust::extern_rust_function]
            pub fn called_from_cpp() {}
        "#;
        let parsed = parse(src);
        assert_eq!(parsed.get_autocxxes().count(), 1);
        let engine = parsed.get_autocxxes().next().unwrap();
        assert!(!engine.get_config().extern_rust_funs.is_empty());
    }

    /// Two blocks are two candidate homes for a discovered item, whichever
    /// scopes they sit in, so the ambiguity is reported rather than resolved by
    /// position.
    #[test]
    fn discoveries_with_a_nested_and_a_top_level_block_are_ambiguous() {
        let src = r#"
            autocxx::include_cpp! {
                #include "a.h"
                generate_all!()
            }

            mod inner {
                autocxx::include_cpp! {
                    #include "a.h"
                    name!(ffi_inner)
                    generate_all!()
                }
            }

            #[autocxx::extern_rust::extern_rust_function]
            pub fn called_from_cpp() {}
        "#;
        assert!(matches!(
            parse_err(src),
            ParseError::MultipleModsForDynamicDiscovery
        ));
    }

    /// `parse_only!` generates nothing, so it is no home for a discovered item
    /// - and its config is frozen, so writing to it panics.
    #[test]
    fn discoveries_skip_a_parse_only_block() {
        let src = r#"
            mod inner {
                autocxx::include_cpp! {
                    #include "a.h"
                    parse_only!()
                }
            }

            #[autocxx::extern_rust::extern_rust_function]
            pub fn called_from_cpp() {}
        "#;
        let parsed = parse(src);
        // No block was synthesised beside the parse-only one, which would have
        // generated a bridge nothing includes.
        assert_eq!(parsed.get_autocxxes().count(), 1);
        assert!(parsed
            .get_autocxxes()
            .next()
            .unwrap()
            .get_config()
            .extern_rust_funs
            .is_empty());
    }

    /// Nested and top-level blocks coexist when nothing is discovered; both get
    /// completed configs.
    #[test]
    fn nested_and_top_level_blocks_coexist() {
        let src = r#"
            autocxx::include_cpp! {
                #include "a.h"
            }

            mod inner {
                autocxx::include_cpp! {
                    #include "a.h"
                    name!(ffi_inner)
                }
            }
        "#;
        let parsed = parse(src);
        assert_eq!(parsed.get_autocxxes().count(), 2);
        for engine in parsed.get_autocxxes() {
            assert!(engine.get_config().bindgen_allowlist().is_some());
        }
    }

    /// The generated file name and the C++ header name both derive from
    /// `name!` alone - a macro cannot know which `mod` it was written in - so
    /// two blocks sharing a name collide however they are scoped, and the
    /// diagnostic has to name what collided.
    #[test]
    fn same_name_in_two_scopes_is_refused_by_name() {
        let src = r#"
            mod a {
                autocxx::include_cpp! {
                    #include "a.h"
                }
            }

            mod b {
                autocxx::include_cpp! {
                    #include "b.h"
                }
            }
        "#;
        let parsed = parse(src);
        let err = parsed
            .check_mod_names()
            .expect_err("colliding names accepted");
        let msg = err.to_string();
        assert!(msg.contains("ffi"), "{msg}");
        assert!(msg.contains("mod a") && msg.contains("mod b"), "{msg}");
    }

    /// Distinct names in distinct scopes are the supported nested shape.
    #[test]
    fn distinct_names_in_two_scopes_are_accepted() {
        let src = r#"
            mod a {
                autocxx::include_cpp! {
                    #include "a.h"
                    name!(ffi_a)
                }
            }

            mod b {
                autocxx::include_cpp! {
                    #include "b.h"
                    name!(ffi_b)
                }
            }
        "#;
        parse(src)
            .check_mod_names()
            .expect("distinct names refused");
    }

    /// A collision between a top-level block and a nested one names both ends.
    #[test]
    fn collision_between_scopes_names_both_ends() {
        let src = r#"
            autocxx::include_cpp! {
                #include "a.h"
            }

            mod inner {
                autocxx::include_cpp! {
                    #include "b.h"
                }
            }
        "#;
        let msg = parse(src)
            .check_mod_names()
            .expect_err("colliding names accepted")
            .to_string();
        assert!(msg.contains("the top level of the file"), "{msg}");
        assert!(msg.contains("mod inner"), "{msg}");
    }

    /// A discovered item's path is recorded relative to the file, and the
    /// bindings which `use` it are generated inside the block's own mod, so a
    /// nested block's copy has to climb back out.
    #[test]
    fn discovered_paths_climb_out_of_nested_mods() {
        let src = r#"
            mod outer {
                mod inner {
                    autocxx::include_cpp! {
                        #include "a.h"
                        generate_all!()
                    }
                }
            }

            #[autocxx::extern_rust::extern_rust_function]
            pub fn called_from_cpp() {}

            #[autocxx::extern_rust::extern_rust_type]
            pub struct UsedFromCpp;
        "#;
        let parsed = parse(src);
        let config = parsed.get_autocxxes().next().unwrap().get_config();
        assert_eq!(
            config.extern_rust_funs[0]
                .path
                .to_token_stream()
                .to_string(),
            "super :: super :: called_from_cpp"
        );
        assert_eq!(
            config.rust_types[0].to_token_stream().to_string(),
            "super :: super :: UsedFromCpp"
        );
    }

    /// The same paths from a top-level block, which is already in the file's
    /// own scope.
    #[test]
    fn discovered_paths_from_a_top_level_block_do_not_climb() {
        let src = r#"
            autocxx::include_cpp! {
                #include "a.h"
                generate_all!()
            }

            #[autocxx::extern_rust::extern_rust_function]
            pub fn called_from_cpp() {}
        "#;
        let parsed = parse(src);
        let config = parsed.get_autocxxes().next().unwrap().get_config();
        assert_eq!(
            config.extern_rust_funs[0]
                .path
                .to_token_stream()
                .to_string(),
            "called_from_cpp"
        );
    }

    /// A `#[subclass]` struct is named as a neighbour of the block, so one
    /// written in a different mod is refused rather than generated into code
    /// which cannot name it.
    #[test]
    fn subclass_outside_the_block_scope_is_refused() {
        let src = r#"
            mod inner {
                autocxx::include_cpp! {
                    #include "a.h"
                    generate_all!()
                }
            }

            #[autocxx::subclass::subclass(superclass("Observer"))]
            pub struct MyObserver;
        "#;
        let err = parse_file_contents(syn::parse_file(src).unwrap(), true, src)
            .err()
            .expect("mismatched scopes accepted");
        let msg = err.to_string();
        assert!(msg.contains("MyObserver"), "{msg}");
        assert!(msg.contains("mod inner"), "{msg}");
        assert!(msg.contains("the top level of the file"), "{msg}");
    }

    /// The same struct beside its block in the same mod is the supported shape.
    #[test]
    fn subclass_beside_a_nested_block_is_accepted() {
        let src = r#"
            mod inner {
                autocxx::include_cpp! {
                    #include "a.h"
                    generate_all!()
                }

                #[autocxx::subclass::subclass(superclass("Observer"))]
                pub struct MyObserver;
            }
        "#;
        let parsed = parse_file_contents(syn::parse_file(src).unwrap(), true, src)
            .expect("matching scopes refused");
        let config = parsed.get_autocxxes().next().unwrap().get_config();
        assert_eq!(config.subclasses.len(), 1);
    }
}
