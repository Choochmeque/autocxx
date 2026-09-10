// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;
use std::borrow::Cow;
use std::hash::Hash;

use itertools::Itertools;
use proc_macro2::Span;
use quote::ToTokens;

#[cfg(feature = "reproduction_case")]
use quote::format_ident;
use syn::{
    parse::{Parse, ParseStream},
    Signature, Token, TypePath,
};
use syn::{Ident, Result as ParseResult};
use thiserror::Error;

use crate::derives::DeriveMap;
use crate::enum_style::{EnumStyle, EnumStyleMap};
use crate::stable_hash::stable_hash;
use crate::{directives::get_directives, RustPath};

use quote::quote;

#[derive(PartialEq, Eq, Clone, Debug, Hash, Default)]
pub enum UnsafePolicy {
    AllFunctionsSafe,
    #[default]
    AllFunctionsUnsafe,
    ReferencesWrappedAllFunctionsSafe,
}

impl Parse for UnsafePolicy {
    fn parse(input: ParseStream) -> ParseResult<Self> {
        if input.parse::<Option<Token![unsafe]>>()?.is_some() {
            return Ok(UnsafePolicy::AllFunctionsSafe);
        }
        let r = match input.parse::<Option<syn::Ident>>()? {
            Some(id) => {
                if id == "unsafe_ffi" {
                    Ok(UnsafePolicy::AllFunctionsSafe)
                } else if id == "unsafe_references_wrapped" {
                    Ok(UnsafePolicy::ReferencesWrappedAllFunctionsSafe)
                } else {
                    Err(syn::Error::new(
                        id.span(),
                        "expected unsafe_ffi or unsafe_references_wrapped",
                    ))
                }
            }
            None => Ok(UnsafePolicy::AllFunctionsUnsafe),
        };
        if !input.is_empty() {
            return Err(syn::Error::new(
                Span::call_site(),
                "unexpected tokens within safety directive",
            ));
        }
        r
    }
}

impl ToTokens for UnsafePolicy {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        if *self == UnsafePolicy::AllFunctionsSafe {
            tokens.extend(quote! { unsafe })
        } else if *self == UnsafePolicy::ReferencesWrappedAllFunctionsSafe {
            tokens.extend(quote! { unsafe_references_wrapped })
        }
    }
}

impl UnsafePolicy {
    /// Whether we are treating C++ references as a different thing from Rust
    /// references and therefore have to generate lots of code for a CppRef type
    pub fn requires_cpprefs(&self) -> bool {
        matches!(self, Self::ReferencesWrappedAllFunctionsSafe)
    }
}

/// An entry in the allowlist.
#[derive(Hash, Debug)]
pub enum AllowlistEntry {
    Item(String),
    Namespace(String),
}

impl AllowlistEntry {
    /// The names to give bindgen for this entry. A namespace has just the one;
    /// a named item may have several, because we can't tell which `::` in what
    /// the user wrote separates namespaces from nesting - see
    /// [`bindgen_spellings`].
    fn to_bindgen_items(&self) -> Box<dyn Iterator<Item = String>> {
        match self {
            AllowlistEntry::Item(i) => Box::new(bindgen_spellings(i.clone())),
            AllowlistEntry::Namespace(ns) => Box::new(std::iter::once(format!("{ns}::.*"))),
        }
    }
}

/// Every name bindgen might know an item by, given the name the user wrote.
///
/// bindgen flattens nesting, so a `struct Inner` inside a `struct Outer` in
/// namespace `ns` is `ns::Outer_Inner` to bindgen but `ns::Outer::Inner` to
/// C++ and to the person writing the directive. We can't tell which `::` in
/// what they wrote separates namespaces from nesting, so we offer bindgen
/// every split: for `a::b::c` that is `a::b::c`, `a::b_c` and `a_b_c`. Only
/// one can match anything, and an allowlist entry which matches nothing costs
/// nothing. See google/autocxx#1422.
fn bindgen_spellings(item: String) -> impl Iterator<Item = String> {
    let segments: Vec<String> = item.split("::").map(|s| s.to_string()).collect();
    // Split point `k` treats everything from segment `k` on as one nested
    // type name, so those segments join with `_` and the namespaces before
    // them keep their `::`. `k == segments.len() - 1` reproduces what the
    // user wrote.
    (0..segments.len()).rev().map(move |k| {
        let nested = segments[k..].join("_");
        if k == 0 {
            nested
        } else {
            format!("{}::{}", segments[..k].join("::"), nested)
        }
    })
}

