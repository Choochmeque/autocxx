// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;

use super::deps::HasDependencies;
use super::fun::{FnAnalysis, FnKind, FnPhase};
use crate::conversion::apivec::ApiVec;
use crate::conversion::{convert_error::ErrorContext, ConvertErrorFromCpp};
use crate::types::QualifiedName;
use crate::{conversion::api::Api, known_types, types::make_ident};

/// Remove any APIs which depend on other items which have been ignored.
/// We also eliminate any APIs that depend on some type that we just don't
/// know about at all. In either case, we don't simply remove the type, but instead
/// replace it with an error marker.
pub(crate) fn filter_apis_by_ignored_dependents(mut apis: ApiVec<FnPhase>) -> ApiVec<FnPhase> {
    let (ignored_items, valid_items): (Vec<&Api<_>>, Vec<&Api<_>>) = apis
        .iter()
        .partition(|api| matches!(api, Api::IgnoredItem { .. }));
    // Why each ignored item was ignored, so that anything we discard for
    // depending on it can say so. Without this the user is told only that some
    // named item could not be generated - and that item's own explanation is
    // usually garbage collected before it reaches the output, since by then
    // nothing reachable from the allowlist refers to it.
    let mut ignored_items: HashMap<QualifiedName, ConvertErrorFromCpp> = ignored_items
        .into_iter()
        .map(|api| match api {
            Api::IgnoredItem { err, .. } => (api.name().clone(), err.clone()),
            // Skipping such an item would leave its dependents alive and
            // uncompilable, so say so rather than dropping it quietly.
            _ => unreachable!("partitioned on Api::IgnoredItem"),
        })
        .collect();
    let valid_types: HashSet<_> = valid_items
        .into_iter()
        .flat_map(|api| api.valid_types())
        .collect();
    let mut iterate_again = true;
    while iterate_again {
        iterate_again = false;
        apis = apis
            .into_iter()
            .map(|api| {
                let ignored_dependents: HashSet<_> = api
                    .deps()
                    .filter(|dep| ignored_items.contains_key(*dep))
                    .cloned()
                    .collect();
                if !ignored_dependents.is_empty() {
                    iterate_again = true;
                    let culprit = ignored_dependents
                        .get_index(0)
                        .expect("just checked it is not empty")
                        .clone();
                    let reason = ignored_items
                        .get(&culprit)
                        .expect("culprit came from the ignored items")
                        .clone();
                    // The reason this item inherits is the culprit's own, so
                    // that a chain of items which merely depended on each other
                    // does not bury the original problem.
                    ignored_items.insert(api.name().clone(), reason.clone());
                    create_ignore_item(
                        api,
                        ConvertErrorFromCpp::IgnoredDependent {
                            deps: ignored_dependents,
                            culprit,
                            reason: Box::new(reason),
                        },
                    )
                } else {
                    let mut missing_deps = api.deps().filter(|dep| {
                        !valid_types.contains(*dep) && !known_types().is_known_type(dep)
                    });
                    let first = missing_deps.next();
                    std::mem::drop(missing_deps);
                    if let Some(missing_dep) = first.cloned() {
                        create_ignore_item(
                            api,
                            ConvertErrorFromCpp::UnknownDependentType(missing_dep),
                        )
                    } else {
                        api
                    }
                }
            })
            .collect();
    }
    apis
}

fn create_ignore_item(api: Api<FnPhase>, err: ConvertErrorFromCpp) -> Api<FnPhase> {
    let id = api.name().get_final_ident();
    log::info!("Marking as ignored: {} because {}", id.to_string(), err);
    Api::IgnoredItem {
        name: api.name_info().clone(),
        err,
        // A function which needed a C++ wrapper is named after that wrapper,
        // an internal name the user has never seen. The error context is
        // both the name of the documentation stub we emit and the name by
        // which the user's `generate!` directives find this item afterwards
        // (see `Api::name_for_allowlist`), so it has to be the name the user
        // knows the function by - its Rust name.
        ctx: match api {
            Api::Function {
                analysis: FnAnalysis {
                    kind, rust_name, ..
                },
                ..
            } => match kind {
                FnKind::TraitMethod { .. } => None,
                FnKind::Method { impl_for, .. } => Some(ErrorContext::new_for_method(
                    impl_for.get_final_ident(),
                    make_ident(rust_name),
                )),
                FnKind::Function => Some(ErrorContext::new_for_item(make_ident(rust_name))),
            },
            _ => Some(ErrorContext::new_for_item(id)),
        },
    }
}
