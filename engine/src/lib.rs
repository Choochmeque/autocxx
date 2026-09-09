//! The core of the `autocxx` engine, used by both the
//! `autocxx_macro` and also code generators (e.g. `autocxx_build`).
//! See [IncludeCppEngine] for general description of how this engine works.

// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// `deny` rather than `forbid` so that `vendored_bindgen` can allow it back:
// bindgen calls libclang and is full of `unsafe`. Nothing autocxx writes may
// use it, which is what this is for, and `forbid` cannot be lifted per module.
#![deny(unsafe_code)]
#![cfg_attr(feature = "nightly", feature(doc_cfg))]

// Declares `mod vendored_bindgen`, autocxx's copy of bindgen. The sources are a
// git submodule pinned at an upstream tag; `build.rs` applies
// `third_party/patches` to them and rewrites the result into a module. The
// patches add what autocxx needs and upstream does not report: C++ access
// specifiers, special members, virtualness, the original C++ spelling of a
// nested name, and markers on opaque types and references.
//
// The declaration is generated rather than written here because `#[path]` takes
// a string literal and nothing else, so `concat!(env!("OUT_DIR"), ..)` cannot
// be spelled at this end.
include!(concat!(env!("OUT_DIR"), "/vendored_bindgen_mount.rs"));

mod ast_discoverer;
mod clang_target;
mod conversion;
mod cpp_standard;
mod cxxbridge;
mod known_types;
mod minisyn;
mod output_generators;
mod parse_callbacks;
mod parse_file;
mod rust_pretty_printer;
mod types;

#[cfg(any(test, feature = "build"))]
mod builder;
#[cfg(any(test, feature = "build"))]
mod cxx_version_parity;

// Public because `Error::Bindgen` carries one and a caller matching on it has
// to be able to name the payload. It used to be nameable as
// `autocxx_bindgen::BindgenError`; `vendored_bindgen` is private, so autocxx
// re-exports the one type of bindgen's that reaches its own API.
pub use crate::vendored_bindgen::BindgenError;
use autocxx_parser::{EnumStyle, IncludeCppConfig, UnsafePolicy};
use conversion::BridgeConverter;
use miette::{SourceOffset, SourceSpan};
use parse_callbacks::{AutocxxParseCallbacks, ParseCallbackResults, UnindexedParseCallbackResults};
use parse_file::CppBuildable;
use proc_macro2::TokenStream as TokenStream2;
use regex::Regex;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::{
    fs::File,
    io::prelude::*,
    path::Path,
    process::{Command, Stdio},
};
use tempfile::NamedTempFile;

use quote::ToTokens;
use syn::Result as ParseResult;
use syn::{
    parse::{Parse, ParseStream},
    parse_quote, ItemMod, Macro,
};
use thiserror::Error;

use itertools::{join, Itertools};
use known_types::known_types;
use log::info;
use miette::Diagnostic;

use crate::vendored_bindgen as bindgen;

#[cfg(any(test, feature = "build"))]
pub use builder::{
    add_sanitizer_flags, Builder, BuilderBuild, BuilderContext, BuilderError, BuilderResult,
    BuilderSuccess,
};
pub use output_generators::{generate_rs_archive, generate_rs_single, RsOutput};
pub use parse_file::{parse_file, ParseError, ParsedFile};

pub use cxx_gen::HEADER;

#[derive(Clone)]
/// Some C++ content which should be written to disk and built.
pub struct CppFilePair {
    /// Declarations to go into a header file.
    pub header: Vec<u8>,
    /// Implementations to go into a .cpp file.
    pub implementation: Option<Vec<u8>>,
    /// The name which should be used for the header file
    /// (important as it may be `#include`d elsewhere)
    pub header_name: String,
}

/// All generated C++ content which should be written to disk.
pub struct GeneratedCpp(pub Vec<CppFilePair>);

/// A [`syn::Error`] which also implements [`miette::Diagnostic`] so can be pretty-printed
/// to show the affected span of code.
#[derive(Error, Debug, Diagnostic)]
#[error("{err}")]
pub struct LocatedSynError {
    err: syn::Error,
    #[source_code]
    file: String,
    #[label("error here")]
    span: SourceSpan,
}

impl LocatedSynError {
    fn new(err: syn::Error, file: &str) -> Self {
        let span = proc_macro_span_to_miette_span(&err.span());
        Self {
            err,
            file: file.to_string(),
            span,
        }
    }
}

/// Errors which may occur in generating bindings for these C++
/// functions.
#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error("Bindgen was unable to generate the initial .rs bindings for this file. This may indicate a parsing problem with the C++ headers.")]
    Bindgen(BindgenError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    MacroParsing(LocatedSynError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    BindingsParsing(LocatedSynError),
    #[error("no C++ include directory was provided.")]
    NoAutoCxxInc,
    #[error(transparent)]
    #[diagnostic(transparent)]
    Conversion(conversion::ConvertError),
    #[error("Using `unsafe_references_wrapped` requires the Rust nightly `arbitrary_self_types` feature")]
    WrappedReferencesButNoArbitrarySelfTypes,
}

/// Result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

struct GenerationResults {
    item_mod: ItemMod,
    cpp: Option<CppFilePair>,
    #[allow(dead_code)]
    inc_dirs: Vec<PathBuf>,
    cxxgen_header_name: String,
}
enum State {
    NotGenerated,
    ParseOnly,
    Generated(Box<GenerationResults>),
}

/// Code generation options.
#[derive(Default)]
pub struct CodegenOptions<'a> {
    // An option used by the test suite to force a more convoluted
    // route through our code, to uncover bugs.
    pub force_wrapper_gen: bool,
    /// Options about the C++ code generation.
    pub cpp_codegen_options: CppCodegenOptions<'a>,
}