/// Whether the name autocxx knows a type by is one a directive naming
/// `directive` claims.
///
/// bindgen flattens a nested class into its enclosing one, so C++'s
/// `ns::Outer::MyPtr` is `ns::Outer_MyPtr`, and which `::` in what the user
/// wrote separates namespaces from nesting is not something this can know -
/// so every split is tried, exactly as an allowlist entry offers bindgen every
/// split.
///
/// A name may also be written without its namespaces, which is the latitude
/// `throws!` gives and carries the same cost: a short name claims every
/// template of that name, in every namespace.
pub fn name_matches_directive(cpp_name: &str, directive: &str) -> bool {
    // Both sides are split, not just the directive: the name a template is
    // known by is bindgen's where a conversion asks, and C++'s where it was
    // rebuilt from a `concrete!` expression, and a directive written either way
    // means the same template.
    let candidates: Vec<String> = bindgen_spellings(cpp_name.to_string()).collect();
    bindgen_spellings(directive.to_string()).any(|spelling| {
        candidates.iter().any(|candidate| {
            *candidate == spelling || candidate.ends_with(&format!("::{spelling}"))
        })
    })
}

/// Allowlist configuration.
#[derive(Hash, Debug)]
pub enum Allowlist {
    Unspecified(Vec<AllowlistEntry>),
    All,
    Specific(Vec<AllowlistEntry>),
}

/// Errors that may be encountered while adding allowlist entries.
#[derive(Error, Debug)]
pub enum AllowlistErr {
    #[error("Conflict between generate/generate_ns! and generate_all! - use one not both")]
    ConflictingGenerateAndGenerateAll,
}

impl Allowlist {
    pub fn push(&mut self, item: AllowlistEntry) -> Result<(), AllowlistErr> {
        match self {
            Allowlist::Unspecified(ref mut uncommitted_list) => {
                let new_list = uncommitted_list
                    .drain(..)
                    .chain(std::iter::once(item))
                    .collect();
                *self = Allowlist::Specific(new_list);
            }
            Allowlist::All => {
                return Err(AllowlistErr::ConflictingGenerateAndGenerateAll);
            }
            Allowlist::Specific(list) => list.push(item),
        };
        Ok(())
    }

    pub(crate) fn set_all(&mut self) -> Result<(), AllowlistErr> {
        if matches!(self, Allowlist::Specific(..)) {
            return Err(AllowlistErr::ConflictingGenerateAndGenerateAll);
        }
        *self = Allowlist::All;
        Ok(())
    }
}

#[allow(clippy::derivable_impls)] // nightly-only
impl Default for Allowlist {
    fn default() -> Self {
        Allowlist::Unspecified(Vec::new())
    }
}

#[derive(Debug, Hash)]
pub struct Subclass {
    pub superclass: String,
    pub subclass: Ident,
}

#[derive(Clone, Hash)]
pub struct RustFun {
    pub path: RustPath,
    pub sig: Signature,
    pub has_receiver: bool,
}

impl std::fmt::Debug for RustFun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustFun")
            .field("path", &self.path)
            .field("sig", &self.sig.to_token_stream().to_string())
            .finish()
    }
}

#[derive(Debug, Clone, Hash)]
pub struct ExternCppType {
    pub rust_path: TypePath,
    pub opaque: bool,
}

/// Newtype wrapper so we can implement Hash.
#[derive(Debug, Default)]
pub struct ExternCppTypeMap(pub HashMap<String, ExternCppType>);

impl std::hash::Hash for ExternCppTypeMap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for (k, v) in &self.0 {
            k.hash(state);
            v.hash(state);
        }
    }
}

/// Newtype wrapper so we can implement Hash.
#[derive(Debug, Default)]
pub struct ConcretesMap(pub HashMap<String, Ident>);

impl std::hash::Hash for ConcretesMap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for (k, v) in &self.0 {
            k.hash(state);
            v.hash(state);
        }
    }
}

