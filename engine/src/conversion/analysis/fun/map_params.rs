// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Recognising the one shape of `std::map` parameter autocxx serves.
//!
//! cxx has no map type, so there is nothing for a map to cross the boundary
//! *as*. What there is instead is a map C++ builds and destroys within the one
//! call: a parameter declared `const std::map<K, V>&` is replaced, in the
//! `cxx::bridge` and in the Rust binding, by two list parameters - the keys and
//! the values - and the C++ wrapper pairs them up into the map the real
//! function is called with. `std::unordered_map` goes the same way, and which
//! list each half becomes is [`super::function_wrapper::MapHalf`]'s business.
//!
//! Everything about the map has to be visible for that, which is why `std::map`
//! is in the known-type database at all: the stand-in autocxx hands bindgen is
//! what keeps the template arguments, bindgen having otherwise discarded them.
//! See [`crate::known_types::Behavior::CxxOrderedMap`].
//!
//! This module decides whether a parameter is that shape. Every map which is
//! not is refused, by the type converter, with
//! [`ConvertErrorFromCpp::UnsupportedMap`].

use quote::ToTokens;
use syn::{parse_quote, GenericArgument, PathArguments, Type, TypePath};

use super::{
    function_wrapper::{TypeConversionPolicy, WholeCppConversion},
    ArgumentAnalysis,
};
use crate::{
    conversion::{
        type_helpers::{ptr_is_mut, unwrap_const, unwrap_reference},
        ConvertErrorFromCpp,
    },
    known_types::known_types,
    minisyn::{FnArg, Ident},
    types::{make_ident, QualifiedName},
};
use indexmap::set::IndexSet as HashSet;

/// What a map parameter's conversion needs to know about the function around
/// it, which the parameter itself cannot say.
pub(super) struct MapContext<'a> {
    /// The error with which map parameters are refused on this function
    /// outright, or `None` where they are served. Decided by
    /// [`super::FnAnalyzer::map_refusal`], which is where the reasons are:
    /// an identical-looking twin declaration, a subclass peer's constructor,
    /// or a constructor whose class declares others an argument could reach.
    pub(super) refusal: Option<ConvertErrorFromCpp>,
    /// The names the function's own parameters hold. The two names a map
    /// parameter becomes are kept clear of these, so a real parameter named
    /// `m_keys` beside a map named `m` does not collide with the synthesized
    /// pair.
    pub(super) taken_param_names: &'a HashSet<String>,
}

/// The names of the two list parameters standing for the map parameter
/// `name`: `{name}_keys` and `{name}_values`, or, where either of those is
/// already a parameter of the function, the first numbered pair which is
/// clear of them all. The two are numbered together so they always read as
/// the pair they are.
pub(super) fn half_names(name: &syn::Ident, taken: &HashSet<String>) -> (Ident, Ident) {
    let mut suffix = 0usize;
    loop {
        let tag = if suffix == 0 {
            String::new()
        } else {
            suffix.to_string()
        };
        let keys = format!("{name}_keys{tag}");
        let values = format!("{name}_values{tag}");
        if !taken.contains(&keys) && !taken.contains(&values) {
            return (make_ident(keys), make_ident(values));
        }
        suffix += 1;
    }
}

/// A parameter which C++ declared `const std::map<K, V>&`, taken apart into
/// what building the map needs.
pub(super) struct MapParam {
    /// `std::map` or `std::unordered_map`, for the C++ wrapper to name.
    pub(super) map: QualifiedName,
    /// The key type as bindgen wrote it, which is what the synthesized vector
    /// parameter is given so that the ordinary conversion makes of it whatever
    /// it would have made of a vector the header declared.
    pub(super) key: Type,
    /// The value type, likewise.
    pub(super) value: Type,
}

/// Whether `ty` is a `const` reference to a map with the default comparator,
/// hash and allocator, and what its key and value are.
///
/// `None` says nothing about whether a map is there: a map in any other shape
/// answers `None` too, and is refused a moment later where the type converter
/// meets it. This only says whether the carve-out applies.
pub(super) fn recognise(ty: &Type) -> Option<MapParam> {
    let Type::Path(tp) = ty else { return None };
    // An lvalue reference. The rvalue marker is a different one, so an
    // `std::map&&` parameter answers `None` here.
    let ptr = unwrap_reference(tp, false)?;
    // `const`: the map the wrapper builds is a temporary, so anything the
    // function writes through the reference is written to storage which is
    // destroyed when the call returns, and Rust would never see it.
    if ptr_is_mut(&ptr.mutability) {
        return None;
    }
    let Type::Path(inner) = &*ptr.elem else {
        return None;
    };
    let Type::Path(map_path) = unwrap_const(inner)? else {
        return None;
    };
    let map = QualifiedName::from_type_path(map_path);
    // The comparator and the allocator - the hash, the equality predicate and
    // the allocator, for an unordered map - each as the template whose
    // specialization is the default. autocxx builds a default-shaped map, so
    // one which fixes any of them to something else is not this shape.
    let defaults = known_types().map_default_extra_arguments(&map)?;
    let args = type_arguments(map_path)?;
    if args.len() != 2 + defaults.len() {
        return None;
    }
    for (arg, expected) in args[2..].iter().zip(defaults) {
        let Type::Path(arg) = arg else { return None };
        if QualifiedName::from_type_path(arg).to_cpp_name() != *expected {
            return None;
        }
    }
    Some(MapParam {
        map,
        key: args[0].clone(),
        value: args[1].clone(),
    })
}

