// Copyright 2021 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::ParseResult;
use proc_macro2::{Ident, Span};
use quote::{quote, ToTokens, TokenStreamExt};
use syn::parse::{Parse, ParseStream};

/// A little like [`syn::Path`] but simpler - contains only identifiers,
/// no path arguments. Guaranteed to always have at least one identifier.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct RustPath(Vec<Ident>);

impl RustPath {
    pub fn new_from_ident(id: Ident) -> Self {
        Self(vec![id])
    }

    #[must_use]
    pub fn append(&self, id: Ident) -> Self {
        Self(self.0.iter().cloned().chain(std::iter::once(id)).collect())
    }

    pub fn get_final_ident(&self) -> &Ident {
        self.0.last().unwrap()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The same path, written from `depth` mods further in. A path recorded
    /// while walking a file is relative to that file's root; code generated for
    /// an `include_cpp!` inside a `mod` has to climb back out to use it.
    #[must_use]
    pub fn from_within_mods(&self, depth: usize) -> Self {
        Self(
            std::iter::repeat_with(|| Ident::new("super", Span::call_site()))
                .take(depth)
                .chain(self.0.iter().cloned())
                .collect(),
        )
    }
}

impl ToTokens for RustPath {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let mut it = self.0.iter();
        let mut id = it.next();
        while id.is_some() {
            id.unwrap().to_tokens(tokens);
            let next = it.next();
            if next.is_some() {
                tokens.append_all(quote! { :: });
            }
            id = next;
        }
    }
}

impl Parse for RustPath {
    fn parse(input: ParseStream) -> ParseResult<Self> {
        // A path written for use from inside a mod starts with a run of
        // `super`, which is a keyword rather than an ordinary identifier. A
        // reproduction case has to read back what autocxx wrote - but only
        // `super` leads a path autocxx writes, so no other keyword is
        // accepted anywhere.
        let mut segments = Vec::new();
        while input.peek(syn::Token![super]) {
            let kw = input.parse::<syn::Token![super]>()?;
            segments.push(Ident::new("super", kw.span));
            input.parse::<syn::token::PathSep>()?;
        }
        let id: Ident = input.parse()?;
        segments.push(id);
        while input.parse::<Option<syn::token::PathSep>>()?.is_some() {
            let id: Ident = input.parse()?;
            segments.push(id);
        }
        Ok(Self(segments))
    }
}