/// The key an `include_cpp!` block's generated bindings are filed under in a
/// JSON archive (see [`crate::MultiBindings`]).
///
/// The codegen phase and the macro phase are separate processes which never
/// share a config: the codegen augments its copy - `confirm_complete`, plus
/// every subclass, `extern_rust_function` and `--auto-allowlist` use it
/// discovers in the file - while the macro sees only what the user wrote.
/// Both must therefore key on the block *as written*, so the hash is taken
/// when the block is parsed, before anything can augment it, and carried from
/// there: `IncludeCpp::config_hash` on the macro side and
/// `IncludeCppEngine::config_hash` on the codegen side. Hashing a config after
/// something has augmented it gives a key nothing looks up.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ConfigHash(pub(crate) u64);

#[derive(Debug, Default, Hash)]
pub struct IncludeCppConfig {
    pub inclusions: Vec<String>,
    pub unsafe_policy: UnsafePolicy,
    pub parse_only: bool,
    pub exclude_impls: bool,
    pub prettify: bool,
    pub(crate) pod_requests: Vec<String>,
    pub allowlist: Allowlist,
    pub(crate) blocklist: Vec<String>,
    pub(crate) constructor_blocklist: Vec<String>,
    pub instantiable: Vec<String>,
    /// The class templates a `smart_pointer!` directive says hold a pointer to
    /// their argument, named as C++ names them. Every instantiation of one gets
    /// an accessor for what it points at.
    pub smart_pointers: Vec<String>,
    pub(crate) exclude_utilities: bool,
    pub(crate) mod_name: Option<Ident>,
    pub rust_types: Vec<RustPath>,
    pub subclasses: Vec<Subclass>,
    pub extern_rust_funs: Vec<RustFun>,
    pub concretes: ConcretesMap,
    pub externs: ExternCppTypeMap,
    pub opaquelist: Vec<String>,
    pub(crate) throws_list: Vec<String>,
    pub(crate) enum_styles: EnumStyleMap,
    pub(crate) derives: DeriveMap,
}

impl Parse for IncludeCppConfig {
    fn parse(input: ParseStream) -> ParseResult<Self> {
        let mut config = IncludeCppConfig::default();

        while !input.is_empty() {
            let has_hexathorpe = input.parse::<Option<syn::token::Pound>>()?.is_some();
            let ident: syn::Ident = input.parse()?;
            let args;
            let (possible_directives, to_parse, parse_completely) = if has_hexathorpe {
                (&get_directives().need_hexathorpe, input, false)
            } else {
                input.parse::<Option<syn::token::Not>>()?;
                syn::parenthesized!(args in input);
                (&get_directives().need_exclamation, &args, true)
            };
            let all_possible = possible_directives.keys().join(", ");
            let ident_str = ident.to_string();
            match possible_directives.get(&ident_str) {
                None => {
                    return Err(syn::Error::new(
                        ident.span(),
                        format!("expected {all_possible}"),
                    ));
                }
                Some(directive) => directive.parse(to_parse, &mut config, &ident.span())?,
            }
            if parse_completely && !to_parse.is_empty() {
                return Err(syn::Error::new(
                    ident.span(),
                    format!("found unexpected input within the directive {ident_str}"),
                ));
            }
            if input.is_empty() {
                break;
            }
        }
        Ok(config)
    }
}

impl IncludeCppConfig {
    pub fn get_pod_requests(&self) -> &[String] {
        &self.pod_requests
    }

    pub fn get_mod_name(&self) -> Ident {
        self.mod_name
            .as_ref()
            .cloned()
            .unwrap_or_else(|| Ident::new("ffi", Span::call_site()))
    }

    /// Whether to avoid generating the standard helpful utility
    /// functions which we normally include in every mod.
    pub fn exclude_utilities(&self) -> bool {
        self.exclude_utilities
    }

