// Copyright 2021 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
use crate::{
    conversion::analysis::fun::{
        function_wrapper::{TypeConversionPolicy, RECEIVER_ARG_NAME},
        ArgumentAnalysis, ReceiverMutability,
    },
    minisyn::FnArg,
    types::QualifiedName,
};
use indexmap::set::IndexSet as HashSet;
use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use std::borrow::Cow;
use syn::{
    parse_quote, punctuated::Punctuated, token::Comma, GenericArgument, Pat, PatType, Path,
    PathSegment, ReturnType, Type, TypePath, TypeReference,
};

/// Function which can add explicit lifetime parameters to function signatures
/// where necessary, based on analysis of parameters and return types.
/// This is necessary in five cases:
/// 1) where the parameter is a Pin<&mut T>
///    and the return type is some kind of reference - because lifetime elision
///    is not smart enough to see inside a Pin.
/// 2) as a workaround for https://github.com/dtolnay/cxx/issues/1024, where the
///    input parameter is a non-POD type but the output reference is a POD or
///    built-in type
/// 3) Any parameter is any form of reference, and we're returning an `impl New`
///    3a) an 'impl ValueParam' counts as a reference.
/// 4) If we're using CppRef<'a, T> as a param or return type
/// 5) a `returns_borrow_from!` directive named the parameter the return
///    borrows from, which has to be written down: elision would otherwise pick
///    the receiver, and pick it silently.
///
/// Case 5 is also the only one where a single parameter is annotated rather
/// than all of them. Everywhere else every reference parameter gets `'a`, which
/// is sound because such a function has exactly one of them (see
/// `NoInputReference` and `MultipleInputReferences` in `analysis::fun`).
/// A directive lifts that restriction, and giving `'a` to every parameter of a
/// function which has several would tie the result's life to all of them: the
/// chainable setter it exists for would keep every key and value it was ever
/// handed borrowed for as long as the returned reference lived.
pub(crate) fn add_explicit_lifetime_if_necessary<'r>(
    param_details: &[ArgumentAnalysis],
    mut params: Punctuated<FnArg, Comma>,
    ret_type: Cow<'r, ReturnType>,
    non_pod_types: &HashSet<QualifiedName>,
    ret_conversion: &Option<TypeConversionPolicy>,
) -> (
    Option<TokenStream>,
    Punctuated<FnArg, Comma>,
    Cow<'r, ReturnType>,
) {
    let has_mutable_receiver = param_details.iter().any(|pd| {
        matches!(pd.self_type, Some((_, ReceiverMutability::Mutable)))
            && !pd.is_placement_return_destination
    });

    let any_param_is_reference = param_details
        .iter()
        .any(|pd| pd.has_lifetime || pd.conversion.is_value_param());

    let any_param_is_cppref = param_details
        .iter()
        .any(|pd| pd.conversion.takes_reference_wrapper());
    let return_type_is_impl = return_type_is_impl(&ret_type);
    let return_type_is_cppref = ret_conversion
        .as_ref()
        .is_some_and(TypeConversionPolicy::returns_reference_wrapper);
    let non_pod_ref_param = reference_parameter_is_non_pod_reference(&params, non_pod_types);
    let ret_type_pod = return_type_is_pod_or_known_type_reference(&ret_type, non_pod_types);
    let returning_impl_with_a_reference_param = return_type_is_impl && any_param_is_reference;
    let hits_1024_bug = non_pod_ref_param && ret_type_pod;
    // The parameter a `returns_borrow_from!` directive named: its identifier,
    // and whether it is the receiver, which goes into a signature under
    // several spellings that `param_is_borrow_source` knows how to read.
    let borrow_source = param_details
        .iter()
        .find(|pd| pd.is_borrow_source)
        .map(|pd| BorrowSource {
            name: pd.name.clone(),
            is_receiver: pd.self_type.is_some(),
        });
    if !(has_mutable_receiver
        || hits_1024_bug
        || returning_impl_with_a_reference_param
        || return_type_is_cppref
        || any_param_is_cppref
        || borrow_source.is_some())
    {
        return (None, params, ret_type);
    }
    let new_return_type = match ret_type.as_ref() {
        ReturnType::Type(rarrow, boxed_type) => match boxed_type.as_ref() {
            Type::Reference(rtr) => {
                let mut new_rtr = rtr.clone();
                new_rtr.lifetime = Some(parse_quote! { 'a });
                Some(ReturnType::Type(
                    *rarrow,
                    Box::new(Type::Reference(new_rtr)),
                ))
            }
            Type::Path(typ) => {
                let mut new_path = typ.clone();
                add_lifetime_to_pinned_reference(&mut new_path.path.segments)
                    .ok()
                    .map(|_| ReturnType::Type(*rarrow, Box::new(Type::Path(new_path))))
            }
            Type::ImplTrait(tyit) => {
                let old_tyit = tyit.to_token_stream();
                Some(parse_quote! {
                    #rarrow #old_tyit + 'a
                })
            }
            _ => None,
        },
        _ => None,
    };

    match new_return_type {
        None if return_type_is_cppref || any_param_is_cppref => {
            (Some(quote! { <'a> }), params, ret_type)
        }
        None => (None, params, ret_type),
        Some(new_return_type) => {
            // How many times the directive's parameter was found in this
            // signature and given `'a`. The return type above already carries
            // `'a`, so a signature which annotates the source zero times
            // promises a lifetime nothing constrains - a bridge or wrapper
            // whose safe signature lies - and one which annotates it twice
            // has matched a parameter it should not have.
            let mut sources_annotated = 0usize;
            for param in params.iter_mut().map(|minifnarg| &mut minifnarg.0) {
                // Where the user said which parameter the result borrows from,
                // that one alone carries the lifetime; the rest keep the fresh
                // elided lifetimes Rust gives them.
                let is_the_source = borrow_source
                    .as_ref()
                    .is_some_and(|source| param_is_borrow_source(param, source));
                if borrow_source.is_some() && !is_the_source {
                    continue;
                }
                // A receiver written `&self` prints from its own tokens rather
                // than from a type, so only the `self: T` spelling has one to
                // qualify.
                let ty = match param {
                    syn::FnArg::Typed(PatType { ty, .. })
                    | syn::FnArg::Receiver(syn::Receiver {
                        kind: syn::ReceiverKind::Typed(_, ty),
                        ..
                    }) => ty,
                    syn::FnArg::Receiver(_) => continue,
                };
                let annotated = match ty.as_mut() {
                    Type::Path(TypePath {
                        path: Path { segments, .. },
                        ..
                    }) => add_lifetime_to_pinned_reference(segments).is_ok(),
                    Type::Reference(tyr) => {
                        add_lifetime_to_reference(tyr);
                        true
                    }
                    Type::ImplTrait(tyit) => {
                        add_lifetime_to_impl_trait(tyit);
                        true
                    }
                    _ => false,
                };
                if is_the_source && annotated {
                    sources_annotated += 1;
                }
            }
            if borrow_source.is_some() {
                // An autocxx bug, not a user error: the analysis promised this
                // signature a borrow source, so exactly one parameter here has
                // to carry `'a`. A rename of the receiver, a reordering, or a
                // parameter shape none of the arms above annotate must break
                // here, loudly, rather than ship the lying signature.
                assert_eq!(
                    sources_annotated,
                    1,
                    "returns_borrow_from: the parameter promised as the borrow \
                     source was not annotated with 'a exactly once in `fn ({})`",
                    params.to_token_stream()
                );
            }

            (Some(quote! { <'a> }), params, Cow::Owned(new_return_type))
        }
    }
}

/// The identity of the parameter a `returns_borrow_from!` directive named, as
/// the analysis knows it, for finding it again in a built signature.
struct BorrowSource {
    name: crate::minisyn::Pat,
    is_receiver: bool,
}

/// Whether this parameter is the one the directive named.
///
/// The receiver arrives under three spellings depending on who built the
/// signature: the plain `cxx::bridge` entry is assembled from bindgen's own
/// parameters, where it is a typed parameter whose pattern is `self`; the
/// bridge entry for a function given a C++ wrapper renames that parameter to
/// [`RECEIVER_ARG_NAME`]; and a Rust wrapper's parameters are parsed from
/// `self: T`, which syn reads as a receiver. The analysis calls it `self`
/// whichever of the three a signature holds, which is why the receiver is
/// matched as the receiver rather than by that name.
fn param_is_borrow_source(param: &syn::FnArg, source: &BorrowSource) -> bool {
    let pat = match param {
        syn::FnArg::Receiver(_) => return source.is_receiver,
        syn::FnArg::Typed(PatType { pat, .. }) => pat,
    };
    let Pat::Ident(found) = pat.as_ref() else {
        return false;
    };
    if source.is_receiver {
        return found.ident == "self" || found.ident == RECEIVER_ARG_NAME;
    }
    matches!(&source.name.0, Pat::Ident(wanted) if found.ident == wanted.ident)
}

fn reference_parameter_is_non_pod_reference(
    params: &Punctuated<FnArg, Comma>,
    non_pod_types: &HashSet<QualifiedName>,
) -> bool {
    params.iter().any(|param| match &param.0 {
        syn::FnArg::Typed(PatType { ty, .. }) => match ty.as_ref() {
            Type::Reference(TypeReference { elem, .. }) => match elem.as_ref() {
                Type::Path(typ) => {
                    let qn = QualifiedName::from_type_path(typ);
                    non_pod_types.contains(&qn)
                }
                _ => false,
            },
            _ => false,
        },
        _ => false,
    })
}

fn return_type_is_pod_or_known_type_reference(
    ret_type: &ReturnType,
    non_pod_types: &HashSet<QualifiedName>,
) -> bool {
    match ret_type {
        ReturnType::Type(_, boxed_type) => match boxed_type.as_ref() {
            Type::Reference(rtr) => match rtr.elem.as_ref() {
                Type::Path(typ) => {
                    let qn = QualifiedName::from_type_path(typ);
                    !non_pod_types.contains(&qn)
                }
                _ => false,
            },
            _ => false,
        },
        _ => false,
    }
}

fn return_type_is_impl(ret_type: &ReturnType) -> bool {
    matches!(ret_type, ReturnType::Type(_, boxed_type) if matches!(boxed_type.as_ref(), Type::ImplTrait(..)))
}

#[derive(Debug)]
enum AddLifetimeError {
    WasNotPin,
}

fn add_lifetime_to_pinned_reference(
    segments: &mut Punctuated<PathSegment, syn::token::PathSep>,
) -> Result<(), AddLifetimeError> {
    static EXPECTED_SEGMENTS: &[(&[&str], bool)] = &[
        (&["std", "core"], false),
        (&["pin"], false),
        (&["Pin"], true), // true = act on the arguments of this segment
    ];

    for (seg, (expected_name, act)) in segments.iter_mut().zip(EXPECTED_SEGMENTS.iter()) {
        if !expected_name
            .iter()
            .any(|expected_name| seg.ident == expected_name)
        {
            return Err(AddLifetimeError::WasNotPin);
        }
        if *act {
            match &mut seg.arguments {
                syn::PathArguments::AngleBracketed(aba) => match aba.args.iter_mut().next() {
                    Some(GenericArgument::Type(Type::Reference(tyr))) => {
                        add_lifetime_to_reference(tyr);
                    }
                    _ => panic!("Expected generic args with a reference"),
                },
                _ => panic!("Expected angle bracketed args"),
            }
        }
    }
    Ok(())
}

fn add_lifetime_to_reference(tyr: &mut syn::TypeReference) {
    tyr.lifetime = Some(parse_quote! { 'a })
}

fn add_lifetime_to_impl_trait(tyit: &mut syn::TypeImplTrait) {
    tyit.bounds
        .push(syn::TypeParamBound::Lifetime(parse_quote! { 'a }))
}
