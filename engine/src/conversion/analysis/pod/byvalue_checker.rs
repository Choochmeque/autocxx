// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::conversion::apivec::ApiVec;
use crate::{conversion::ConvertErrorFromCpp, known_types::known_types};
use crate::{
    conversion::{
        analysis::tdef::TypedefPhase,
        api::{Api, TypedefKind},
    },
    types::{Namespace, QualifiedName},
};
use autocxx_parser::IncludeCppConfig;
use std::collections::{HashMap, HashSet};
use syn::{ItemStruct, Type};

#[derive(Clone)]
enum PodState {
    UnsafeToBePod(String),
    SafeToBePod,
    IsPod,
    IsAlias(QualifiedName),
}

#[derive(Clone)]
struct StructDetails {
    state: PodState,
    dependent_structs: Vec<QualifiedName>,
}

impl StructDetails {
    fn new(state: PodState) -> Self {
        StructDetails {
            state,
            dependent_structs: Vec::new(),
        }
    }
}

/// Type which is able to check whether it's safe to make a type
/// fully representable by cxx. For instance if it is a struct containing
/// a struct containing a std::string, the answer is no, because that
/// std::string contains a self-referential pointer.
/// It is possible that this is duplicative of the information stored
/// elsewhere in the `Api` list and could possibly be removed or simplified.
/// In general this is one of the oldest parts of autocxx and
/// the code here could quite possibly be simplified by reusing code
/// elsewhere.
pub struct ByValueChecker {
    // Mapping from type name to whether it is safe to be POD
    results: HashMap<QualifiedName, StructDetails>,
}

impl ByValueChecker {
    pub fn new() -> Self {
        let mut results = HashMap::new();
        for (tn, by_value_safe) in known_types().get_pod_safe_types() {
            let safety = if by_value_safe {
                PodState::IsPod
            } else {
                PodState::UnsafeToBePod(format!("type {tn} is not safe for POD"))
            };
            results.insert(tn.clone(), StructDetails::new(safety));
        }
        ByValueChecker { results }
    }

    /// Scan APIs to work out which are by-value safe. Constructs a [ByValueChecker]
    /// that others can use to query the results.
    pub(crate) fn new_from_apis(
        apis: &ApiVec<TypedefPhase>,
        config: &IncludeCppConfig,
    ) -> Result<ByValueChecker, ConvertErrorFromCpp> {
        let mut byvalue_checker = ByValueChecker::new();
        for blocklisted in config.get_blocklist() {
            let tn = QualifiedName::new_from_cpp_name(blocklisted);
            let safety = PodState::UnsafeToBePod(format!("type {tn} is on the blocklist"));
            byvalue_checker
                .results
                .insert(tn, StructDetails::new(safety));
        }
        // As we do this analysis, we need to be aware that structs
        // may depend on other types. Ideally we'd use the depth first iterator
        // but that's awkward given that our ApiPhase does not yet have a fixed
        // list of field/base types. Instead, we'll iterate first over non-struct
        // types and then over structs.
        // TODO: the second pass is still order-dependent. `ingest_struct` looks
        // each field type up in `results` as it goes, so a struct holding one
        // that bindgen emitted after it is reported as "isn't known" and can't
        // be POD. C++ forces a struct to be complete before it's held by value,
        // so this only bites for a nested class - `struct A { struct B {..}; B
        // b; };` with `generate_pod!("A")` fails today. Ingesting structs to a
        // fixed point, or sorting them by dependency, would fix it.
        for api in apis.iter() {
            match api {
                Api::Typedef { analysis, .. } => {
                    let name = api.name();
                    // Whatever this typedef names: the substitute for a type we
                    // know about (`uint32_t` -> `u32`), or else the name as
                    // written, which may be another typedef or a struct we're
                    // also processing. Either way the typedef is POD exactly
                    // when its target is, so we record the link and let
                    // `satisfy_requests` walk the chain. See google/autocxx#264.
                    let typedef_target = match analysis.kind {
                        TypedefKind::Type(ref type_item) => match type_item.ty.as_ref() {
                            Type::Path(typ) => Some(QualifiedName::from_type_path(typ)),
                            _ => None,
                        },
                        TypedefKind::Use(ref ty) => match **ty {
                            crate::minisyn::Type(Type::Path(ref typ)) => {
                                Some(QualifiedName::from_type_path(typ))
                            }
                            _ => None,
                        },
                    }
                    .map(|target_tn| {
                        match known_types().consider_substitution(&target_tn) {
                            Some(typ) => QualifiedName::from_type_path(&typ),
                            None => target_tn,
                        }
                    });
                    // A typedef to a raw pointer is trivially
                    // copyable regardless of pointee, exactly like a
                    // directly written pointer field; previously it
                    // fell through to "typedef to a complex type" and
                    // poisoned containing PODs. See google/autocxx#1368.
                    let target_is_pointer = match analysis.kind {
                        TypedefKind::Type(ref type_item) => {
                            matches!(type_item.ty.as_ref(), Type::Ptr(_))
                        }
                        TypedefKind::Use(ref ty) => {
                            matches!(**ty, crate::minisyn::Type(Type::Ptr(_)))
                        }
                    };
                    match typedef_target {
                        Some(target) => {
                            byvalue_checker.results.insert(
                                name.clone(),
                                StructDetails::new(PodState::IsAlias(target)),
                            );
                        }
                        None if target_is_pointer => {
                            byvalue_checker
                                .results
                                .insert(name.clone(), StructDetails::new(PodState::IsPod));
                        }
                        None => byvalue_checker.ingest_nonpod_type(name.clone()),
                    }
                }
                Api::Enum { .. } | Api::ExternCppType { pod: true, .. } => {
                    byvalue_checker
                        .results
                        .insert(api.name().clone(), StructDetails::new(PodState::IsPod));
                }
                _ => {}
            }
        }
        for api in apis.iter() {
            if let Api::Struct { details, .. } = api {
                byvalue_checker.ingest_struct(&details.item, api.name().get_namespace())
            }
        }
        let pod_requests = config
            .get_pod_requests()
            .iter()
            .map(|ty| QualifiedName::new_from_cpp_name(ty))
            .collect();
        byvalue_checker
            .satisfy_requests(pod_requests)
            .map_err(ConvertErrorFromCpp::UnsafePodType)?;
        Ok(byvalue_checker)
    }

