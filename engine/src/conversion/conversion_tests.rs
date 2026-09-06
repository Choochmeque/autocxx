// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use autocxx_parser::UnsafePolicy;
#[allow(unused_imports)]
use syn::parse_quote;
use syn::ItemMod;

use crate::{CodegenOptions, UnindexedParseCallbackResults};

use super::BridgeConverter;

// This mod is for tests which take bindgen output directly.
// This should be avoided where possible, since these tests will
// become obsolete or have to change if and when we update
// bindgen. Instead, please add tests working directly from
// the original C++ in integration_tests.rs if possible.
// Also, if you're pasting in code from github issues, it's
// important to make sure that the underlying code has an
// acceptable license. That's why what's here is written by hand.

fn do_test(input: ItemMod) {
    // `generate_all!` so that a pasted bindgen mod is converted whole; with
    // no allowlist directive at all the config is still `Unspecified` and
    // asking whether anything is allowlisted panics.
    let tc = parse_quote! { generate_all!() };
    let bc = BridgeConverter::new(&[], &tc);
    let inclusions = "".into();
    let parse_callback_results = UnindexedParseCallbackResults::with_only_a_root_mod().index();
    bc.convert(
        input,
        parse_callback_results,
        UnsafePolicy::AllFunctionsSafe,
        inclusions,
        &CodegenOptions::default(),
        "",
    )
    .unwrap();
}

// How to add a test here
//
// #[test]
// fn test_xyz() {
//      do_test(parse_quote!{ /* paste bindgen output here */})
// }

/// bindgen names an item `_` whenever the item exists only for its side
/// effect and has nothing anybody can refer to - the `const _: () = ...`
/// blocks holding its layout assertions being the case that reaches us.
/// `_` is a Rust keyword rather than an identifier, so anything autocxx
/// generates under that name fails to parse; the engine used to panic
/// trying.
#[test]
fn test_underscore_named_item_does_not_panic() {
    do_test(parse_quote! {
        mod bindgen {
            mod root {
                #[repr(C)]
                pub struct A {
                    pub a: u32,
                }
                const _: () = {
                    ["Size of A"][::std::mem::size_of::<A>() - 4usize];
                    ["Alignment of A"][::std::mem::align_of::<A>() - 4usize];
                };
            }
        }
    })
}
