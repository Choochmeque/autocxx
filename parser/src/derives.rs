// Copyright 2026 The autocxx maintainers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The `derive!` directive: extra `#[derive(..)]` traits on the Rust types
//! autocxx generates from C++ ones.

use crate::cpp_names::is_plain_qualified_name;
use indexmap::map::IndexMap as HashMap;
use syn::Path;

/// The traits `derive!` asked for, by the C++ name of the type each goes on.
///
/// Keyed by type rather than by trait so that repeating a trait for one type
/// is a parse error rather than a duplicate `#[derive]` and a conflicting
/// implementation much later on. Each trait is held as the path it will be
/// written as, so that whether the user wrote something nameable is settled
/// here, where the span to complain about is still to hand.
#[derive(Debug, Default)]
pub struct DeriveMap(HashMap<String, Vec<Path>>);

impl std::hash::Hash for DeriveMap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for (name, traits) in &self.0 {
            name.hash(state);
            traits.hash(state);
        }
    }
}

/// Why a `derive!` argument was rejected.
pub(crate) enum DeriveError {
    /// The type name isn't a plain `::`-separated identifier path.
    NotAPlainName,
    /// This trait was already asked for on this type.
    Duplicate,
}

impl DeriveMap {
    /// Record that `cpp_name` should also derive `trait_name`, or report why
    /// not. Both are taken as written; whether the type exists, and whether
    /// the trait can go on it, are settled once the C++ has been parsed.
    pub(crate) fn insert(&mut self, cpp_name: String, trait_path: Path) -> Result<(), DeriveError> {
        if !is_plain_qualified_name(&cpp_name) {
            return Err(DeriveError::NotAPlainName);
        }
        let traits = self.0.entry(cpp_name).or_default();
        if traits.contains(&trait_path) {
            return Err(DeriveError::Duplicate);
        }
        traits.push(trait_path);
        Ok(())
    }

    /// Every (C++ type name, traits) pair, in the order the directives were
    /// written.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[Path])> {
        self.0
            .iter()
            .map(|(name, traits)| (name.as_str(), traits.as_slice()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