    fn ingest_struct(&mut self, def: &ItemStruct, ns: &Namespace) {
        // For this struct, work out whether it _could_ be safe as a POD.
        let tyname = QualifiedName::new(ns, def.ident.clone().into());
        let mut field_safety_problem = PodState::SafeToBePod;
        let fieldlist = Self::get_field_types(def);
        for ty_id in &fieldlist {
            match self.results.get(ty_id) {
                None if ty_id.get_final_item() == "__BindgenUnionField" => {
                    field_safety_problem = PodState::UnsafeToBePod(format!(
                        "Type {tyname} could not be POD because it is a union"
                    ));
                    break;
                }
                None if ty_id.get_final_item() == "__BindgenBitfieldUnit" => {
                    field_safety_problem = PodState::UnsafeToBePod(format!(
                        "Type {tyname} could not be POD because it is a bitfield"
                    ));
                    break;
                }
                None => {
                    field_safety_problem = PodState::UnsafeToBePod(format!(
                        "Type {tyname} could not be POD because its dependent type {ty_id} isn't known"
                    ));
                    break;
                }
                Some(deets) => {
                    if let PodState::UnsafeToBePod(reason) = &deets.state {
                        let new_reason = format!("Type {tyname} could not be POD because its dependent type {ty_id} isn't safe to be POD. Because: {reason}");
                        field_safety_problem = PodState::UnsafeToBePod(new_reason);
                        break;
                    }
                }
            }
        }
        if Self::has_vtable(def) {
            let reason =
                format!("Type {tyname} could not be POD because it has virtual functions.");
            field_safety_problem = PodState::UnsafeToBePod(reason);
        }
        let mut my_details = StructDetails::new(field_safety_problem);
        my_details.dependent_structs = fieldlist;
        self.results.insert(tyname, my_details);
    }

    fn ingest_nonpod_type(&mut self, tyname: QualifiedName) {
        let new_reason = format!("Type {tyname} is a typedef to a complex type");
        self.results.insert(
            tyname,
            StructDetails::new(PodState::UnsafeToBePod(new_reason)),
        );
    }