/// The arguments autocxx parses headers with, before a caller's own.
///
/// The standard here is the one the headers are *parsed* at, which is not the
/// one the generated C++ is compiled at - the caller's `cc::Build` chooses that,
/// and autocxx generates C++ which compiles as C++14. C++17 is the parse
/// standard because a C++17 function type carries the function's exception
/// specification, which is the only way to learn which way a `noexcept(expr)`
/// operand resolved; see [`cpp_standard`]. It comes first in the argument
/// vector, so a caller who needs an older one can say so - see
/// [`builder::Builder::extra_clang_args`].
const AUTOCXX_CLANG_ARGS: &[&str; 8] = &[
    "-x",
    "c++",
    "-std=c++17",
    // Which symbols a shared library exports. bindgen reads it as a statement
    // about whether a symbol can be linked to at all, and drops every function,
    // and every static data member, whose visibility is not `default` - so
    // whatever visibility clang's driver happens to default to decides whether a
    // header binds anything. clang's WebAssembly driver defaults to `hidden`,
    // which is why a `wasm32-*` target bound types and none of their methods,
    // with nothing said about it anywhere: see google/autocxx#1508.
    //
    // Visibility is not a question autocxx can answer from a header, and none of
    // what it generates depends on the answer: the shims it writes are compiled
    // into the same artifact as the calls they serve, where a hidden symbol
    // links like any other. Where the definition is in a separate shared
    // library which does not export it, the binding now exists and the link
    // fails naming the symbol, rather than the method silently not being there.
    //
    // Saying `default` is what a host or cross compilation for any other target
    // already gets, so this restates a default rather than overriding anything
    // in the builds autocxx is used from, and it goes before a
    // caller's own arguments: `-fvisibility=hidden` in `extra_clang_args` still
    // wins, and still means bindgen drops what the header hides. Visibility of
    // the *compiled* C++ is a matter for the `cc::Build` the caller gets back.
    "-fvisibility=default",
    // C++17 removed three things a header written for an older standard may
    // still contain, and clang's diagnostic for each of them is an error by
    // default, which bindgen treats as fatal. Raising the standard autocxx
    // parses at must not stop it reading a header it read before, so each is
    // demoted to a warning - `-Wno-error=`, not `-Wno-`, which would silence it
    // outright and hide a real thing about the header. bindgen prints a
    // warning and carries on.
    //
    // These change only how loudly clang reports what it parsed. Nothing here
    // changes the declarations autocxx sees, which is why a library facility
    // C++17 removed - `std::auto_ptr` - is not addressed the same way and needs
    // the `-std=` escape instead.
    "-Wno-error=dynamic-exception-spec",
    "-Wno-error=register",
    "-Wno-error=increment-bool",
    "-DBINDGEN",
];

/// Implement to learn of the files this build process reads - the Rust
/// source containing `include_cpp!` and every header the preprocessor
/// opened while parsing it - such that your build system can choose to
/// rerun the build process if any of them changes in future.
pub trait RebuildDependencyRecorder: std::fmt::Debug {
    /// Records that this autocxx build read the given file, so that a
    /// change to it invalidates what this build produced. Headers arrive
    /// as full paths; the Rust input arrives as the caller named it.
    fn record_dependency(&self, filename: &str);
}

/// Core of the autocxx engine.
///
/// The basic idea is this. We will run `bindgen` which will spit
/// out a ton of Rust code corresponding to all the types and functions
/// defined in C++. We'll then post-process that bindgen output
/// into a form suitable for ingestion by `cxx`.
/// (It's the `BridgeConverter` mod which does that.)
/// Along the way, the `bridge_converter` might tell us of additional
/// C++ code which we should generate, e.g. wrappers to move things
/// into and out of `UniquePtr`s.
///
/// # Build time
///
/// Everything here runs inside `autocxx_engine`, including the `libclang`
/// parse, because bindgen is vendored as `vendored_bindgen` rather than
/// depended upon. The exception is the final C++ codegen, which belongs to
/// `cxx_gen`.
///
/// ```text
///   .rs input (your source, containing include_cpp!)
///        |
///        v
///   File parser --> Config from include_cpp -----------------+
///                             |                              |
///                             | configures                   |
///                             v                              |
///   C++ headers --> +-- vendored_bindgen -------------+      |
///                   |  libclang parse --> bindgen IR  |      |
///                   +----------------|----------------+      |
///                                    v                       |
///                       bindgen generated bindings           |
///                                    |                       |
///                                    v                       |
///                             Parse with syn                 |
///                                    |                       |
///                                    v                       |
///                            'conversion' mod <--------------+
///                              |         |
///                              |         +-- autocxx C++ codegen --> C++ output
///                              v
///                     Generated .rs TokenStream
///                              |         |
///                              |         +-- autocxx .rs codegen --> .rs output
///                              v
///                      cxx_gen C++ codegen ------------------> C++ output
/// ```
///
/// The two arrows marked `C++ output` land in the same place: autocxx's own
/// C++ and cxx's are compiled and linked together below.
///
/// # `rustc` build time
///
/// ```text
///   .rs input, with the generated .rs output included into it
///        |
///        v
///   autocxx include_cpp! macro (autocxx_macro)
///        |
///        v
///   cxx procedural macro (cxx)
///        |
///        v
///   fully expanded Rust code ---+
///                               |
///                               +--> linker
///                               |
///   compiled C++ output --------+
/// ```
///
/// # A zoomed-in view of the "conversion" part
///
/// Parsing produces a list of APIs; each analysis phase then consumes that
/// list and emits a new one, parameterized by a richer set of metadata.
///
/// 1. Parse the bindgen mod into unanalyzed APIs, noting on the way which
///    type names a same-named C++ variable hides, and which names bindgen
///    defined twice. The Rust codegen needs both to repair the bindgen mod,
///    and this is the last point at which either is knowable.
/// 2. Typedef analysis: point bindgen-style typedef targets (`root::std::unique_ptr`)
///    at their cxx-style equivalents (`UniquePtr`).
/// 3. POD analysis: confirm that the types the user asked to be POD really are,
///    and mark dependent types. Everything else stays opaque.
/// 4. Discard static data whose type isn't exposed exactly as bindgen
///    declared it, since a static is re-exported by `use`ing bindgen's
///    declaration. POD structs and enums survive; everything else goes.
/// 5. Break the link on typedefs pointing at something we can't represent, and
///    use the typedef itself as a first-class type instead.
/// 6. Add base-class casts, and synthesize allocators and deallocators.
/// 7. Function materialization analysis: work out which functions are plain
///    entries in the `cxx::bridge` mod and which need a C++ wrapper function.
///    This is the most complex part of autocxx.
/// 8. Mark as abstract any type whose functions turned out to be pure
///    virtual, take its constructors away, and reject it outright if it is
///    nested inside another type, which cxx has no way to spell. Then
///    withdraw the constructors and allocators of any type whose destructor
///    is inaccessible, since Rust must never own one.
/// 9. Note which constructors, and which alloc and free functions, each type
///    depends on, so garbage collection doesn't take them later; and, now
///    that abstractness and constructors are settled, turn the functions
///    which can't be generated at all - protected ones, say - into ignored
///    items.
/// 10. Name check: confirm the names can be represented in cxx, and settle on
///     the name each type goes by inside the bridge mod's flat namespace.
/// 11. Turn any API which depends on an ignored item, or on a type we never
///     knew about at all, into an ignored item of its own carrying the
///     reason - rather than removing it silently.
/// 12. Garbage collection: follow the edges outwards from the allowlist, and
///     drop everything unreachable.
/// 13. C int analysis: spot the C types cxx cannot spell - the
///     variable-length integers, `void` and the C++ character types - and add
///     an API for each so the generated C++ declares a typedef for it.
/// 14. Confirm that every `generate!` still names something which survived
///     all of the above, and that every `derive!` names a type whose Rust
///     definition the user can actually see - and isn't asking for `Default`
///     on an enum.
///
/// The resulting analyzed APIs then feed both the `.cpp` codegen and the `.rs`
/// codegen.
pub struct IncludeCppEngine {
    config: IncludeCppConfig,
    state: State,
    source_code: Option<Rc<String>>, // so we can create diagnostics
}