    /// Items which the user has explicitly asked us to generate;
    /// we should raise an error if we weren't able to do so.
    pub fn must_generate_list(&self) -> Box<dyn Iterator<Item = String> + '_> {
        if let Allowlist::Specific(items) = &self.allowlist {
            Box::new(
                items
                    .iter()
                    .filter_map(|i| match i {
                        AllowlistEntry::Item(i) => Some(i),
                        AllowlistEntry::Namespace(_) => None,
                    })
                    .chain(self.pod_requests.iter())
                    .cloned(),
            )
        } else {
            Box::new(self.pod_requests.iter().cloned())
        }
    }

    /// The allowlist of items to be passed into bindgen, if any.
    pub fn bindgen_allowlist(&self) -> Option<Box<dyn Iterator<Item = String> + '_>> {
        match &self.allowlist {
            Allowlist::All => None,
            Allowlist::Specific(items) => Some(Box::new(
                items
                    .iter()
                    .flat_map(AllowlistEntry::to_bindgen_items)
                    .chain(
                        self.pod_requests
                            .iter()
                            .cloned()
                            .flat_map(bindgen_spellings),
                    )
                    .chain(self.active_utilities())
                    // A subclass needs the C++ peer class autocxx generates for
                    // it, plus the superclass it derives from. The plain
                    // subclass name is deliberately absent: that names a Rust
                    // struct, and no C++ entity is ever emitted under it, so
                    // allowlisting it could only ever match a user's own type
                    // by coincidence.
                    .chain(
                        self.subclasses
                            .iter()
                            .flat_map(|sc| [format!("{}Cpp", sc.subclass), sc.superclass.clone()]),
                    ),
            )),
            // `IncludeCppEngine::generate` settles the allowlist before
            // anything reads it.
            Allowlist::Unspecified(_) => {
                unreachable!("the allowlist was read before it was settled")
            }
        }
    }

    fn active_utilities(&self) -> Vec<String> {
        if self.exclude_utilities {
            Vec::new()
        } else {
            vec![self.get_makestring_name()]
        }
    }

    fn is_subclass_or_superclass(&self, cpp_name: &str) -> bool {
        self.subclasses
            .iter()
            .flat_map(|sc| {
                [
                    Cow::Owned(sc.subclass.to_string()),
                    Cow::Borrowed(&sc.superclass),
                ]
            })
            .any(|item| cpp_name == item.as_str())
    }

    /// Whether this type is on the allowlist specified by the user.
    ///
    /// A note on the allowlist handling in general. It's used in two places:
    /// 1) As directives to bindgen
    /// 2) After bindgen has generated code, to filter the APIs which
    ///    we pass to cxx.
    ///
    /// This second pass may seem redundant. But sometimes bindgen generates
    /// unnecessary stuff.
    pub fn is_on_allowlist(&self, cpp_name: &str) -> bool {
        self.active_utilities().iter().any(|item| *item == cpp_name)
            || self.is_subclass_or_superclass(cpp_name)
            || self.is_subclass_holder(cpp_name)
            || self.is_subclass_cpp(cpp_name)
            || self.is_rust_fun(cpp_name)
            || self.is_rust_type_name(cpp_name)
            || self.is_concrete_type(cpp_name)
            || match &self.allowlist {
                Allowlist::Unspecified(_) => {
                    unreachable!("the allowlist was read before it was settled")
                }
                Allowlist::All => true,
                Allowlist::Specific(items) => items.iter().any(|entry| match entry {
                    AllowlistEntry::Item(i) => i == cpp_name,
                    AllowlistEntry::Namespace(ns) => cpp_name.starts_with(ns),
                }),
            }
    }

    pub fn is_on_blocklist(&self, cpp_name: &str) -> bool {
        self.blocklist.contains(&cpp_name.to_string())
    }

    pub fn is_on_constructor_blocklist(&self, cpp_name: &str) -> bool {
        self.constructor_blocklist.contains(&cpp_name.to_string())
    }

    /// Whether a `smart_pointer!` directive named this class template.
    ///
    /// `cpp_name` is the name autocxx knows the template by, which is the one
    /// bindgen reported: namespaces separated by `::`, and any enclosing class
    /// flattened into the final segment with an `_`, so a `MyPtr` declared
    /// inside `Outer` is `Outer_MyPtr`. A directive may be written either way -
    /// `smart_pointer!("Outer::MyPtr")` is what C++ calls it - and may leave the
    /// namespaces off, as `throws!` may for a function.
    pub fn is_smart_pointer_template(&self, cpp_name: &str) -> bool {
        self.smart_pointers
            .iter()
            .any(|entry| name_matches_directive(cpp_name, entry))
    }

    pub fn is_on_throws_list(&self, cpp_name: &str) -> bool {
        self.throws_list
            .iter()
            .any(|entry| cpp_name == entry || cpp_name.ends_with(&format!("::{}", entry)))
    }

    /// The `enum_style!` a given C++ type was given, if any.
    pub fn enum_style(&self, cpp_name: &str) -> Option<EnumStyle> {
        self.enum_styles.get(cpp_name)
    }

    /// Every `enum_style!` request, so that the engine can pass them on to
    /// `bindgen`.
    pub fn enum_styles(&self) -> impl Iterator<Item = (&str, EnumStyle)> {
        self.enum_styles.iter()
    }

    /// Every `derive!` request, as (C++ type name, traits).
    pub fn derives(&self) -> impl Iterator<Item = (&str, &[syn::Path])> {
        self.derives.iter()
    }

    /// Whether any `derive!` was written at all.
    pub fn has_derives(&self) -> bool {
        !self.derives.is_empty()
    }

    pub fn get_blocklist(&self) -> impl Iterator<Item = &String> {
        self.blocklist.iter()
    }

    pub fn get_opaquelist(&self) -> impl Iterator<Item = &String> {
        self.opaquelist.iter()
    }

    fn is_concrete_type(&self, cpp_name: &str) -> bool {
        self.concretes.0.values().any(|val| *val == cpp_name)
    }

    /// Get a hash of the contents of this `include_cpp!` block *as it stands*.
    ///
    /// Only an archive key while the block is still as the user wrote it - see
    /// [`ConfigHash`].
    pub fn get_hash(&self) -> ConfigHash {
        ConfigHash(stable_hash(self))
    }

    /// In case there are multiple sets of ffi mods in a single binary,
    /// endeavor to return a name which can be used to make symbols
    /// unique.
    pub fn uniquify_name_per_mod(&self, name: &str) -> String {
        format!("{}_{:#x}", name, self.get_hash().0)
    }

    pub fn get_makestring_name(&self) -> String {
        self.uniquify_name_per_mod("autocxx_make_string")
    }

    pub fn is_rust_type(&self, id: &Ident) -> bool {
        let id_string = id.to_string();
        self.is_rust_type_name(&id_string) || self.is_subclass_holder(&id_string)
    }

    fn is_rust_type_name(&self, possible_ty: &str) -> bool {
        self.rust_types
            .iter()
            .any(|rt| rt.get_final_ident() == possible_ty)
    }

    fn is_rust_fun(&self, possible_fun: &str) -> bool {
        self.extern_rust_funs
            .iter()
            .map(|fun| &fun.sig.ident)
            .any(|id| id == possible_fun)
    }

    pub fn superclasses(&self) -> impl Iterator<Item = &String> {
        let mut uniquified = HashSet::new();
        uniquified.extend(self.subclasses.iter().map(|sc| &sc.superclass));
        uniquified.into_iter()
    }

    pub fn is_subclass_holder(&self, id: &str) -> bool {
        self.subclasses
            .iter()
            .any(|sc| format!("{}Holder", sc.subclass) == id)
    }

    fn is_subclass_cpp(&self, id: &str) -> bool {
        self.subclasses
            .iter()
            .any(|sc| format!("{}Cpp", sc.subclass) == id)
    }

    /// Return the filename to which generated .rs should be written.
    pub fn get_rs_filename(&self) -> String {
        format!(
            "autocxx-{}-gen.rs",
            self.mod_name
                .as_ref()
                .map(|id| id.to_string())
                .unwrap_or_else(|| "ffi-default".into())
        )
    }

    pub fn confirm_complete(&mut self) {
        if matches!(self.allowlist, Allowlist::Unspecified(_)) {
            self.allowlist = Allowlist::Specific(Vec::new());
        }
    }

    /// Used in reduction to substitute all included headers with a single
    /// preprocessed replacement.
    pub fn replace_included_headers(&mut self, replacement: &str) {
        self.inclusions.clear();
        self.inclusions.push(replacement.to_string());
    }
}

