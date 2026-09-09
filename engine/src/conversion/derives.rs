// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Matching `derive!` directives against the types autocxx generated.

use autocxx_parser::IncludeCppConfig;
use indexmap::map::IndexMap as HashMap;
use syn::Path;

use crate::types::QualifiedName;

use super::{
    analysis::{fun::FnPhase, pod::PodAnalysis},
    api::{Api, TypeKind},
    apivec::ApiVec,
    ConvertErrorFromCpp,
};

/// The traits each type is to derive, by the name autocxx knows the type by
/// rather than by whichever C++ spelling the directive happened to use.
pub(crate) type DeriveRequests = HashMap<QualifiedName, Vec<Path>>;

/// What a `derive!` may name: a type whose Rust definition the user can
/// actually see, because autocxx re-exports it from the bindgen module rather
/// than replacing it with an opaque stand-in.
#[derive(Clone, Copy, PartialEq)]
enum DeriveTarget {
    Pod,
    Enum,
}

/// Work out which bindgen item each `derive!` is talking about, and reject the
/// ones which could never take effect.
///
/// Nothing here can be settled while parsing the directive: whether the name
/// belongs to a type autocxx generated, and whether that type is one whose
/// definition reaches the user, are both answers only the C++ can give.
pub(crate) fn resolve_derive_directives(
    config: &IncludeCppConfig,
    apis: &ApiVec<FnPhase>,
) -> Result<DeriveRequests, ConvertErrorFromCpp> {
    let mut requests = DeriveRequests::new();
    if !config.has_derives() {
        return Ok(requests);
    }
    // Both spellings of a nested type, as elsewhere: nobody writing a
    // directive knows about the `Outer_Inner` bindgen flattened it to, and
    // equally nobody should be made to stop using it.
    // See google/autocxx#1422.
    let mut targets: HashMap<String, (QualifiedName, Option<DeriveTarget>)> = HashMap::new();
    for api in apis.iter() {
        let target = match api {
            Api::Struct {
                analysis:
                    super::analysis::fun::PodAndDepAnalysis {
                        pod:
                            PodAnalysis {
                                kind: TypeKind::Pod,
                                ..
                            },
                        ..
                    },
                ..
            } => Some(DeriveTarget::Pod),
            Api::Enum { .. } => Some(DeriveTarget::Enum),
            _ => None,
        };
        let name_info = api.name_info();
        for spelling in name_info.cpp_spellings() {
            // A name can belong to more than one API - a struct and the
            // functions on it - and only the type carries a target.
            let entry = targets
                .entry(spelling)
                .or_insert_with(|| (name_info.name.clone(), None));
            if target.is_some() {
                *entry = (name_info.name.clone(), target);
            }
        }
    }
    for (cpp_name, traits) in config.derives() {
        let (name, target) = match targets.get(cpp_name) {
            Some((name, Some(target))) => (name, *target),
            Some(_) => {
                return Err(ConvertErrorFromCpp::DeriveOnTypeWithNoRustDefinition(
                    cpp_name.to_string(),
                ))
            }
            None => {
                return Err(ConvertErrorFromCpp::DeriveDirectiveMatchedNothing(
                    cpp_name.to_string(),
                ))
            }
        };
        for path in traits {
            // Nothing makes one enumerator of a C++ enum the default, so
            // bindgen's `Default` is stripped from every enum before this and
            // a derived one here would be `error[E0665]` rather than a
            // default anybody wanted.
            if target == DeriveTarget::Enum && path.is_ident("Default") {
                return Err(ConvertErrorFromCpp::DeriveDefaultOnEnum(
                    cpp_name.to_string(),
                ));
            }
            requests.entry(name.clone()).or_default().push(path.clone());
        }
    }
    Ok(requests)
}