impl Parse for IncludeCppEngine {
    fn parse(input: ParseStream) -> ParseResult<Self> {
        let config = input.parse::<IncludeCppConfig>()?;
        let state = if config.parse_only {
            State::ParseOnly
        } else {
            State::NotGenerated
        };
        Ok(Self {
            config,
            state,
            source_code: None,
        })
    }
}

impl IncludeCppEngine {
    pub fn new_from_syn(mac: Macro, file_contents: Rc<String>) -> Result<Self> {
        let mut this = mac
            .parse_body::<IncludeCppEngine>()
            .map_err(|e| Error::MacroParsing(LocatedSynError::new(e, &file_contents)))?;
        this.source_code = Some(file_contents);
        Ok(this)
    }

    /// Used if we find that we're asked to auto-discover extern_rust_type and similar
    /// but didn't have any include_cpp macro at all.
    pub fn new_for_autodiscover() -> Self {
        Self {
            config: IncludeCppConfig::default(),
            state: State::NotGenerated,
            source_code: None,
        }
    }

    pub fn config_mut(&mut self) -> &mut IncludeCppConfig {
        assert!(
            matches!(self.state, State::NotGenerated),
            "Can't alter config after generation commenced"
        );
        &mut self.config
    }

    fn build_header(&self) -> String {
        join(
            self.config
                .inclusions
                .iter()
                .map(|path| format!("#include \"{path}\"\n")),
            "",
        )
    }