#[cfg(feature = "reproduction_case")]
impl ToTokens for IncludeCppConfig {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let directives = get_directives();
        let hexathorpe = syn::token::Pound(Span::call_site());
        for (id, directive) in &directives.need_hexathorpe {
            let id = format_ident!("{}", id);
            for output in directive.output(self) {
                tokens.extend(quote! {
                    #hexathorpe #id #output
                })
            }
        }
        for (id, directive) in &directives.need_exclamation {
            let id = format_ident!("{}", id);
            for output in directive.output(self) {
                tokens.extend(quote! {
                    #id ! (#output)
                })
            }
        }
    }
}

#[cfg(test)]
mod parse_tests {
    use crate::config::UnsafePolicy;
    use crate::{ConfigHash, EnumStyle, IncludeCppConfig};
    use syn::parse_quote;

    #[test]
    fn test_enum_style() {
        let config: IncludeCppConfig = parse_quote! {
            enum_style!(BitfieldEnum, "Flags", "MoreFlags")
            enum_style!(RustifiedNonExhaustiveEnum, "ns::Error")
            generate_pod!("Flags")
        };
        assert_eq!(config.enum_style("Flags"), Some(EnumStyle::BitfieldEnum));
        assert_eq!(
            config.enum_style("MoreFlags"),
            Some(EnumStyle::BitfieldEnum)
        );
        assert_eq!(
            config.enum_style("ns::Error"),
            Some(EnumStyle::RustifiedNonExhaustiveEnum)
        );
        assert_eq!(config.enum_style("Unmentioned"), None);
    }