    fn satisfy_requests(&mut self, mut requests: Vec<QualifiedName>) -> Result<(), String> {
        // Typedefs whose target hasn't settled yet, and which we've therefore
        // put back on the queue behind that target. Meeting the same typedef
        // here twice means its target still isn't settled after we asked for
        // it, i.e. the chain of typedefs is circular and never will settle, so
        // we must complain rather than spin round for ever.
        let mut aliases_awaiting_target: HashSet<QualifiedName> = HashSet::new();
        while let Some(ty_id) = requests.pop() {
            let deets = self.results.get_mut(&ty_id);
            let mut alias_to_consider = None;
            match deets {
                None => {
                    return Err(format!(
                        "Unable to make {ty_id} POD because we never saw a struct definition"
                    ))
                }
                Some(deets) => match &deets.state {
                    PodState::UnsafeToBePod(error_msg) => return Err(error_msg.clone()),
                    PodState::IsPod => {}
                    PodState::SafeToBePod => {
                        deets.state = PodState::IsPod;
                        requests.extend_from_slice(&deets.dependent_structs);
                    }
                    PodState::IsAlias(target_type) => {
                        alias_to_consider = Some(target_type.clone());
                    }
                },
            }
            // Do the following outside the match to avoid borrow checker violation.
            if let Some(alias) = alias_to_consider {
                match self.results.get(&alias).map(|deets| &deets.state) {
                    // The target's state is final, so this typedef is POD
                    // exactly when its target is. Adopt that state and go
                    // round again, which reports any error against the
                    // typedef in the normal way.
                    Some(state @ (PodState::IsPod | PodState::UnsafeToBePod(_))) => {
                        let state = match state {
                            PodState::UnsafeToBePod(reason) => PodState::UnsafeToBePod(format!(
                                "Type {ty_id} could not be POD because it is a typedef to {alias}. Because: {reason}"
                            )),
                            state => state.clone(),
                        };
                        self.results
                            .get_mut(&ty_id)
                            .expect("we matched on this entry a moment ago")
                            .state = state;
                        requests.push(ty_id);
                    }
                    // The target is a struct nobody has asked about yet, or
                    // another typedef: settle it first, then come back to
                    // this one. We pop from the back, so the target has to go
                    // on last.
                    Some(PodState::SafeToBePod | PodState::IsAlias(_)) => {
                        if !aliases_awaiting_target.insert(ty_id.clone()) {
                            return Err(format!(
                                "Unable to make {ty_id} POD because it is part of a circular chain of typedefs"
                            ));
                        }
                        requests.push(ty_id);
                        requests.push(alias);
                    }
                    // Every struct, enum and typedef we know of is already in
                    // `results` by now, so a target we can't find is one we
                    // never generated - blocklisted, or ignored earlier on.
                    None => {
                        return Err(format!(
                            "Unable to make {ty_id} POD because it is a typedef to {alias}, which we know nothing about"
                        ))
                    }
                }
            }
        }
        Ok(())
    }

    /// Return whether a given type is POD (i.e. can be represented by value in Rust) or not.
    /// Unless we've got a definite record that it _is_, we return false.
    /// Some types won't be in our `results` map. For example: (a) AutocxxConcrete types
    /// which we've synthesized; (b) types we couldn't parse but returned ignorable
    /// errors so that we could continue. Assume non-POD for all such cases.
    pub fn is_pod(&self, ty_id: &QualifiedName) -> bool {
        matches!(
            self.results.get(ty_id),
            Some(StructDetails {
                state: PodState::IsPod,
                dependent_structs: _,
            })
        )
    }

    /// This is a miniature version of the analysis in `super::get_struct_field_types`.
    /// It would be nice to unify them. However, this version only cares about spotting
    /// fields which may be non-POD, so can largely concern itself with just `Type::Path`
    /// fields.
    fn get_field_types(def: &ItemStruct) -> Vec<QualifiedName> {
        let mut results = Vec::new();
        for f in &def.fields {
            let fty = &f.ty;
            if let Type::Path(p) = fty {
                results.push(QualifiedName::from_type_path(p));
            }
            // TODO handle anything else which bindgen might spit out, e.g. arrays?
        }
        results
    }