    fn make_bindgen_builder(
        &self,
        inc_dirs: &[PathBuf],
        extra_clang_args: &[&str],
    ) -> bindgen::Builder {
        // The markers we ask bindgen for. Each is a fake type wrapping what it
        // marks, and every one of them is read off the syntax bindgen emits
        // and then stripped, so what they are declared *as* only has to keep
        // the bindgen mod compiling until then.
        //
        // `Const`, `Volatile` and `StdArray` are declared as transparent
        // aliases rather than newtypes, being the ones which land on types in
        // positions that also carry a value. `Volatile`'s own case is beside
        // the list below; the other two are these.
        //
        // A `const` variable's type sits beside the initializer bindgen emits
        // for it, and a `const` bitfield's accessors cast between the field
        // type and the allocation unit's integer (`self.x() as u32`). A
        // newtype makes both of those `error[E0605]: non-primitive cast`.
        //
        // A `std::array` is a POD struct's field, and a POD struct is emitted
        // for Rust to build and read: a newtype leaves the caller holding a
        // `__bindgen_marker_StdArray<[u8; 4]>` where the array is what they
        // wrote and what they compare against.
        //
        // An alias is invisible to all of it and just as visible to us, since
        // we match on the type bindgen printed.
        let bindgen_marker_newtypes = [
            "Opaque",
            "Reference",
            "RValueReference",
            "LongDouble",
            "Float128",
        ];
        // `Volatile` is an alias for the same reason `Const` is, and reaches
        // one of the same value-carrying positions: a `volatile` bitfield's
        // accessors cast between the field type and the allocation unit's
        // integer, so they return `__bindgen_marker_Volatile<u32>` out of a
        // `... as u32 as _`, which a newtype makes `error[E0605]`.
        //
        // `StdArray` is one for a value-carrying position of its own: it lands
        // on a POD struct's field, and a POD struct is emitted for Rust to
        // build and read, so a newtype leaves the caller holding a
        // `__bindgen_marker_StdArray<[u8; 4]>` where the array is what they
        // wrote and what they compare against.
        let bindgen_marker_aliases = ["Const", "Volatile", "StdArray"];
        let raw_line = bindgen_marker_newtypes
            .iter()
            .map(|t| {
                // `Default` because we ask bindgen to derive it for the
                // structs it generates, so that a struct containing bitfields
                // can be built in Rust at all (see google/autocxx#1478), and
                // bindgen assumes these types of ours are as default-able as
                // anything else it emits - which they are, whenever what they
                // wrap is. It has to be the `derive`, not a hand-written impl:
                // `codegen_rs::bindgen_sanitizer` strips every hand-written
                // `impl Default` out of the bindgen mod, because bindgen's own
                // are zero-filling and unsound for us.
                format!(
                    "#[repr(transparent)] #[derive(Default)] \
                     pub struct __bindgen_marker_{t}<T: ?Sized>(T);"
                )
            })
            .chain(
                bindgen_marker_aliases
                    .iter()
                    .map(|t| format!("pub type __bindgen_marker_{t}<T> = T;")),
            )
            .join(" ");
        let bindgen_marker_types: Vec<_> = bindgen_marker_newtypes
            .iter()
            .chain(bindgen_marker_aliases.iter())
            .collect();
        let use_list = bindgen_marker_types
            .iter()
            .map(|t| format!("__bindgen_marker_{t}"))
            .join(", ");
        // bindgen names each C++ character type with a name of its own
        // invention rather than an integer; these bind those names to the
        // newtypes which represent them. `parse_bindgen` knows to skip them
        // rather than read them back as typedefs.
        let char_type_uses = known_types::CXX_CHARACTER_TYPES
            .iter()
            .map(|(_, bindgen_name, rs_name)| {
                format!("#[allow(unused_imports)] use {rs_name} as {bindgen_name};")
            })
            .join(" ");
        let all_module_raw_line =
            format!("#[allow(unused_imports)] use super::{{{use_list}}}; {char_type_uses}");

        let mut builder = bindgen::builder()
            .clang_args(make_clang_args(inc_dirs, extra_clang_args))
            .derive_copy(false)
            .derive_debug(false)
            // A struct with bitfields can only be built in Rust by filling in
            // the opaque allocation unit bindgen generates for them, which is
            // what `..Default::default()` is for. See google/autocxx#1478.
            .derive_default(true)
            .default_enum_style(bindgen::EnumVariation::Rust {
                non_exhaustive: false,
            })
            .formatter(if log::log_enabled!(log::Level::Info) {
                bindgen::Formatter::Rustfmt
            } else {
                bindgen::Formatter::None
            })
            .size_t_is_usize(true)
            .enable_cxx_namespaces()
            .generate_inline_functions(true)
            .respect_cxx_access_specs(true)
            .use_specific_virtual_function_receiver(true)
            .use_opaque_newtype_wrapper(true)
            .use_reference_newtype_wrapper(true)
            .use_const_newtype_wrapper(true)
            .use_volatile_newtype_wrapper(true)
            .use_long_double_newtype_wrapper(true)
            .use_float128_newtype_wrapper(true)
            .represent_cxx_operators(true)
            .represent_std_array(true)
            .use_std_array_newtype_wrapper(true)
            .use_distinct_char16_t(true)
            .use_distinct_wchar_t(true)
            .use_distinct_char32_t(true)
            .use_distinct_char8_t(true)
            .generate_deleted_functions(true)
            .generate_pure_virtual_functions(true)
            .raw_line(raw_line)
            .every_module_raw_line(all_module_raw_line)
            .generate_private_functions(true)
            .dependent_qualified_types(true)
            // Off, and staying off. Turning them on was tried in fork PR #72
            // (CI run 34049300179), where every test and examples leg failed,
            // for two reasons.
            //
            // bindgen writes each assertion as `const _: () = { ... }`, and an
            // item named `_` used to crash the error-stub generator. That part
            // is fixed - see `ErrorContextType::is_declarable` - but it was
            // never the interesting failure.
            //
            // The interesting one is that the assertions are correct and fire
            // anyway, on the types autocxx hands bindgen a substitute for.
            // `known_types` feeds bindgen a `replaces=` prelude in which
            // `std::vector` and `std::string` are one pointer each; bindgen
            // measures the real C++ type - 24 bytes for a `std::vector<int>` -
            // and Rust is left holding the 8-byte stand-in, as is every type
            // with such a member. The assertions worth having are the ones on
            // everything else, because a non-POD type's Rust representation is
            // `#[repr(transparent)]` around the bindgen struct, so bindgen's
            // layout is the layout Rust uses for it and a wrong one is the
            // unsoundness `codegen_rs::non_pod_struct` warns about. bindgen
            // skips an assertion only for types it decided were opaque itself,
            // and `layout_tests` is one global switch, so there is no asking
            // for the second set without the first.
            .layout_tests(false)
            // The member functions of a class template, which bindgen
            // otherwise discards while parsing. They are the only description
            // of what a concrete instantiation can be asked to do: bindgen
            // generates nothing for a specialization, so autocxx writes its own
            // shims from these. See google/autocxx#723.
            .report_template_member_functions(true)
            // The member function templates of any class, which bindgen
            // otherwise does not parse as members at all. autocxx cannot bind
            // one - calling it means choosing its template arguments - so what
            // this buys is the note which says the member is there, in place of
            // silence. See google/autocxx#109.
            .report_member_function_templates(true);

        // 3. Passes allowlist and other options to the bindgen::Builder equivalent
        //    to --output-style=cxx --allowlist=<as passed in>
        if let Some(allowlist) = self.config.bindgen_allowlist() {
            for a in allowlist {
                // One name, every kind of item bearing it. That is what
                // `generate!` means: the book tells users to write one for
                // "every *type* or *function*" they want, and never to say
                // which. Nor could they - `AllowlistEntry` distinguishes an
                // item from a namespace and nothing else, so the kind of an
                // item is not something the directive language can carry.
                // Saying it would take new syntax (`generate_type!`,
                // `generate_function!`) or a discovery pass to tell the user
                // what kinds exist under a name before they choose.
                //
                // Decision: allowlisting stays coarse. What that costs is that
                // where C++ really does declare more than one thing under one
                // name - `struct stat` and a `stat` variable, say - the
                // directive asks for all of them, and the user has no way to
                // say which they meant. `ApiVec::push` settles it instead: the
                // type keeps the name, a function of that name is refiled as a
                // stub under an invented one, and a variable of it is dropped.
                builder = builder
                    .allowlist_type(&a)
                    .allowlist_function(&a)
                    .allowlist_function(format!("{a}_bindgen_original"))
                    .allowlist_var(&a);
            }
        }

        // Per-enum overrides of `default_enum_style` above, from `enum_style!`.
        for (name, style) in self.config.enum_styles() {
            builder = match style {
                EnumStyle::BitfieldEnum => builder.bitfield_enum(name),
                EnumStyle::NewtypeEnum => builder.newtype_enum(name),
                EnumStyle::RustifiedEnum => builder.rustified_enum(name),
                EnumStyle::RustifiedNonExhaustiveEnum => {
                    builder.rustified_non_exhaustive_enum(name)
                }
            };
        }

        for item in &self.config.opaquelist {
            builder = builder.opaque_type(item);
        }

        // Make C++ standard library implementation details opaque.
        // Names such as std::__tree (libc++) or std::_Rb_tree
        // (libstdc++) are reserved implementation-detail names which
        // bindgen descends into when a user type holds a std container
        // member; with newer standard library headers this produces
        // uncompilable bindings (unresolved template parameters such
        // as _CharT or _Hashtable). See google/autocxx#1491 and
        // google/autocxx#1480. Opaque types keep the correct layout
        // (a [u8; N] blob), so containing structs still work. This is
        // deliberately scoped to reserved names: ordinary std types
        // and user types are unaffected, so the implicit-constructor
        // analysis discussed below still sees the fields it needs.
        for pattern in [
            // libc++ details, e.g. std::__tree, std::__hash_table
            "std::__[a-zA-Z0-9_]+.*",
            // libstdc++ and MSVC STL details, e.g. std::_Rb_tree, std::_Tree
            "std::_[A-Z].*",
            "__gnu_cxx::.*",
        ] {
            builder = builder.opaque_type(pattern);
        }

        // At this point it woul be great to use `Builder::opaque_type` for
        // everything which is on the allowlist but not on the POD list.
        // This would free us from a large proportion of bindgen bugs which
        // are dealing with obscure templated types. Unfortunately, even
        // for types which we expose to the user as opaque (non-POD), autocxx
        // internally still cares about seeing what fields they've got because
        // we make decisions about implicit constructors on that basis.
        // So, for now, we can't do that. Perhaps in future bindgen could
        // gain an option to generate any implicit constructors, if that
        // information is exposed by clang. That would remove a lot of
        // autocxx complexity and would allow us to request opaque types.

        log::info!(
            "Bindgen flags would be: {}",
            builder
                .command_line_flags()
                .into_iter()
                .map(|f| format!("\"{f}\""))
                .join(" ")
        );
        builder
    }

