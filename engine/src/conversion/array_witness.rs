// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The triviality certificate cxx will not write for an array element.

use autocxx_parser::{stable_hash, IncludeCppConfig};
use indexmap::set::IndexSet;
use syn::{FnArg, PatType, ReturnType, Type};

use crate::{
    conversion::{
        analysis::fun::FnPhase, api::Api, apivec::ApiVec, type_helpers::cpp_array_element,
    },
    known_types::known_types,
    types::QualifiedName,
};

/// Every element type in the bridge's own signatures which needs a triviality
/// certificate written for it.
///
/// cxx holds a `[T; N]` where `T` is one of its atoms, or where something in
/// the bridge requires `T` to be trivially movable - and an array element is
/// not among the uses it reads as such a requirement. What it does read is
/// `required_trivial_reasons`, cxx-gen 0.7.200 `src/syntax/trivial.rs:30`: a
/// function argument, a return, a struct field, a `Box`, a `Vec` and a slice.
///
/// So the requirement is stated in the first of those forms, by
/// [`super::codegen_rs::RsCodeGenerator`] declaring a by-value function per
/// element type and [`super::codegen_cpp::CppCodeGenerator`] defining it. An
/// element reaching here is one the analysis let through - see
/// `FnAnalyzer::permissible_array_element` for what that admits, and for where
/// the C++ check on it comes from.
///
/// Every signature the bridge emits is read, not only the C++ ones: an
/// `extern "Rust"` function and a subclass callback reach it by routes of
/// their own, and cxx applies the same rule to an array in any of them.
///
/// Nothing else joins the set: an atom needs no certificate, and a type which
/// is never an array element gets no declaration, so a bridge whose header has
/// no `std::array` is unchanged.
///
/// This is standing in for a `TrivialReason::ArrayElement` cxx does not have.
/// Should cxx gain one, the whole of this - the set, the declaration and the
/// definition - is deletable.
pub(crate) fn array_element_witnesses(apis: &ApiVec<FnPhase>) -> IndexSet<QualifiedName> {
    let mut witnesses = IndexSet::new();
    for api in apis.iter() {
        match api {
            Api::Function { analysis, .. } => {
                // Keep in step with the same test at the top of `gen_function`.
                // A signature which is not emitted declares nothing, and its
                // element type may not reach the bridge at all - so a
                // certificate for it would name a type nothing declares.
                if analysis.ignore_reason.is_err() || !analysis.externally_callable {
                    continue;
                }
                for param in analysis.params.iter() {
                    note_fn_arg(param, &mut witnesses);
                }
                note_return(&analysis.ret_type, &mut witnesses);
            }
            // Nothing reaches this arm today: `extern_rust_function` turns an
            // array down before the API is made, in
            // `parse/extern_fun_signatures.rs`. It is here so that the
            // certificate follows the signature rather than the other way
            // round, if that restriction is ever lifted.
            Api::RustFn { details, .. } => {
                for param in details.sig.inputs.iter() {
                    note_fn_arg(param, &mut witnesses);
                }
                note_return(&details.sig.output, &mut witnesses);
            }
            Api::RustSubclassFn { details, .. } => {
                for param in details.params.iter() {
                    note_fn_arg(param, &mut witnesses);
                }
                note_return(&details.ret, &mut witnesses);
            }
            _ => {}
        }
    }
    witnesses
}

fn note_fn_arg(arg: &FnArg, witnesses: &mut IndexSet<QualifiedName>) {
    if let FnArg::Typed(PatType { ty, .. }) = arg {
        note_element(ty, witnesses);
    }
}

fn note_return(ret: &ReturnType, witnesses: &mut IndexSet<QualifiedName>) {
    if let ReturnType::Type(_, ty) = ret {
        note_element(ty, witnesses);
    }
}

/// Add whatever `ty` holds an array of, where that is a type which needs the
/// certificate.
fn note_element(ty: &Type, witnesses: &mut IndexSet<QualifiedName>) {
    let Some(Type::Path(path)) = cpp_array_element(ty) else {
        return;
    };
    let name = QualifiedName::from_type_path(path);
    if !known_types().permissible_within_array(&name) {
        witnesses.insert(name);
    }
}

/// The name the bridge and the generated C++ both call the certificate for
/// `ty`.
///
/// Three parts, each earning its place. The element's own final name, so that
/// generated code can be read. A hash of its whole C++ name, because
/// flattening a namespace into an identifier is ambiguous - `a::b_C` and
/// `a_b::C` are different types with one flattening - and two certificates of
/// one name would be two Rust declarations of one name. And
/// [`IncludeCppConfig::uniquify_name_per_mod`], because cxx writes the shim
/// for this function under a C-linkage symbol built from its name alone: two
/// `include_cpp!` blocks in one binary whose headers share an element type
/// would otherwise define that symbol twice, and the second definition is a
/// link error rather than anything the Rust compiler would catch.
///
/// A user's own C++ function of this name would collide, which is the bargain
/// the holder shims make too; the hashes are what make it a name nobody
/// writes.
///
/// Both hashes are [`stable_hash`], not any hash: this name is emitted into
/// generated Rust and generated C++ and becomes part of a linker symbol, and generated
/// output for unchanged input has to be byte-identical from one run to the
/// next whatever compiled the generator.
///
/// The readable part is reduced to single underscores first, because cxx turns
/// down any C++ identifier containing a double one - cxx-gen 0.7.200
/// `src/syntax/ident.rs:15` - and a class called `Elem_` would otherwise make
/// one when the rest of the name is appended.
pub(crate) fn witness_name(config: &IncludeCppConfig, ty: &QualifiedName) -> String {
    config.uniquify_name_per_mod(&format!(
        "{}_autocxx_array_element_{:x}",
        single_underscores(ty.get_final_item()),
        stable_hash(&ty.to_cpp_name())
    ))
}

/// `name` with every run of underscores reduced to one and any trailing
/// underscore removed, so that appending to it cannot produce a double.
fn single_underscores(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::witness_name;
    use crate::types::QualifiedName;
    use autocxx_parser::IncludeCppConfig;
    use syn::parse_quote;

    /// This name is written into generated Rust and generated C++ and becomes
    /// part of a linker symbol, so the same input has to produce the same name
    /// whatever built the generator - and both hashes in it have to come from
    /// [`autocxx_parser::stable_hash`] for that to hold. Pinned for the same
    /// reasons as `autocxx_parser`'s own config-hash pin: if this moves,
    /// every build system caching on generated file contents rebuilds
    /// everything downstream of an autocxx bridge.
    #[test]
    fn test_witness_name_is_pinned() {
        let hexathorpe = syn::token::Pound(proc_macro2::Span::call_site());
        let config: IncludeCppConfig = parse_quote! {
            #hexathorpe include "a.h"
            generate!("Foo")
        };
        assert_eq!(
            witness_name(&config, &QualifiedName::new_from_cpp_name("a::b::Elem")),
            "Elem_autocxx_array_element_d99fd6cbbb04d53e_0xb03cf0e02f64c740"
        );
    }
}
