// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::IndexMap;
use proc_macro2::TokenStream;
use serde::{Deserialize, Serialize};

use crate::ConfigHash;

/// Struct which stores multiple sets of bindings and can be serialized
/// to disk. This is used when our build system uses `autocxx_gen`; that
/// can handle multiple `include_cpp!` macros and therefore generate multiple
/// sets of Rust bindings. We can't simply `include!` those because there's
/// no (easy) way to pass their details from the codegen phase across to
/// the Rust macro phase. Instead, we use this data structure to store
/// several sets of .rs bindings in a single file, and then the macro
/// extracts the correct set of bindings at expansion time.
#[derive(Serialize, Deserialize, Default)]
pub struct MultiBindings(IndexMap<u64, String>);

use thiserror::Error;

#[derive(Error, Debug)]
pub enum MultiBindingsErr {
    #[error("unable to find the desired bindings within the archive of Rust bindings produced by the autocxx code generation phase")]
    MissingBindings,
    #[error("the stored bindings within the JSON file could not be parsed as valid Rust tokens")]
    BindingsNotParseable,
}

/// Two sets of generated bindings claim one key in the archive.
#[derive(Error, Debug)]
#[error("two different sets of Rust bindings were generated for one key in the archive of bindings, and only one of them can be stored under it. The key is a hash of the include_cpp! block, so the two blocks are the same as far as the macro can tell: if they are separate blocks, give them different mod names with name!(); if they are one source file which reached the codegen tool more than once, pass it once.")]
pub struct ConflictingBindingsErr;

impl MultiBindings {
    /// Insert some generated Rust bindings into this data structure, under the
    /// key for the `include_cpp!` block they were generated from.
    ///
    /// The same bindings twice are the same archive, so they are accepted;
    /// different bindings under one key are refused rather than stored,
    /// because the second would replace the first and the first block's macro
    /// would then expand to the other block's bindings. Two blocks share a key
    /// when they hash alike, which is what the macro has to go on too.
    pub fn insert(
        &mut self,
        config_hash: ConfigHash,
        bindings: TokenStream,
    ) -> Result<(), ConflictingBindingsErr> {
        let bindings = bindings.to_string();
        // Checked before the map is touched, so a refused insert leaves the
        // archive as it was.
        match self.0.get(&config_hash.0) {
            Some(existing) if *existing != bindings => Err(ConflictingBindingsErr),
            Some(_) => Ok(()),
            None => {
                self.0.insert(config_hash.0, bindings);
                Ok(())
            }
        }
    }

    /// Retrieves the bindings for a given `include_cpp!` block.
    pub fn get(&self, config_hash: ConfigHash) -> Result<TokenStream, MultiBindingsErr> {
        match self.0.get(&config_hash.0) {
            None => Err(MultiBindingsErr::MissingBindings),
            Some(bindings) => Ok(bindings
                .parse()
                .map_err(|_| MultiBindingsErr::BindingsNotParseable)?),
        }
    }
}

#[cfg(test)]
mod tests {
    use proc_macro2::Span;
    use quote::quote;
    use syn::parse_quote;

    use crate::IncludeCppConfig;

    use super::MultiBindings;

    #[test]
    fn test_multi_bindings() {
        let hexathorpe = syn::token::Pound(Span::call_site());
        let config1: IncludeCppConfig = parse_quote! {
            #hexathorpe include "a.h"
            generate!("Foo")
        };
        let config2: IncludeCppConfig = parse_quote! {
            #hexathorpe include "b.h"
            generate!("Bar")
        };
        let config3: IncludeCppConfig = parse_quote! {
            #hexathorpe include "c.h"
            generate!("Bar")
        };
        let mut multi_bindings = MultiBindings::default();
        multi_bindings
            .insert(config1.get_hash(), quote! { first; })
            .unwrap();
        multi_bindings
            .insert(config2.get_hash(), quote! { second; })
            .unwrap();
        let json = serde_json::to_string(&multi_bindings).unwrap();
        let multi_bindings2: MultiBindings = serde_json::from_str(&json).unwrap();
        assert_eq!(
            multi_bindings2.get(config2.get_hash()).unwrap().to_string(),
            "second ;"
        );
        assert_eq!(
            multi_bindings2.get(config1.get_hash()).unwrap().to_string(),
            "first ;"
        );
        assert!(multi_bindings2.get(config3.get_hash()).is_err());
    }

    /// The codegen calls `confirm_complete()` on its copy of the config and the
    /// macro never does, so the two see different hashes for one block. This is
    /// why the archive key is taken when the block is parsed rather than when
    /// its bindings are filed - see [`crate::ConfigHash`].
    #[test]
    fn test_confirm_complete_moves_the_hash() {
        let hexathorpe = syn::token::Pound(Span::call_site());
        let as_written: IncludeCppConfig = parse_quote! {
            #hexathorpe include "a.h"
        };
        let mut completed: IncludeCppConfig = parse_quote! {
            #hexathorpe include "a.h"
        };
        completed.confirm_complete();
        assert_ne!(as_written.get_hash(), completed.get_hash());
    }

    /// Two blocks whose contents match share a key, so the second set of
    /// bindings would replace the first and the first block's macro would
    /// expand to the other block's bindings. Refused rather than stored, and
    /// the archive is left holding the first.
    #[test]
    fn test_conflicting_bindings_for_one_key_refused() {
        let hexathorpe = syn::token::Pound(Span::call_site());
        let config: IncludeCppConfig = parse_quote! {
            #hexathorpe include "a.h"
            generate!("Foo")
        };
        let mut multi_bindings = MultiBindings::default();
        multi_bindings
            .insert(config.get_hash(), quote! { mod ffi {} })
            .unwrap();
        multi_bindings
            .insert(config.get_hash(), quote! { mod ffi { fn other(); } })
            .expect_err("the second block's bindings replaced the first's");
        assert_eq!(
            multi_bindings.get(config.get_hash()).unwrap().to_string(),
            "mod ffi { }"
        );
    }

    /// The same bindings twice are not a conflict: an archive built from the
    /// same input twice is the same archive.
    #[test]
    fn test_identical_bindings_for_one_key_allowed() {
        let hexathorpe = syn::token::Pound(Span::call_site());
        let config: IncludeCppConfig = parse_quote! {
            #hexathorpe include "a.h"
            generate!("Foo")
        };
        let mut multi_bindings = MultiBindings::default();
        multi_bindings
            .insert(config.get_hash(), quote! { mod ffi {} })
            .unwrap();
        multi_bindings
            .insert(config.get_hash(), quote! { mod ffi {} })
            .unwrap();
        assert_eq!(
            multi_bindings.get(config.get_hash()).unwrap().to_string(),
            "mod ffi { }"
        );
    }
}
