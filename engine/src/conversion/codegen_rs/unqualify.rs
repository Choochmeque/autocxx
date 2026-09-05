// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use syn::{
    parse_quote, punctuated::Punctuated, GenericArgument, PathArguments, PathSegment, ReturnType,
    Token, Type, TypePath,
};

use crate::conversion::analysis::bridge_type_names::BridgeTypeNames;
use crate::minisyn::FnArg;
use crate::types::{make_ident, Namespace, QualifiedName};

/// The mod alias every autocxx type is spelled through during analysis, which
/// is how we recognize one here. See [`QualifiedName::to_type_path`].
const OUTPUT_MOD: &str = "output";

fn unqualify_type_path(typ: TypePath, bridge_type_names: &BridgeTypeNames) -> TypePath {
    // If we've still got more than one path segment then this is referring to
    // a type within C++ namespaces. The bridge mod has a flat namespace, so
    // the type goes by a single identifier there - which is not necessarily
    // its own, since two namespaces may each hold a type of that name.
    let bridge_ident = qualified_name_of(&typ).map(|name| bridge_type_names.get(&name));
    let last_seg = typ.path.segments.into_iter().next_back().unwrap();
    let ident = bridge_ident.unwrap_or_else(|| last_seg.ident.clone().into());
    let args = match last_seg.arguments {
        PathArguments::AngleBracketed(mut ab) => {
            ab.args = unqualify_punctuated(ab.args, bridge_type_names);
            PathArguments::AngleBracketed(ab)
        }
        _ => last_seg.arguments.clone(),
    };
    let last_seg: PathSegment = parse_quote!( #ident #args );
    parse_quote!(
        #last_seg
    )
}

/// Recover the name of the autocxx type this path refers to, if it is one.
/// Types `cxx` already knows - `CxxString`, `autocxx::c_int` - are spelled
/// through their own crates rather than through the output mod, and are left
/// to the plain last-segment treatment.
fn qualified_name_of(typ: &TypePath) -> Option<QualifiedName> {
    let mut segments = typ.path.segments.iter();
    if segments.next()?.ident != OUTPUT_MOD {
        return None;
    }
    let mut ns = Namespace::new();
    let mut last = None;
    for seg in segments {
        if let Some(previous) = last.replace(seg.ident.to_string()) {
            ns = ns.push(previous);
        }
    }
    Some(QualifiedName::new(&ns, make_ident(last?)))
}

fn unqualify_punctuated<P>(
    pun: Punctuated<GenericArgument, P>,
    bridge_type_names: &BridgeTypeNames,
) -> Punctuated<GenericArgument, P>
where
    P: Default,
{
    let mut new_pun = Punctuated::new();
    for arg in pun.into_iter() {
        new_pun.push(match arg {
            GenericArgument::Type(t) => GenericArgument::Type(unqualify_type(t, bridge_type_names)),
            _ => arg,
        });
    }
    new_pun
}

fn unqualify_type(typ: Type, bridge_type_names: &BridgeTypeNames) -> Type {
    match typ {
        Type::Path(typ) => Type::Path(unqualify_type_path(typ, bridge_type_names)),
        Type::Reference(mut typeref) => {
            typeref.elem = unqualify_boxed_type(typeref.elem, bridge_type_names);
            Type::Reference(typeref)
        }
        Type::Ptr(mut typeptr) => {
            typeptr.elem = unqualify_boxed_type(typeptr.elem, bridge_type_names);
            Type::Ptr(typeptr)
        }
        _ => typ,
    }
}

fn unqualify_boxed_type(typ: Box<Type>, bridge_type_names: &BridgeTypeNames) -> Box<Type> {
    Box::new(unqualify_type(*typ, bridge_type_names))
}

pub(crate) fn unqualify_ret_type(
    ret_type: ReturnType,
    bridge_type_names: &BridgeTypeNames,
) -> ReturnType {
    match ret_type {
        ReturnType::Type(tok, boxed_type) => {
            ReturnType::Type(tok, unqualify_boxed_type(boxed_type, bridge_type_names))
        }
        _ => ret_type,
    }
}

pub(crate) fn unqualify_params_minisyn(
    params: Punctuated<FnArg, Token![,]>,
    bridge_type_names: &BridgeTypeNames,
) -> Punctuated<FnArg, Token![,]> {
    params
        .into_iter()
        .map(|p| match p.0 {
            syn::FnArg::Typed(mut pt) => {
                pt.ty = unqualify_boxed_type(pt.ty, bridge_type_names);
                syn::FnArg::Typed(pt)
            }
            _ => p.0,
        })
        .map(FnArg)
        .collect()
}

pub(crate) fn unqualify_params(
    params: Punctuated<syn::FnArg, Token![,]>,
    bridge_type_names: &BridgeTypeNames,
) -> Punctuated<syn::FnArg, Token![,]> {
    params
        .into_iter()
        .map(|p| match p {
            syn::FnArg::Typed(mut pt) => {
                pt.ty = unqualify_boxed_type(pt.ty, bridge_type_names);
                syn::FnArg::Typed(pt)
            }
            _ => p,
        })
        .collect()
}