    /// The reproduction case has to re-parse to the same configuration, so
    /// the style has to come out before the names, as `enum_style!` reads it.
    #[cfg(feature = "reproduction_case")]
    #[test]
    fn test_enum_style_reproduction_case_round_trips() {
        let config: IncludeCppConfig = parse_quote! {
            enum_style!(BitfieldEnum, "Flags", "MoreFlags")
            enum_style!(NewtypeEnum, "Other")
            generate_pod!("Flags")
        };
        let reparsed: IncludeCppConfig =
            syn::parse2(quote::ToTokens::to_token_stream(&config)).unwrap();
        assert_eq!(reparsed.enum_style("Flags"), Some(EnumStyle::BitfieldEnum));
        assert_eq!(
            reparsed.enum_style("MoreFlags"),
            Some(EnumStyle::BitfieldEnum)
        );
        assert_eq!(reparsed.enum_style("Other"), Some(EnumStyle::NewtypeEnum));
    }

    /// A discovered item is recorded by the path from which the block's own
    /// mod can reach it, which for a block inside a mod starts with `super`.
    /// A reproduction case has to read that back.
    #[cfg(feature = "reproduction_case")]
    #[test]
    fn test_paths_out_of_a_mod_round_trip() {
        let config: IncludeCppConfig = parse_quote! {
            generate_all!()
            extern_rust_function!(super::super::called_from_cpp, fn called_from_cpp())
            extern_rust_type!(super::UsedFromCpp)
        };
        let reparsed: IncludeCppConfig =
            syn::parse2(quote::ToTokens::to_token_stream(&config)).unwrap();
        assert_eq!(reparsed.extern_rust_funs.len(), 1);
        assert_eq!(
            quote::ToTokens::to_token_stream(&reparsed.extern_rust_funs[0].path).to_string(),
            "super :: super :: called_from_cpp"
        );
        assert_eq!(
            quote::ToTokens::to_token_stream(&reparsed.rust_types[0]).to_string(),
            "super :: UsedFromCpp"
        );
    }

    /// Only `super` leads a path, and only at the front of one.
    #[test]
    fn test_no_other_keyword_is_a_path_segment() {
        for directive in [
            quote::quote! { extern_rust_type!(fn::T) },
            quote::quote! { extern_rust_type!(T::fn) },
            quote::quote! { extern_rust_type!(T::super::U) },
        ] {
            syn::parse2::<IncludeCppConfig>(directive.clone())
                .expect_err(&format!("accepted {directive}"));
        }
    }

    fn enum_style_parse_error(directive: proc_macro2::TokenStream) -> String {
        syn::parse2::<IncludeCppConfig>(directive)
            .expect_err("expected the enum_style! directive to be rejected")
            .to_string()
    }

