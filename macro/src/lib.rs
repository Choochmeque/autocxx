// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![forbid(unsafe_code)]

use autocxx_parser::{IncludeCpp, SubclassAttrs};
use proc_macro::TokenStream;
use proc_macro2::{Ident, Span};
use quote::quote;
use syn::parse::Parser;
use syn::{parse_macro_input, parse_quote, Error, Fields, Item, ItemStruct, Result, Visibility};

/// Renders a refusal as the `compile_error!` invocation the caller sees.
///
/// Each entry point below reports at most one problem and then stops, which is
/// what `proc_macro_error`'s `abort!` did for it before: the error tokens are
/// the macro's entire output, and the annotated item is not emitted. Keeping
/// that shape means a caller sees the same diagnostic at the same span, plus
/// the same downstream "cannot find" errors from the item having gone.
fn refuse(err: Error) -> TokenStream {
    err.into_compile_error().into()
}

/// Implementation of the `include_cpp` macro. See documentation for `autocxx` crate.
#[proc_macro]
pub fn include_cpp_impl(input: TokenStream) -> TokenStream {
    let include_cpp = parse_macro_input!(input as IncludeCpp);
    TokenStream::from(include_cpp.generate_rs())
}

/// Attribute to state that a Rust `struct` is a C++ subclass.
/// This adds an additional field to the struct which autocxx uses to
/// track a C++ instantiation of this Rust subclass.
#[proc_macro_attribute]
pub fn subclass(attr: TokenStream, item: TokenStream) -> TokenStream {
    match subclass_impl(attr, item) {
        Ok(toks) => toks.into(),
        Err(err) => refuse(err),
    }
}

fn subclass_impl(attr: TokenStream, item: TokenStream) -> Result<proc_macro2::TokenStream> {
    let mut s: ItemStruct =
        syn::parse(item).map_err(|_| Error::new(Span::call_site(), "Expected a struct"))?;
    if !matches!(s.vis, Visibility::Public(..)) {
        use syn::spanned::Spanned;
        return Err(Error::new(
            s.vis.span(),
            "Rust subclasses of C++ types must by public",
        ));
    }
    let id = &s.ident;
    let cpp_ident = Ident::new(&format!("{id}Cpp"), Span::call_site());
    let input = quote! {
        cpp_peer: autocxx::subclass::CppSubclassCppPeerHolder<ffi:: #cpp_ident>
    };
    let parser = syn::Field::parse_named;
    let new_field = parser.parse2(input).unwrap();
    s.fields = match &mut s.fields {
        Fields::Named(fields) => {
            fields.named.push(new_field);
            s.fields
        }
        Fields::Unit => Fields::Named(parse_quote! {
            {
                #new_field
            }
        }),
        _ => return Err(Error::new(Span::call_site(), "Expect a struct with named fields - use struct A{} or struct A; as opposed to struct A()")),
    };
    let subclass_attrs: SubclassAttrs = syn::parse(attr)
        .map_err(|_| Error::new(Span::call_site(), "Unable to parse attributes"))?;
    let self_owned_bit = if subclass_attrs.self_owned {
        Some(quote! {
            impl autocxx::subclass::CppSubclassSelfOwned<ffi::#cpp_ident> for #id {}
        })
    } else {
        None
    };
    Ok(quote! {
        #s

        impl autocxx::subclass::CppSubclass<ffi::#cpp_ident> for #id {
            fn peer_holder_mut(&mut self) -> &mut autocxx::subclass::CppSubclassCppPeerHolder<ffi::#cpp_ident> {
                &mut self.cpp_peer
            }
            fn peer_holder(&self) -> &autocxx::subclass::CppSubclassCppPeerHolder<ffi::#cpp_ident> {
                &self.cpp_peer
            }
        }

        #self_owned_bit
    })
}

/// Attribute to state that a Rust type is to be exported to C++
/// in the `extern "Rust"` section of the generated `cxx` bindings.
#[proc_macro_attribute]
pub fn extern_rust_type(attr: TokenStream, input: TokenStream) -> TokenStream {
    match check_extern_rust_type(attr, input.clone()) {
        Ok(()) => input,
        Err(err) => refuse(err),
    }
}

fn check_extern_rust_type(attr: TokenStream, input: TokenStream) -> Result<()> {
    if !attr.is_empty() {
        return Err(Error::new(Span::call_site(), "Expected no attributes"));
    }
    let i: Item =
        syn::parse(input).map_err(|_| Error::new(Span::call_site(), "Expected an item"))?;
    match i {
        Item::Struct(..) | Item::Enum(..) | Item::Fn(..) => Ok(()),
        _ => Err(Error::new(Span::call_site(), "Expected a struct or enum")),
    }
}

/// Attribute to state that a Rust function is to be exported to C++
/// in the `extern "Rust"` section of the generated `cxx` bindings.
#[proc_macro_attribute]
pub fn extern_rust_function(attr: TokenStream, input: TokenStream) -> TokenStream {
    match check_extern_rust_function(attr, input.clone()) {
        Ok(()) => input,
        Err(err) => refuse(err),
    }
}

fn check_extern_rust_function(attr: TokenStream, input: TokenStream) -> Result<()> {
    if !attr.is_empty() {
        return Err(Error::new(Span::call_site(), "Expected no attributes"));
    }
    let i: Item =
        syn::parse(input).map_err(|_| Error::new(Span::call_site(), "Expected an item"))?;
    match i {
        Item::Fn(..) => Ok(()),
        _ => Err(Error::new(Span::call_site(), "Expected a function")),
    }
}

/// Attribute which should never be encountered in real life.
/// This is something which features in the Rust source code generated
/// by autocxx-bindgen and passed to autocxx-engine, which should never
/// normally be compiled by rustc before it undergoes further processing.
#[proc_macro_attribute]
pub fn cpp_semantics(_attr: TokenStream, _input: TokenStream) -> TokenStream {
    refuse(Error::new(
        Span::call_site(),
        "Please do not attempt to compile this code. \n\
        This code is the output from the autocxx-specific version of bindgen, \n\
        and should be interpreted by autocxx-engine before further usage.",
    ))
}
