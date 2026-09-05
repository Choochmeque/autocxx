// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use autocxx_parser::IncludeCppConfig;
use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;

use crate::{
    conversion::{
        analysis::tdef::TypedefAnalysis,
        api::{Api, OpaqueTypedefReason},
        apivec::ApiVec,
        convert_error::{ConvertErrorWithContext, ErrorContext},
        ConvertErrorFromCpp,
    },
    types::QualifiedName,
};

use super::pod::PodPhase;
/// Where we find a typedef pointing at something we can't represent,
/// e.g. because it uses too many template parameters, break the link.
/// Use the typedef as a first-class type.
pub(crate) fn replace_hopeless_typedef_targets(
    config: &IncludeCppConfig,
    apis: ApiVec<PodPhase>,
) -> ApiVec<PodPhase> {
    // Both maps keep each item's own reason alongside its name, so that a
    // typedef which loses its target can say what was wrong with it rather
    // than only that something was.
    let ignored_types: HashMap<QualifiedName, ConvertErrorFromCpp> = apis
        .iter()
        .filter_map(|api| match api {
            Api::IgnoredItem { err, .. } => Some((api.name().clone(), err.clone())),
            _ => None,
        })
        .collect();
    let ignored_forward_declarations: HashMap<QualifiedName, ConvertErrorFromCpp> = apis
        .iter()
        .filter_map(|api| match api {
            Api::ForwardDeclaration {
                err: Some(ConvertErrorWithContext(err, _)),
                ..
            } => Some((api.name().clone(), err.clone())),
            _ => None,
        })
        .collect();
    // Convert any Typedefs which depend on these things into OpaqueTypedefs
    // instead.
    // And, after this point we no longer need special knowledge of forward
    // declarations with errors, so just convert them into regular IgnoredItems too.
    apis.into_iter()
        .map(|api| match api {
            Api::Typedef {
                ref name,
                analysis: TypedefAnalysis { ref deps, .. },
                ..
            } if blames_any_of(deps, &ignored_types).is_some() =>
            // This typedef depended on something we ignored.
            // Ideally, we'd turn it into an opaque item.
            // We can't do that if this is an inner type,
            // because we have no way to know if it's abstract or not,
            // and we can't represent inner types in cxx without knowing
            // that.
            {
                let name_id = name.name.get_final_ident();
                if api.effective_cpp_name().is_nested() {
                    Api::IgnoredItem {
                        name: api.name_info().clone(),
                        err: ConvertErrorFromCpp::NestedOpaqueTypedef,
                        ctx: Some(ErrorContext::new_for_item(name_id)),
                    }
                } else {
                    Api::OpaqueTypedef {
                        name: api.name_info().clone(),
                        forward_declaration: !config
                            .instantiable
                            .contains(&name.name.to_cpp_name()),
                        reason: blames_any_of(deps, &ignored_types),
                    }
                }
            }
            Api::Typedef {
                analysis: TypedefAnalysis { ref deps, .. },
                ..
            } if blames_any_of(deps, &ignored_forward_declarations).is_some() => {
                Api::OpaqueTypedef {
                    name: api.name_info().clone(),
                    forward_declaration: true,
                    reason: blames_any_of(deps, &ignored_forward_declarations),
                }
            }
            Api::ForwardDeclaration {
                name,
                err: Some(ConvertErrorWithContext(err, ctx)),
            } => Api::IgnoredItem { name, err, ctx },
            _ => api,
        })
        .collect()
}

/// The first of `deps` which `failures` says could not be generated, together
/// with why, or `None` if none of them is in there.
///
/// A typedef usually has exactly one dependency that failed; where it has
/// several, any of them explains why the typedef had to become opaque, so
/// naming the first is as good as naming all of them and reads far better.
fn blames_any_of(
    deps: &HashSet<QualifiedName>,
    failures: &HashMap<QualifiedName, ConvertErrorFromCpp>,
) -> Option<OpaqueTypedefReason> {
    deps.iter().find_map(|dep| {
        failures.get(dep).map(|reason| OpaqueTypedefReason {
            culprit: dep.clone(),
            reason: Box::new(reason.clone()),
        })
    })
}
