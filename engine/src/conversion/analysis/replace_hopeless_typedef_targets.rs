// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use autocxx_parser::IncludeCppConfig;
use indexmap::set::IndexSet as HashSet;

use crate::{
    conversion::{
        analysis::tdef::TypedefAnalysis,
        api::Api,
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
    let ignored_types: HashSet<QualifiedName> = apis
        .iter()
        .filter_map(|api| match api {
            Api::IgnoredItem { .. } => Some(api.name()),
            _ => None,
        })
        .cloned()
        .collect();
    let ignored_forward_declarations: HashSet<QualifiedName> = apis
        .iter()
        .filter_map(|api| match api {
            Api::ForwardDeclaration { err: Some(_), .. } => Some(api.name()),
            _ => None,
        })
        .cloned()
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
            } if !ignored_types.is_disjoint(deps) =>
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
                    }
                }
            }
            // Unlike the arm above, this one drops the reason on the floor:
            // `OpaqueTypedef` has nowhere to put it. Anything which then uses
            // this typedef is refused with
            // `TypeContainingForwardDeclaration`, whose message talks about
            // UniquePtr and CxxVector and says nothing about what was actually
            // wrong with the target. That is what a user sees on MSVC for a
            // class-scoped typedef of `std::function`
            // (`test_std_function_method_costs_only_that_method`), where the
            // real explanation - `UnsupportedStdFunction` - belongs to the
            // forward declaration and never travels. Fixing it means carrying
            // the error through `OpaqueTypedef`,
            // `TypeConverter::find_incomplete_types` and
            // `TypeContainingForwardDeclaration`, the way `IgnoredDependent`
            // now carries it.
            Api::Typedef {
                analysis: TypedefAnalysis { ref deps, .. },
                ..
            } if !ignored_forward_declarations.is_disjoint(deps) => Api::OpaqueTypedef {
                name: api.name_info().clone(),
                forward_declaration: true,
            },
            Api::ForwardDeclaration {
                name,
                err: Some(ConvertErrorWithContext(err, ctx)),
            } => Api::IgnoredItem { name, err, ctx },
            _ => api,
        })
        .collect()
}
