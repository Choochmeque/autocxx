// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::set::IndexSet as HashSet;

use crate::{
    conversion::{
        api::{Api, TypeKind},
        apivec::ApiVec,
        error_reporter::convert_item_apis,
        ConvertErrorFromCpp,
    },
    types::QualifiedName,
};

use super::pod::{PodAnalysis, PodPhase};

/// Discard any [`Api::Static`] whose C++ type isn't something we can hand to
/// Rust by value.
///
/// We re-export a static by simply `use`ing `bindgen`'s declaration of it, so
/// the type the user ends up seeing is `bindgen`'s. That's only the same type
/// as the one we expose in our output mod when the type is POD; for anything
/// else our output mod holds an opaque wrapper instead, and re-exporting
/// `bindgen`'s raw struct would both leak our internals and let safe Rust read
/// a type it has no business reading. Enums are fine too, since we re-export
/// `bindgen`'s enum unchanged.
pub(crate) fn discard_statics_of_non_pod_type(apis: ApiVec<PodPhase>) -> ApiVec<PodPhase> {
    let representable = representable_static_types(&apis);
    let mut results = ApiVec::new();
    convert_item_apis(apis, &mut results, |api| match api {
        Api::Static {
            cpp_ty: Some(ref cpp_ty),
            ..
        } if !representable.contains(cpp_ty) => {
            Err(ConvertErrorFromCpp::StaticDataOfNonPodType(cpp_ty.clone()))
        }
        _ => Ok(Box::new(std::iter::once(api))),
    });
    results
}

/// The C++ types which our output mod exposes exactly as `bindgen` declared
/// them, and which are therefore safe to use as the type of a re-exported
/// static.
fn representable_static_types(apis: &ApiVec<PodPhase>) -> HashSet<QualifiedName> {
    apis.iter()
        .filter_map(|api| match api {
            Api::Struct {
                name,
                analysis:
                    PodAnalysis {
                        kind: TypeKind::Pod,
                        ..
                    },
                ..
            }
            | Api::Enum { name, .. } => Some(name.name.clone()),
            _ => None,
        })
        .collect()
}
