// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The `enum_style!` directive: per-enum overrides of how `bindgen` renders a
//! C++ `enum` in Rust.

use crate::cpp_names::is_plain_qualified_name;
use indexmap::map::IndexMap as HashMap;
use proc_macro2::{Ident, Span};
use quote::{quote, ToTokens};
use syn::parse::Parse;

/// How `bindgen` should render a particular C++ `enum`.
///
/// Each variant names a `bindgen::Builder` method; the mapping lives in
/// `autocxx_engine`, because this crate deliberately doesn't depend on
/// `bindgen`.
///
/// `bindgen` offers three further styles which `autocxx` cannot use, so they
/// are absent here rather than merely undocumented:
///
/// * `constified_enum` and `constified_enum_module` emit a type alias plus a
///   set of free constants. There is no `struct` or `enum` for `autocxx` to
///   hang a C++ type onto, so nothing would reach the `cxx` bridge.
/// * `newtype_global_enum` emits the newtype but puts its constants at module
///   scope rather than in an `impl` block. `autocxx` re-exports named types,
///   not loose constants, so the values would be unreachable from `ffi`.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum EnumStyle {
    /// An integer newtype whose variants are associated constants, plus
    /// bitwise `&`, `|`, `^` and `!` operators. Good for flag enums.
    BitfieldEnum,
    /// An integer newtype whose variants are associated constants.
    NewtypeEnum,
    /// A native Rust `enum`. This is `autocxx`'s default.
    RustifiedEnum,
    /// A native Rust `enum` annotated `#[non_exhaustive]`.
    RustifiedNonExhaustiveEnum,
}

impl EnumStyle {
    /// The spelling accepted inside `enum_style!` and emitted back out by
    /// [`ToTokens`].
    pub fn as_str(&self) -> &'static str {
        match self {
            EnumStyle::BitfieldEnum => "BitfieldEnum",
            EnumStyle::NewtypeEnum => "NewtypeEnum",
            EnumStyle::RustifiedEnum => "RustifiedEnum",
            EnumStyle::RustifiedNonExhaustiveEnum => "RustifiedNonExhaustiveEnum",
        }
    }

    /// Every style a user may write, for diagnostics.
    pub fn all() -> [EnumStyle; 4] {
        [
            EnumStyle::BitfieldEnum,
            EnumStyle::NewtypeEnum,
            EnumStyle::RustifiedEnum,
            EnumStyle::RustifiedNonExhaustiveEnum,
        ]
    }

    /// Whether an enum rendered in this style reaches Rust as a `struct`
    /// rather than an `enum`.
    ///
    /// That distinction matters a great deal to `autocxx`: a `struct` is only
    /// re-exported with its associated constants intact if it is POD, so these
    /// styles require `generate_pod!` rather than `generate!`. See the
    /// `enum_style!` documentation.
    pub fn is_newtype(&self) -> bool {
        match self {
            EnumStyle::BitfieldEnum | EnumStyle::NewtypeEnum => true,
            EnumStyle::RustifiedEnum | EnumStyle::RustifiedNonExhaustiveEnum => false,
        }
    }

    /// The style a given spelling names, if any. Deliberately not
    /// `std::str::FromStr`: the caller wants the span-carrying `syn::Error`
    /// built in [`Parse`], not a stringly-typed one.
    fn from_name(name: &str) -> Option<Self> {
        EnumStyle::all()
            .into_iter()
            .find(|style| style.as_str() == name)
    }
}

impl Parse for EnumStyle {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let style_ident: Ident = input.parse()?;
        EnumStyle::from_name(&style_ident.to_string()).ok_or_else(|| {
            let known = EnumStyle::all()
                .iter()
                .map(|style| style.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            syn::Error::new(
                style_ident.span(),
                format!("unknown enum style `{style_ident}`; expected one of {known}"),
            )
        })
    }
}

impl ToTokens for EnumStyle {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let var = Ident::new(self.as_str(), Span::call_site());
        tokens.extend(quote! { #var });
    }
}

/// The style requested for each named C++ enum. Enums absent from the map get
/// `autocxx`'s default, which is [`EnumStyle::RustifiedEnum`].
///
/// Keyed by enum name rather than by style so that asking for one enum in two
/// different styles is a parse error rather than a silent race between two
/// `bindgen` builder calls.
#[derive(Debug, Default)]
pub(crate) struct EnumStyleMap(HashMap<String, EnumStyle>);

impl std::hash::Hash for EnumStyleMap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for (name, style) in &self.0 {
            name.hash(state);
            style.hash(state);
        }
    }
}

/// Why an `enum_style!` name was rejected.
pub(crate) enum EnumStyleError {
    /// The same enum was already asked for in a different style.
    Conflict(EnumStyle),
    /// The name isn't a plain `::`-separated identifier path.
    NotAPlainName,
}

impl EnumStyleMap {
    /// Record that `name` should be rendered in `style`, or report why not.
    pub(crate) fn insert(&mut self, name: String, style: EnumStyle) -> Result<(), EnumStyleError> {
        // `bindgen` would happily take a regex here, but autocxx has to
        // recognize the same enum again later - to know not to synthesize
        // constructors for something which is really an enum - and it does
        // that by matching the name literally. Rather than let the two drift
        // apart silently, insist on a name we can match.
        if !is_plain_qualified_name(&name) {
            return Err(EnumStyleError::NotAPlainName);
        }
        match self.0.insert(name, style) {
            Some(previous) if previous != style => Err(EnumStyleError::Conflict(previous)),
            _ => Ok(()),
        }
    }

    /// The style asked for a given C++ enum name, if any.
    pub(crate) fn get(&self, cpp_name: &str) -> Option<EnumStyle> {
        self.0.get(cpp_name).copied()
    }

    /// Every (name, style) pair, in the order the directives were written.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, EnumStyle)> {
        self.0.iter().map(|(name, style)| (name.as_str(), *style))
    }
}