    pub fn get_rs_filename(&self) -> String {
        self.config.get_rs_filename()
    }

    /// Generate the Rust bindings. Call `generate` first.
    pub fn get_rs_output(&self) -> RsOutput<'_> {
        RsOutput {
            config: &self.config,
            rs: match &self.state {
                State::NotGenerated => panic!("Generate first"),
                State::Generated(gen_results) => Some(&gen_results.item_mod),
                State::ParseOnly => None,
            },
        }
    }

    /// Returns the name of the mod which this `include_cpp!` will generate.
    /// Can and should be used to ensure multiple mods in a file don't conflict.
    pub fn get_mod_name(&self) -> String {
        self.config.get_mod_name().to_string()
    }

    fn parse_bindings(&self, bindings: bindgen::Bindings) -> Result<ItemMod> {
        // This bindings object is actually a TokenStream internally and we're wasting
        // effort converting to and from string. We could enhance the bindgen API
        // in future.
        let bindings = bindings.to_string();
        // Manually add the mod ffi {} so that we can ask syn to parse
        // into a single construct.
        let bindings = format!("mod bindgen {{ {bindings} }}");
        info!("Bindings: {}", bindings);
        syn::parse_str::<ItemMod>(&bindings)
            .map_err(|e| Error::BindingsParsing(LocatedSynError::new(e, &bindings)))
    }

    /// Actually examine the headers to find out what needs generating.
    /// Most errors occur at this stage as we fail to interpret the C++
    /// headers properly.
    ///
    /// See documentation for this type for flow diagrams and more details.
    pub fn generate(
        &mut self,
        inc_dirs: Vec<PathBuf>,
        extra_clang_args: &[&str],
        dep_recorder: Option<Box<dyn RebuildDependencyRecorder>>,
        codegen_options: &CodegenOptions,
    ) -> Result<()> {
        // If we are in parse only mode, do nothing. This is used for
        // doc tests to ensure the parsing is valid, but we can't expect
        // valid C++ header files or linkers to allow a complete build.
        match self.state {
            State::ParseOnly => return Ok(()),
            State::NotGenerated => {}
            State::Generated(_) => panic!("Only call generate once"),
        }

        if matches!(
            self.config.unsafe_policy,
            UnsafePolicy::ReferencesWrappedAllFunctionsSafe
        ) && !rustversion::cfg!(nightly)
        {
            return Err(Error::WrappedReferencesButNoArbitrarySelfTypes);
        }

        let parse_callback_results =
            Rc::new(RefCell::new(UnindexedParseCallbackResults::default()));
        let mod_name = self.config.get_mod_name();
        let mut builder = self
            .make_bindgen_builder(&inc_dirs, extra_clang_args)
            .parse_callbacks(Box::new(AutocxxParseCallbacks::new(
                dep_recorder,
                parse_callback_results.clone(),
            )));
        let header_contents = self.build_header();
        self.dump_header_if_so_configured(&header_contents, &inc_dirs, extra_clang_args);
        let header_and_prelude = format!("{}\n\n{}", known_types().get_prelude(), header_contents);
        log::info!("Header and prelude for bindgen:\n{}", header_and_prelude);
        builder = builder.header_contents("example.hpp", &header_and_prelude);

        let bindings = builder.generate().map_err(Error::Bindgen)?;
        let bindings = self.parse_bindings(bindings)?;
        let parse_callback_results = parse_callback_results.take();
        log::info!("Parse callback results: {:?}", parse_callback_results);

        // Source code contents just used for diagnostics - if we don't have it,
        // use a blank string and miette will not attempt to annotate it nicely.
        let source_file_contents = self
            .source_code
            .as_ref()
            .cloned()
            .unwrap_or_else(|| Rc::new("".to_string()));

        let converter = BridgeConverter::new(
            &self.config.inclusions,
            &self.config,
            clang_target::expected_wchar_t_size(extra_clang_args),
        );

        let conversion = converter
            .convert(
                bindings,
                parse_callback_results.index(
                    cpp_standard::exception_specifications_are_part_of_the_type(extra_clang_args),
                ),
                self.config.unsafe_policy.clone(),
                header_contents,
                codegen_options,
                &source_file_contents,
            )
            .map_err(Error::Conversion)?;
        let items = conversion.rs;
        let new_bindings: ItemMod = parse_quote! {
            #[allow(non_snake_case)]
            #[allow(dead_code)]
            #[allow(non_upper_case_globals)]
            #[allow(non_camel_case_types)]
            #[doc = "Generated using autocxx - do not edit directly"]
            #[doc = "@generated"]
            mod #mod_name {
                #(#items)*
            }
        };
        info!(
            "New bindings:\n{}",
            rust_pretty_printer::pretty_print(&new_bindings)
        );
        self.state = State::Generated(Box::new(GenerationResults {
            item_mod: new_bindings,
            cpp: conversion.cpp,
            inc_dirs,
            cxxgen_header_name: conversion.cxxgen_header_name,
        }));
        Ok(())
    }

    /// Return the include directories used for this include_cpp invocation.
    #[cfg(any(test, feature = "build"))]
    fn include_dirs(&self) -> impl Iterator<Item = &PathBuf> {
        match &self.state {
            State::Generated(gen_results) => gen_results.inc_dirs.iter(),
            _ => panic!("Must call generate() before include_dirs()"),
        }
    }

    fn dump_header_if_so_configured(
        &self,
        header: &str,
        inc_dirs: &[PathBuf],
        extra_clang_args: &[&str],
    ) {
        if let Ok(output_path) = std::env::var("AUTOCXX_PREPROCESS") {
            self.make_preprocessed_file(
                &PathBuf::from(output_path),
                header,
                inc_dirs,
                extra_clang_args,
            );
        }
        #[cfg(feature = "reproduction_case")]
        if let Ok(output_path) = std::env::var("AUTOCXX_REPRO_CASE") {
            let tf = NamedTempFile::new().unwrap();
            self.make_preprocessed_file(
                &PathBuf::from(tf.path()),
                header,
                inc_dirs,
                extra_clang_args,
            );
            let header = std::fs::read(tf.path()).unwrap();
            let header = String::from_utf8_lossy(&header);
            let output_path = PathBuf::from(output_path);
            let config = self.config.to_token_stream().to_string();
            let json = serde_json::json!({
                "header": header,
                "config": config
            });
            let f = File::create(output_path).unwrap();
            serde_json::to_writer(f, &json).unwrap();
        }
    }

    fn make_preprocessed_file(
        &self,
        output_path: &Path,
        header: &str,
        inc_dirs: &[PathBuf],
        extra_clang_args: &[&str],
    ) {
        // Include a load of system headers at the end of the preprocessed output,
        // because we would like to be able to generate bindings from the
        // preprocessed header, and then build those bindings. The C++ parts
        // of those bindings might need things inside these various headers;
        // we make sure all these definitions and declarations are inside
        // this one header file so that the reduction process does not have
        // to refer to local headers on the reduction machine too.
        let suffix = ALL_KNOWN_SYSTEM_HEADERS
            .iter()
            .map(|hdr| format!("#include <{hdr}>\n"))
            .join("\n");
        let input = format!("/*\nautocxx config:\n\n{:?}\n\nend autocxx config.\nautocxx preprocessed input:\n*/\n\n{}\n\n/* autocxx: extra headers added below for completeness. */\n\n{}\n{}\n",
            self.config, header, suffix, cxx_gen::HEADER);
        let mut tf = NamedTempFile::new().unwrap();
        write!(tf, "{input}").unwrap();
        let tp = tf.into_temp_path();
        preprocess(&tp, &PathBuf::from(output_path), inc_dirs, extra_clang_args).unwrap();
    }
}