/// Whether this parameter is a *reference* to one of the two maps - lvalue or
/// rvalue, `const` or not.
///
/// Asked by the constructor rule - see
/// [`super::FnAnalyzer::build_constructor_capture_candidates`]: no map binds
/// to a reference to a map of another type, so a sibling constructor taking
/// only such a reference cannot quietly capture the `const` lvalue another
/// constructor's binding presents - the worst it can do is tie or fail to
/// compile, loudly - and such siblings are therefore not a reason to refuse.
/// The `T(const M&)`/`T(M&&)` pair the const-lvalue cast exists to serve is
/// exactly this shape. A map taken *by value* is deliberately not this shape:
/// where the constructor being judged was secretly transparent, a by-value
/// sibling of the same key and value is an exact match for the built map and
/// would capture the call silently, so it counts as reachable like any other
/// class.
pub(super) fn parameter_is_map_reference(arg: &FnArg) -> bool {
    let syn::FnArg::Typed(pt) = &arg.0 else {
        return false;
    };
    let Type::Path(tp) = &*pt.ty else {
        return false;
    };
    let Some(ptr) = unwrap_reference(tp, false).or_else(|| unwrap_reference(tp, true)) else {
        return false;
    };
    let Type::Path(tp) = &*ptr.elem else {
        return false;
    };
    let tp = match unwrap_const(tp) {
        Some(Type::Path(inner)) => inner,
        Some(_) => return false,
        None => tp,
    };
    known_types().is_map(&QualifiedName::from_type_path(tp))
}

/// Whether this parameter is a map in the shape above, and under what name.
///
/// The name is the one C++ gave the parameter, which is what the two
/// synthesized ones are named after.
pub(super) fn recognise_arg(arg: &FnArg) -> Option<(syn::Ident, MapParam)> {
    let syn::FnArg::Typed(pt) = &arg.0 else {
        return None;
    };
    let syn::Pat::Ident(name) = &*pt.pat else {
        return None;
    };
    recognise(&pt.ty).map(|map| (name.ident.clone(), map))
}

/// What to say on a binding whose parameters are not the C++ function's,
/// which is one line per map the signature had.
///
/// Empty for every other function, which is what keeps this off the bindings
/// which need nothing said about them.
pub(super) fn doc_attrs(param_details: &[ArgumentAnalysis]) -> Vec<syn::Attribute> {
    // Pairwise, because the values parameter is always the one after the keys:
    // it is where the other half of the name to quote comes from.
    param_details
        .windows(2)
        .filter_map(|pair| match &pair[0].conversion {
            TypeConversionPolicy::Whole {
                cpp: WholeCppConversion::FromVectorsToMap(build),
                ..
            } => Some((&pair[0].name, &pair[1].name, &build.map)),
            _ => None,
        })
        .map(|(keys, values, map)| {
            let keys = keys.to_token_stream().to_string();
            let values = values.to_token_stream().to_string();
            let map = map.to_cpp_name();
            let doc = format!(
                "C++ takes a `const {map}<K, V>&` where this takes `{keys}` and `{values}`. \
                 They are paired by index - the first key with the first value, and so on - \
                 and the map is built in C++ for the duration of the call; nothing about it \
                 comes back. The two must be the same length, and this panics if they are \
                 not. Where a key appears twice, the first of its values wins."
            );
            parse_quote! { #[doc = #doc] }
        })
        .collect()
}

/// The template arguments written on the last segment of `path`, if every one
/// of them is a type.
fn type_arguments(path: &TypePath) -> Option<Vec<Type>> {
    let PathArguments::AngleBracketed(ab) = &path.path.segments.last()?.arguments else {
        return None;
    };
    ab.args
        .iter()
        .map(|arg| match arg {
            GenericArgument::Type(ty) => Some(ty.clone()),
            _ => None,
        })
        .collect()
}