    fn has_vtable(def: &ItemStruct) -> bool {
        for f in &def.fields {
            if f.ident.as_ref().map(|id| id == "vtable_").unwrap_or(false) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{ByValueChecker, PodState, StructDetails};
    use crate::minisyn::ItemStruct;
    use crate::types::{Namespace, QualifiedName};
    use syn::parse_quote;

    fn ty_from_ident(id: &syn::Ident) -> QualifiedName {
        QualifiedName::new_from_cpp_name(&id.to_string())
    }

    /// Record `name` as a typedef to `target`, as `new_from_apis` does for an
    /// `Api::Typedef`.
    fn add_alias(bvc: &mut ByValueChecker, name: &str, target: &str) -> QualifiedName {
        let name = QualifiedName::new_from_cpp_name(name);
        bvc.results.insert(
            name.clone(),
            StructDetails::new(PodState::IsAlias(QualifiedName::new_from_cpp_name(target))),
        );
        name
    }

    #[test]
    fn test_primitive_by_itself() {
        let bvc = ByValueChecker::new();
        let t_id = QualifiedName::new_from_cpp_name("u32");
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_primitives() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Foo {
                a: i32,
                b: i64,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_nested_primitives() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Foo {
                a: i32,
                b: i64,
            }
        };
        bvc.ingest_struct(&t, &Namespace::new());
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: Foo,
                b: i64,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_with_up() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: cxx::UniquePtr<CxxString>,
                b: i64,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_with_cxxstring() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: CxxString,
                b: i64,
            }
        };
        let t_id = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        assert!(bvc.satisfy_requests(vec![t_id]).is_err());
    }

    #[test]
    fn test_typedef_chain_to_primitive() {
        let mut bvc = ByValueChecker::new();
        let first = add_alias(&mut bvc, "first", "u32");
        let second = add_alias(&mut bvc, "second", "first");
        let third = add_alias(&mut bvc, "third", "second");
        bvc.satisfy_requests(vec![third.clone()]).unwrap();
        assert!(bvc.is_pod(&third));
        assert!(bvc.is_pod(&second));
        assert!(bvc.is_pod(&first));
    }

    #[test]
    fn test_typedef_to_struct_makes_both_pod() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bob {
                a: u32,
            }
        };
        let bob = ty_from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        let horace = add_alias(&mut bvc, "Horace", "Bob");
        bvc.satisfy_requests(vec![horace.clone()]).unwrap();
        assert!(bvc.is_pod(&horace));
        // The struct behind the alias has to be POD as well, or we'd emit an
        // alias to an opaque type.
        assert!(bvc.is_pod(&bob));
    }

    #[test]
    fn test_typedef_to_non_pod_struct_is_rejected() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bob {
                a: CxxString,
            }
        };
        bvc.ingest_struct(&t, &Namespace::new());
        let horace = add_alias(&mut bvc, "Horace", "Bob");
        assert!(bvc.satisfy_requests(vec![horace]).is_err());
    }

    #[test]
    fn test_circular_typedefs_are_rejected() {
        // Such a cycle can't be written in C++, but we must terminate rather
        // than chase it round for ever if bindgen ever hands us one.
        let mut bvc = ByValueChecker::new();
        let a = add_alias(&mut bvc, "A", "B");
        add_alias(&mut bvc, "B", "A");
        let err = bvc.satisfy_requests(vec![a]).unwrap_err();
        assert!(
            format!("{err:?}").contains("circular"),
            "error should name the cycle, was: {err:?}"
        );
    }

    #[test]
    fn test_self_referential_typedef_is_rejected() {
        let mut bvc = ByValueChecker::new();
        let a = add_alias(&mut bvc, "A", "A");
        let err = bvc.satisfy_requests(vec![a]).unwrap_err();
        assert!(
            format!("{err:?}").contains("circular"),
            "error should name the cycle, was: {err:?}"
        );
    }

    #[test]
    fn test_typedef_to_unknown_type_is_rejected() {
        let mut bvc = ByValueChecker::new();
        let a = add_alias(&mut bvc, "A", "SomethingWeNeverSaw");
        let err = bvc.satisfy_requests(vec![a]).unwrap_err();
        assert!(
            format!("{err:?}").contains("SomethingWeNeverSaw"),
            "error should name the missing target, was: {err:?}"
        );
    }
}
