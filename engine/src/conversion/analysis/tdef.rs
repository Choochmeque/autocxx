// Copyright 2021 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::set::IndexSet as HashSet;

use autocxx_parser::IncludeCppConfig;
use syn::ItemType;

use crate::{
    conversion::{
        analysis::type_converter::{add_analysis, Annotated, TypeConversionContext, TypeConverter},
        api::{AnalysisPhase, Api, ApiName, NullPhase, TypedefKind},
        apivec::ApiVec,
        check_for_fatal_attrs,
        convert_error::{ConvertErrorWithContext, ErrorContext},
        error_reporter::convert_apis,
        ConvertErrorFromCpp,
    },
    types::QualifiedName,
    ParseCallbackResults,
};

#[derive(std::fmt::Debug)]
pub(crate) struct TypedefAnalysis {
    pub(crate) kind: TypedefKind,
    pub(crate) deps: HashSet<QualifiedName>,
}

/// Analysis phase where typedef analysis has been performed but no other
/// analyses just yet.
#[derive(std::fmt::Debug)]
pub(crate) struct TypedefPhase;

impl AnalysisPhase for TypedefPhase {
    type TypedefAnalysis = TypedefAnalysis;
    type StructAnalysis = ();
    type FunAnalysis = ();
}

#[allow(clippy::needless_collect)] // we need the extra collect because the closure borrows extra_apis
pub(crate) fn convert_typedef_targets(
    config: &IncludeCppConfig,
    apis: ApiVec<NullPhase>,
    parse_callback_results: &ParseCallbackResults,
) -> ApiVec<TypedefPhase> {
    let mut type_converter = TypeConverter::new(config, &apis);
    let mut extra_apis = ApiVec::new();
    let mut results = ApiVec::new();
    convert_apis(
        apis,
        &mut results,
        Api::fun_unchanged,
        Api::struct_unchanged,
        Api::enum_unchanged,
        |name, item, old_tyname, _| {
            Ok(Box::new(std::iter::once(match item {
                TypedefKind::Type(ity) => get_replacement_typedef(
                    name,
                    (*ity).into(),
                    old_tyname,
                    &mut type_converter,
                    &mut extra_apis,
                    parse_callback_results,
                )?,
                TypedefKind::Use { .. } => Api::Typedef {
                    name,
                    item: item.clone(),
                    old_tyname,
                    analysis: TypedefAnalysis {
                        kind: item,
                        deps: HashSet::new(),
                    },
                },
            })))
        },
    );
    results.extend(extra_apis.into_iter().map(add_analysis));
    ignore_typedefs_to_alias_templates(results)
}

/// An alias template reaches us as a plain typedef, because bindgen discards
/// its template parameters, and we ignore it above because bindgen tells us
/// that happened. Where bindgen left a further typedef pointing at such an
/// alias template - rather than resolving through it to a concrete type - that
/// typedef is parameterized too, so we must ignore it as well: cxx would
/// otherwise emit a forward declaration naming the alias template without its
/// template arguments, which isn't valid C++.
/// See google/autocxx#1094 and google/autocxx#1501.
/// This must happen here, rather than later, because at this point the only
/// items ignored for this reason are typedefs.
fn ignore_typedefs_to_alias_templates(mut apis: ApiVec<TypedefPhase>) -> ApiVec<TypedefPhase> {
    // Propagate to a fixed point: each newly ignored typedef can in
    // turn invalidate typedefs which point at *it* bare (chains of
    // erased alias templates wrap each other). Terminates because
    // every round strictly shrinks the set of Typedef apis.
    loop {
        let alias_templates: HashSet<QualifiedName> = apis
            .iter()
            .filter_map(|api| match api {
                Api::IgnoredItem {
                    err: ConvertErrorFromCpp::UnusedTemplateParam,
                    ..
                } => Some(api.name()),
                _ => None,
            })
            .cloned()
            .collect();
        let mut changed = false;
        apis = apis
            .into_iter()
            .map(|api| match api {
                Api::Typedef {
                    ref name,
                    analysis: TypedefAnalysis { ref deps, .. },
                    ..
                } if !alias_templates.is_disjoint(deps) => {
                    changed = true;
                    Api::IgnoredItem {
                        name: api.name_info().clone(),
                        err: ConvertErrorFromCpp::UnusedTemplateParam,
                        ctx: Some(ErrorContext::new_for_item(name.name.get_final_ident())),
                    }
                }
                _ => api,
            })
            .collect();
        if !changed {
            return apis;
        }
    }
}

fn get_replacement_typedef(
    name: ApiName,
    ity: ItemType,
    old_tyname: Option<QualifiedName>,
    type_converter: &mut TypeConverter,
    extra_apis: &mut ApiVec<NullPhase>,
    parse_callback_results: &ParseCallbackResults,
) -> Result<Api<TypedefPhase>, ConvertErrorWithContext> {
    if !ity.generics.params.is_empty() {
        return Err(ConvertErrorWithContext(
            ConvertErrorFromCpp::TypedefTakesGenericParameters,
            Some(ErrorContext::new_for_item(name.name.get_final_ident())),
        ));
    }
    let mut converted_type = ity.clone();
    check_for_fatal_attrs(parse_callback_results, &name.name)?;
    let type_conversion_results = type_converter.convert_type(
        (*ity.ty).clone(),
        name.name.get_namespace(),
        &TypeConversionContext::WithinReference,
    );
    match type_conversion_results {
        Err(err) => Err(ConvertErrorWithContext(
            err,
            Some(ErrorContext::new_for_item(name.name.get_final_ident())),
        )),
        Ok(Annotated {
            ty: syn::Type::Path(ref typ),
            ..
        }) if QualifiedName::from_type_path(typ) == name.name => Err(ConvertErrorWithContext(
            ConvertErrorFromCpp::InfinitelyRecursiveTypedef(name.name.clone()),
            Some(ErrorContext::new_for_item(name.name.get_final_ident())),
        )),
        Ok(mut final_type) => {
            converted_type.ty = Box::new(final_type.ty.clone());
            extra_apis.append(&mut final_type.extra_apis);
            Ok(Api::Typedef {
                name,
                item: TypedefKind::Type(Box::new(ity.into())),
                old_tyname,
                analysis: TypedefAnalysis {
                    kind: TypedefKind::Type(Box::new(converted_type.into())),
                    deps: final_type.types_encountered,
                },
            })
        }
    }
}