/// This is a list of all the headers known to be included in generated
/// C++ by cxx. We only use this when `AUTOCXX_PERPROCESS` is set to true,
/// in an attempt to make the resulting preprocessed header more hermetic.
/// We clearly should _not_ use this in any other circumstance; obviously
/// we'd then want to add an API to cxx_gen such that we could retrieve
/// that information from source.
static ALL_KNOWN_SYSTEM_HEADERS: &[&str] = &[
    "memory",
    "string",
    "algorithm",
    "array",
    "cassert",
    "cstddef",
    "cstdint",
    "cstring",
    "exception",
    "functional",
    "initializer_list",
    "iterator",
    "memory",
    "new",
    "stdexcept",
    "type_traits",
    "utility",
    "vector",
    "sys/types.h",
];

pub fn do_cxx_cpp_generation(
    rs: TokenStream2,
    cpp_codegen_options: &CppCodegenOptions,
    cxxgen_header_name: String,
) -> Result<CppFilePair, cxx_gen::Error> {
    let mut opt = cxx_gen::Opt::default();
    opt.cxx_impl_annotations
        .clone_from(&cpp_codegen_options.cxx_impl_annotations);
    let cxx_generated = cxx_gen::generate_header_and_cc(rs, &opt)?;
    Ok(CppFilePair {
        header: strip_system_headers(
            cxx_generated.header,
            cpp_codegen_options.suppress_system_headers,
        ),
        header_name: cxxgen_header_name,
        implementation: Some(strip_system_headers(
            cxx_generated.implementation,
            cpp_codegen_options.suppress_system_headers,
        )),
    })
}

