// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;

use crate::{
    conversion::{
        api::{Api, ApiName, StructDetails, TypeKind},
        apivec::ApiVec,
        convert_error::ConvertErrorWithContext,
        error_reporter::convert_apis,
    },
    types::QualifiedName,
};

use super::fun::{
    FnAnalysis, FnKind, FnPhase, FnPrePhase3, MethodKind, PodAndConstructorAnalysis,
    PodAndDepAnalysis, TraitMethodKind,
};

/// We've now analyzed all functions (including both implicit and explicit
/// constructors). Decorate each struct with a note of its constructors,
/// which will later be used as edges in the garbage collection, because
/// typically any use of a type will require us to call its copy or move
/// constructor. The same applies to its alloc/free functions.
pub(crate) fn decorate_types_with_constructor_deps(apis: ApiVec<FnPrePhase3>) -> ApiVec<FnPhase> {
    let mut constructors_and_allocators_by_type = find_important_constructors(&apis);
    let mut results = ApiVec::new();
    convert_apis(
        apis,
        &mut results,
        Api::fun_unchanged,
        |name, details, pod| {
            decorate_struct(name, details, pod, &mut constructors_and_allocators_by_type)
        },
        Api::enum_unchanged,
        Api::typedef_unchanged,
        Api::subclass_unchanged,
    );
    // `convert_apis` passes a concrete type through untouched, so decorate
    // those separately. They need it for the same reason a struct does, and
    // more urgently: a concrete type is usually reached as a typedef's
    // dependency rather than named by an allowlist directive, so its special
    // members are not roots in their own right the way an allowlisted class's
    // are. See google/autocxx#723.
    results
        .into_iter()
        .map(|api| match api {
            Api::ConcreteType {
                name,
                rs_definition,
                cpp_definition,
                holder_surface,
                incomplete_argument,
                ..
            } => {
                let constructor_and_allocator_deps = constructors_and_allocators_by_type
                    .remove(&name.name)
                    .unwrap_or_default();
                Api::ConcreteType {
                    name,
                    rs_definition,
                    cpp_definition,
                    holder_surface,
                    constructor_and_allocator_deps,
                    incomplete_argument,
                }
            }
            _ => api,
        })
        .collect()
}

fn decorate_struct(
    name: ApiName,
    details: Box<StructDetails>,
    fn_struct: PodAndConstructorAnalysis,
    constructors_and_allocators_by_type: &mut HashMap<QualifiedName, Vec<QualifiedName>>,
) -> Result<Box<dyn Iterator<Item = Api<FnPhase>>>, ConvertErrorWithContext> {
    let pod = fn_struct.pod;
    let is_abstract = matches!(pod.kind, TypeKind::Abstract);
    let constructor_and_allocator_deps = if is_abstract || pod.num_generics > 0 {
        Vec::new()
    } else {
        constructors_and_allocators_by_type
            .remove(&name.name)
            .unwrap_or_default()
    };
    Ok(Box::new(std::iter::once(Api::Struct {
        name,
        details,
        analysis: PodAndDepAnalysis {
            pod,
            constructor_and_allocator_deps,
            constructors: fn_struct.constructors,
        },
    })))
}

fn find_important_constructors(
    apis: &ApiVec<FnPrePhase3>,
) -> HashMap<QualifiedName, Vec<QualifiedName>> {
    // A class named by an allowlist directive is already what makes its own
    // `new()` and destructor garbage-collection roots, so those are not listed
    // for one. A concrete type usually has no such directive naming it - the
    // user writes the typedef which resolves to it - so it has to list them.
    // See google/autocxx#723.
    let concrete_types: HashSet<&QualifiedName> = apis
        .iter()
        .filter_map(|api| match api {
            Api::ConcreteType { name, .. } => Some(&name.name),
            _ => None,
        })
        .collect();
    let mut results: HashMap<QualifiedName, Vec<QualifiedName>> = HashMap::new();
    for api in apis.iter() {
        if let Api::Function {
            name,
            analysis:
                FnAnalysis {
                    kind,
                    ignore_reason: Ok(_),
                    ..
                },
            ..
        } = api
        {
            let impl_for = match kind {
                FnKind::TraitMethod {
                    kind:
                        TraitMethodKind::Alloc
                        | TraitMethodKind::Dealloc
                        | TraitMethodKind::CopyConstructor
                        | TraitMethodKind::MoveConstructor,
                    impl_for,
                    ..
                } => Some(impl_for),
                FnKind::TraitMethod {
                    kind: TraitMethodKind::Destructor,
                    impl_for,
                    ..
                }
                | FnKind::Method {
                    impl_for,
                    method_kind: MethodKind::Constructor { .. },
                    ..
                } if concrete_types.contains(impl_for) => Some(impl_for),
                _ => None,
            };
            if let Some(impl_for) = impl_for {
                results
                    .entry(impl_for.clone())
                    .or_default()
                    .push(name.name.clone())
            }
        }
    }
    results
}