    #[test]
    fn test_enum_style_unknown_style_rejected() {
        let err = enum_style_parse_error(quote::quote! {
            enum_style!(ConstifiedEnum, "Flags")
        });
        assert!(
            err.contains("unknown enum style `ConstifiedEnum`") && err.contains("BitfieldEnum"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn test_enum_style_conflicting_styles_rejected() {
        let err = enum_style_parse_error(quote::quote! {
            enum_style!(BitfieldEnum, "Flags")
            enum_style!(NewtypeEnum, "Flags")
        });
        assert!(
            err.contains("Flags") && err.contains("BitfieldEnum") && err.contains("NewtypeEnum"),
            "unhelpful error: {err}"
        );
    }

    /// A pattern would reach bindgen but not autocxx's own bookkeeping, so it
    /// is refused rather than silently half-honoured.
    #[test]
    fn test_enum_style_regex_rejected() {
        let err = enum_style_parse_error(quote::quote! {
            enum_style!(BitfieldEnum, ".*Flags")
        });
        assert!(
            err.contains("not a plain enum name"),
            "unhelpful error: {err}"
        );
    }

    fn vec_of(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// Each `derive!` request with its traits written back out as strings,
    /// since `syn::Path` is awkward to write in an expected value.
    fn rendered_derives(config: &IncludeCppConfig) -> Vec<(String, Vec<String>)> {
        config
            .derives()
            .map(|(name, traits)| {
                (
                    name.to_string(),
                    traits
                        .iter()
                        .map(|path| {
                            quote::ToTokens::to_token_stream(path)
                                .to_string()
                                .replace(' ', "")
                        })
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn test_derive() {
        let config: IncludeCppConfig = parse_quote! {
            derive!("Point", "Debug", "PartialEq")
            derive!("Point", "Clone")
            derive!("ns::Thing", "num_enum::TryFromPrimitive")
            generate_pod!("Point")
        };
        let derives = rendered_derives(&config);
        assert_eq!(
            derives,
            vec![
                (
                    "Point".to_string(),
                    vec_of(&["Debug", "PartialEq", "Clone"])
                ),
                (
                    "ns::Thing".to_string(),
                    vec_of(&["num_enum::TryFromPrimitive"])
                ),
            ]
        );
    }

    #[cfg(feature = "reproduction_case")]
    #[test]
    fn test_derive_reproduction_case_round_trips() {
        let config: IncludeCppConfig = parse_quote! {
            derive!("Point", "Debug", "PartialEq")
            derive!("Other", "Clone")
            generate_pod!("Point")
        };
        let reparsed: IncludeCppConfig =
            syn::parse2(quote::ToTokens::to_token_stream(&config)).unwrap();
        let derives = rendered_derives(&reparsed);
        assert_eq!(
            derives,
            vec![
                ("Point".to_string(), vec_of(&["Debug", "PartialEq"])),
                ("Other".to_string(), vec_of(&["Clone"])),
            ]
        );
    }

    fn derive_parse_error(directive: proc_macro2::TokenStream) -> String {
        syn::parse2::<IncludeCppConfig>(directive)
            .expect_err("expected the derive! directive to be rejected")
            .to_string()
    }

    /// Deriving the same trait twice would be two `#[derive]`s of it, which
    /// is a conflicting implementation rather than a no-op.
    #[test]
    fn test_derive_duplicate_trait_rejected() {
        let err = derive_parse_error(quote::quote! {
            derive!("Point", "Debug")
            derive!("Point", "Debug")
        });
        assert!(
            err.contains("Point") && err.contains("already"),
            "unhelpful error: {err}"
        );
    }

    /// A pattern would reach nothing: autocxx matches the name literally
    /// against the types it generated.
    #[test]
    fn test_derive_regex_rejected() {
        let err = derive_parse_error(quote::quote! {
            derive!(".*Point", "Debug")
        });
        assert!(
            err.contains("not a plain type name"),
            "unhelpful error: {err}"
        );
    }

    /// The trait has to be something Rust could name inside `#[derive(..)]`.
    #[test]
    fn test_derive_non_trait_rejected() {
        let err = derive_parse_error(quote::quote! {
            derive!("Point", "not a trait")
        });
        assert!(
            err.contains("is not the name of a trait"),
            "unhelpful error: {err}"
        );
    }

    /// The bindgen allowlist for a subclass carries the C++ peer class autocxx
    /// generates and the superclass being derived from, and nothing else. The
    /// plain subclass name used to be in there too, but it names a Rust
    /// struct which has no C++ counterpart, so it matched nothing.
    #[test]
    fn test_subclass_bindgen_allowlist() {
        let config: IncludeCppConfig = parse_quote! {
            generate!("Bar")
            subclass!("Observer", MyObserver)
        };
        let allowlist: Vec<String> = config.bindgen_allowlist().unwrap().collect();
        let has = |name: &str| allowlist.iter().any(|entry| entry == name);
        assert!(has("MyObserverCpp"));
        assert!(has("Observer"));
        assert!(!has("MyObserver"));
    }

    #[test]
    fn test_safety_unsafe() {
        let us: UnsafePolicy = parse_quote! {
            unsafe
        };
        assert_eq!(us, UnsafePolicy::AllFunctionsSafe)
    }

    #[test]
    fn test_safety_unsafe_ffi() {
        let us: UnsafePolicy = parse_quote! {
            unsafe_ffi
        };
        assert_eq!(us, UnsafePolicy::AllFunctionsSafe)
    }

    #[test]
    fn test_safety_safe() {
        let us: UnsafePolicy = parse_quote! {};
        assert_eq!(us, UnsafePolicy::AllFunctionsUnsafe)
    }

    /// The archive key crosses a process boundary - `autocxx-gen` writes it and
    /// a separately compiled proc macro looks it up - and hash-derived
    /// identifiers cross into generated Rust and C++, where a build system
    /// caching on file contents notices every change. Neither can afford a hash
    /// which moves on its own, so the values are pinned here.
    ///
    /// Two configs, because the hash is only as fixed as the least fixed thing
    /// fed to it: the plain one covers the strings and the `Allowlist`
    /// discriminant every block has, and the second reaches the `syn` types -
    /// `Ident`, `TypePath`, `Signature` - whose `Hash` impls are the part
    /// `stable_hash` cannot promise anything about.
    ///
    /// Nothing about a failure here is safe to settle by updating the literal
    /// without knowing why it moved. If this crate deliberately changed what
    /// goes into the hash, then archives already on disk are unreadable to
    /// macros built from this code and the hash-derived symbols in generated
    /// output have moved with them, so both have to be regenerated together and
    /// it needs a release note. If instead a compiler or dependency upgrade
    /// moved it, then `stable_hash`'s promise has a hole in it - the
    /// `#[derive(Hash)]` input stream, most likely - and the fix belongs there.
    ///
    /// Two configs are two values, not a proof: a change reaching only fields
    /// neither of them exercises moves neither literal.
    #[test]
    fn test_config_hash_is_pinned() {
        let hexathorpe = syn::token::Pound(proc_macro2::Span::call_site());
        let plain: IncludeCppConfig = parse_quote! {
            #hexathorpe include "a.h"
            generate!("Foo")
        };
        assert_eq!(plain.get_hash(), ConfigHash(0xb03cf0e02f64c740));

        let with_syn_types: IncludeCppConfig = parse_quote! {
            #hexathorpe include "a.h"
            name!(ffi2)
            generate_pod!("Foo")
            subclass!("Base", MySub)
            concrete!("Templated<int>", TemplatedInt)
            extern_cpp_opaque_type!("Opaque", crate::Opaque)
            extern_rust_type!(MyType)
            extern_rust_function!(some_mod::called_from_cpp, fn called_from_cpp(a: u32) -> bool)
            derive!("Foo", "Clone")
            enum_style!(BitfieldEnum, "Flags")
        };
        assert_eq!(with_syn_types.get_hash(), ConfigHash(0x809f6210a5e1f8fe));
        // The point of the second config is the `syn` types, so it is worth
        // knowing they are in there rather than silently dropped.
        assert!(!with_syn_types.extern_rust_funs.is_empty());
        assert!(!with_syn_types.rust_types.is_empty());
        assert!(with_syn_types.mod_name.is_some());
    }
}