pub fn get_cxx_header_bytes(suppress_system_headers: bool) -> Vec<u8> {
    strip_system_headers(cxx_gen::HEADER.as_bytes().to_vec(), suppress_system_headers)
}

fn strip_system_headers(input: Vec<u8>, suppress_system_headers: bool) -> Vec<u8> {
    if suppress_system_headers {
        std::str::from_utf8(&input)
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with("#include <"))
            .join("\n")
            .as_bytes()
            .to_vec()
    } else {
        input
    }
}

impl CppBuildable for IncludeCppEngine {
    /// Generate C++-side bindings for these APIs. Call `generate` first.
    fn generate_h_and_cxx(
        &self,
        cpp_codegen_options: &CppCodegenOptions,
    ) -> Result<GeneratedCpp, cxx_gen::Error> {
        let mut files = Vec::new();
        match &self.state {
            State::ParseOnly => panic!("Cannot generate C++ in parse-only mode"),
            State::NotGenerated => panic!("Call generate() first"),
            State::Generated(gen_results) => {
                let rs = gen_results.item_mod.to_token_stream();
                files.push(do_cxx_cpp_generation(
                    rs,
                    cpp_codegen_options,
                    gen_results.cxxgen_header_name.clone(),
                )?);
                if let Some(cpp_file_pair) = &gen_results.cpp {
                    files.push(cpp_file_pair.clone());
                }
            }
        };
        Ok(GeneratedCpp(files))
    }
}

/// Get clang args as if we were operating clang the same way as we operate
/// bindgen.
pub fn make_clang_args<'a>(
    incs: &'a [PathBuf],
    extra_args: &'a [&str],
) -> impl Iterator<Item = String> + 'a {
    // Which target to parse for, where clang would otherwise guess wrong -
    // see `clang_target`. Nothing here when the caller has already said.
    let target_arg = clang_target::extra_clang_target_arg(extra_args);
    // AUTOCXX_CLANG_ARGS come first so that any defaults defined there(e.g. for the `-std`
    // argument) can be overridden by extra_args.
    AUTOCXX_CLANG_ARGS
        .iter()
        .map(|s| s.to_string())
        .chain(target_arg)
        .chain(incs.iter().map(|i| format!("-I{}", i.to_str().unwrap())))
        .chain(extra_args.iter().map(|s| s.to_string()))
}

/// Preprocess a file using the same options
/// as is used by autocxx. Input: listing_path, output: preprocess_path.
pub fn preprocess(
    listing_path: &Path,
    preprocess_path: &Path,
    incs: &[PathBuf],
    extra_clang_args: &[&str],
) -> Result<(), std::io::Error> {
    let mut cmd = Command::new(get_clang_path());
    cmd.arg("-E");
    cmd.arg("-C");
    cmd.args(make_clang_args(incs, extra_clang_args));
    cmd.arg(listing_path.to_str().unwrap());
    cmd.stderr(Stdio::inherit());
    let result = cmd.output().expect("failed to execute clang++");
    assert!(result.status.success(), "failed to preprocess");
    let mut file = File::create(preprocess_path)?;
    file.write_all(&result.stdout)?;
    Ok(())
}

/// Get the path to clang which is effective for any preprocessing
/// operations done by autocxx.
pub fn get_clang_path() -> String {
    // `CLANG_PATH` is the environment variable that clang-sys uses to specify
    // the path to Clang, so in most cases where someone is using a compiler
    // that's not on the path, things should just work. We also check `CXX`,
    // since some users may have set that.
    std::env::var("CLANG_PATH")
        .or_else(|_| std::env::var("CXX"))
        .unwrap_or_else(|_| "clang++".to_string())
}

/// Function to generate the desired name of the header containing autocxx's
/// extra generated C++.
/// Newtype wrapper so we can give it a [`Default`].
pub struct AutocxxgenHeaderNamer<'a>(pub Box<dyn 'a + Fn(String) -> String>);

impl Default for AutocxxgenHeaderNamer<'static> {
    fn default() -> Self {
        Self(Box::new(|mod_name| format!("autocxxgen_{mod_name}.h")))
    }
}

impl AutocxxgenHeaderNamer<'_> {
    fn name_header(&self, mod_name: String) -> String {
        self.0(mod_name)
    }
}

/// Function to generate the desired name of the header containing cxx's
/// declarations.
/// Newtype wrapper so we can give it a [`Default`].
pub struct CxxgenHeaderNamer<'a>(pub Box<dyn 'a + Fn() -> String>);

impl Default for CxxgenHeaderNamer<'static> {
    fn default() -> Self {
        // The default implementation here is to name these headers
        // cxxgen.h, cxxgen1.h, cxxgen2.h etc.
        // These names are not especially predictable by callers and this
        // behavior is not tested anywhere - so this is considered semi-
        // supported, at best. This only comes into play in the rare case
        // that you're generating bindings to multiple include_cpp!
        // or a mix of include_cpp! and #[cxx::bridge] bindings.
        let header_counter = Rc::new(RefCell::new(0));
        Self(Box::new(move || {
            let header_counter = header_counter.clone();
            let header_counter_cell = header_counter.as_ref();
            let mut header_counter = header_counter_cell.borrow_mut();
            if *header_counter == 0 {
                *header_counter += 1;
                "cxxgen.h".into()
            } else {
                let count = *header_counter;
                *header_counter += 1;
                format!("cxxgen{count}.h")
            }
        }))
    }
}

impl CxxgenHeaderNamer<'_> {
    fn name_header(&self) -> String {
        self.0()
    }
}

/// Options for C++ codegen
#[derive(Default)]
pub struct CppCodegenOptions<'a> {
    /// Whether to avoid generating `#include <some-system-header>`.
    /// You may wish to do this to make a hermetic test case with no
    /// external dependencies.
    pub suppress_system_headers: bool,
    /// Optionally, a prefix to go at `#include "*here*cxx.h". This is a header file from the `cxx`
    /// crate.
    pub path_to_cxx_h: Option<String>,
    /// Optionally, a prefix to go at `#include "*here*cxxgen.h". This is a header file which we
    /// generate.
    pub path_to_cxxgen_h: Option<String>,
    /// Optionally, a function called to determine the name that will be used
    /// for the autocxxgen.h file.
    /// The function is passed the name of the module generated by each `include_cpp`,
    /// configured via `name`. These will be unique.
    pub autocxxgen_header_namer: AutocxxgenHeaderNamer<'a>,
    /// A function to generate the name of the cxxgen.h header that should be output.
    pub cxxgen_header_namer: CxxgenHeaderNamer<'a>,
    /// An annotation optionally to include on each C++ function.
    /// For example to export the symbol from a library.
    pub cxx_impl_annotations: Option<String>,
}

fn proc_macro_span_to_miette_span(span: &proc_macro2::Span) -> SourceSpan {
    // A proc_macro2::Span stores its location as a byte offset. But there are
    // no APIs to get that offset out.
    // We could use `.start()` and `.end()` to get the line + column numbers, but it appears
    // they're a little buggy. Hence we do this, to get the offsets directly across into
    // miette.
    struct Err;
    let r: Result<(usize, usize), Err> = (|| {
        let span_desc = format!("{span:?}");
        let re = Regex::new(r"(\d+)..(\d+)").unwrap();
        let captures = re.captures(&span_desc).ok_or(Err)?;
        let start = captures.get(1).ok_or(Err)?;
        let start: usize = start.as_str().parse().map_err(|_| Err)?;
        let start = start.saturating_sub(1); // proc_macro::Span offsets seem to be off-by-one
        let end = captures.get(2).ok_or(Err)?;
        let end: usize = end.as_str().parse().map_err(|_| Err)?;
        let end = end.saturating_sub(1); // proc_macro::Span offsets seem to be off-by-one
        Ok((start, end.saturating_sub(start)))
    })();
    let (start, end) = r.unwrap_or((0, 0));
    SourceSpan::new(SourceOffset::from(start), SourceOffset::from(end))
}

#[cfg(test)]
mod dependent_qualified_type_tests {
    //! The vendored bindgen can give a member whose type is named through a
    //! template parameter - `typename T::Inner` - that type, as an associated
    //! type of a generated trait, instead of an opaque blob. autocxx does not
    //! ask for it yet (see `dependent_qualified_types` for what still stands in
    //! the way), so these are what exercise it.

    use super::bindgen;

    const HDR: &str = "
        namespace ns {
        struct Inner { typedef int related_type; };
        template <typename T> class Container {
        public:
            typename T::related_type contents_;
            const typename T::related_type* ptr_;
        };
        typedef Container<Inner> Concrete;
        template <typename T> class ByCallback {
        public:
            void (*cb)(typename T::related_type);
        };
        }
    ";

    fn generate(dependent_qualified_types: bool) -> String {
        bindgen::builder()
            .header_contents("test.hpp", HDR)
            .clang_args(["-x", "c++", "-std=c++14"])
            .enable_cxx_namespaces()
            .formatter(bindgen::Formatter::None)
            .dependent_qualified_types(dependent_qualified_types)
            .generate()
            .expect("bindgen cannot parse the test header")
            .to_string()
    }

    #[test]
    fn dependent_qualified_type_becomes_an_associated_type() {
        let rs = generate(true);
        // The trait is declared in the root module, which is where the
        // dependent qualified types needing it are parented, and named from
        // `ns` through that module.
        assert!(
            rs.contains("pub trait __bindgen_has_inner_type_related_type"),
            "{rs}"
        );
        assert!(
            rs.contains("impl root :: __bindgen_has_inner_type_related_type for Inner"),
            "{rs}"
        );
        assert!(
            rs.contains("where T : root :: __bindgen_has_inner_type_related_type"),
            "{rs}"
        );
        assert!(
            rs.contains("< T as root :: __bindgen_has_inner_type_related_type > :: related_type"),
            "{rs}"
        );
        // Through a pointer, and through the `const` which the spelling
        // libclang gives such a type carries.
        assert!(
            rs.contains("pub ptr_ : * const < T as root :: __bindgen"),
            "{rs}"
        );
        // And through a function signature, which is the other way a member
        // can name one without naming it directly.
        assert!(
            rs.contains("pub struct ByCallback < T , > where T : root :: __bindgen"),
            "{rs}"
        );
        // The parameter is used, so it is not discarded and the instantiation
        // is of the template rather than of a blob.
        assert!(
            rs.contains("pub type Concrete = root :: ns :: Container <"),
            "{rs}"
        );
    }

    #[test]
    fn off_by_default_leaves_the_blob() {
        let rs = generate(false);
        assert!(!rs.contains("__bindgen_has_inner_type"), "{rs}");
    }
}
