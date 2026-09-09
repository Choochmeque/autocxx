// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;
use syn::parse_quote;

use crate::{
    conversion::{
        api::{Api, ApiName, CppVisibility, FuncToConvert, HolderSurface, Provenance, TypeKind},
        apivec::ApiVec,
        codegen_cpp::type_to_cpp::CppNameMap,
        error_reporter::convert_item_apis,
        parse::CppRefQualifier,
        ConvertErrorFromCpp,
    },
    known_types::known_types,
    minisyn::{Attribute, Type},
    types::{make_ident, Namespace, QualifiedName},
};

use super::{
    fun::function_wrapper::{CppFunctionBody, CppFunctionKind},
    pod::{PodAnalysis, PodPhase},
    type_converter::concrete_type_ident,
};

/// Expose each [`Api::Static`] - a C++ variable with static storage duration -
/// in the shape its type allows.
///
/// A variable is re-exported by simply `use`ing `bindgen`'s declaration of it,
/// so the type the user ends up seeing is `bindgen`'s. That's only the same
/// type as the one we expose in our output mod when the type is POD; for
/// anything else our output mod holds an opaque wrapper instead, and
/// re-exporting `bindgen`'s raw struct would both leak our internals and let
/// safe Rust read a type it has no business reading. Enums are fine too, since
/// we re-export `bindgen`'s enum unchanged.
///
/// Such a variable is reached through a generated getter instead, which hands
/// back an opaque holder standing for a `const` reference to it. Nothing is
/// copied: the variable's type need not be copy-constructible, and what the
/// caller reads is the object itself rather than a snapshot of it taken when
/// the getter ran. See google/autocxx#94.
///
/// The holder is `const` whether or not C++ wrote `const` on the variable.
/// Writing to a C++ global from Rust would need a shape of its own - a
/// `Pin<&mut>` receiver, and an answer to what makes that sound for an object
/// any thread may be touching - and #94 asks for neither, so a mutable global
/// is exposed for reading like every other one.
pub(crate) fn expose_statics(apis: ApiVec<PodPhase>) -> ApiVec<PodPhase> {
    let representable = representable_static_types(&apis);
    let name_map = CppNameMap::new_for_analysis(&apis);
    // Every concrete type autocxx already has, so that two variables of one
    // type share a holder, and so do a holder of ours and one the type
    // converter goes on to want for the same C++ type.
    let mut holders: HashMap<String, Option<QualifiedName>> = apis
        .iter()
        .filter_map(|api| match api {
            // `None` for one whose surface is not ours: it is the same C++
            // type, so a second holder for it would be a second typedef of one
            // thing, and it has no `get` for a getter to hand over.
            Api::ConcreteType {
                cpp_definition,
                holder_surface,
                ..
            } => Some((
                cpp_definition.clone(),
                matches!(holder_surface, Some(HolderSurface::ConstRef { .. }))
                    .then(|| api.name().clone()),
            )),
            _ => None,
        })
        .collect();
    let mut names_taken: HashSet<QualifiedName> =
        apis.iter().map(|api| api.name().clone()).collect();
    let holdable = holdable_static_types(&apis);
    let mut new_holders = ApiVec::new();
    let mut results = ApiVec::new();
    convert_item_apis(apis, &mut results, |api| {
        let needs_getter = matches!(&api,
            Api::Static { cpp_ty: Some(cpp_ty), .. } if !representable.contains(cpp_ty));
        if !needs_getter {
            return Ok(Box::new(std::iter::once(api)));
        }
        let Api::Static {
            name,
            cpp_ty: Some(cpp_ty),
            name_is_cpp_name,
        } = api
        else {
            unreachable!("just matched the same shape")
        };
        if !name_is_cpp_name {
            return Err(ConvertErrorFromCpp::StaticDataWithUnknownCppName(
                name.name.clone(),
            ));
        }
        // Asked before `holdable`, which answers yes for every known type: a
        // holder over a `std::string_view` would have accessors naming a Rust
        // type there is none of, and the reason is the one the view's own
        // diagnostic gives rather than anything about holders.
        if known_types().is_string_view(&cpp_ty) {
            return Err(ConvertErrorFromCpp::StringViewOutOfCpp);
        }
        if !holdable.contains(&cpp_ty) {
            return Err(ConvertErrorFromCpp::StaticDataOfUnholdableType(cpp_ty));
        }
        let holder = holder_for(
            &cpp_ty,
            &name_map,
            &mut holders,
            &mut names_taken,
            &mut new_holders,
        )?;
        Ok(Box::new(std::iter::once(getter(name, &holder))))
    });
    results.append(&mut new_holders);
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

/// The C++ types a holder can be built over: the ones the `cxx::bridge` goes
/// on to declare, and which its `get` can therefore hand a pointer to.
///
/// A variable's type reaches us as the [`QualifiedName`] `bindgen` wrote in
/// the declaration and nothing more, so what is *not* here matters as much as
/// what is. A name with template arguments arrives without them - a
/// `Box<int>` variable is recorded as `Box` - and an alias arrives as itself,
/// where the bridge declares the type it aliases. Both would name something
/// the bridge has not declared, so both are turned down rather than lowered.
fn holdable_static_types(apis: &ApiVec<PodPhase>) -> HashSet<QualifiedName> {
    apis.iter()
        .filter_map(|api| match api {
            Api::Struct {
                name,
                analysis: PodAnalysis {
                    num_generics: 0, ..
                },
                ..
            } => Some(name.name.clone()),
            _ => None,
        })
        // Every name autocxx knows a C++ type by, POD-safe or not: a
        // `std::string` variable is exactly what this is for, and the bridge
        // declares such a type whether or not Rust may hold one by value.
        .chain(known_types().get_pod_safe_types().map(|(tn, _)| tn))
        .collect()
}

/// The opaque holder standing for a `const` reference to a variable of type
/// `cpp_ty`, made if this is the first variable of that type and found if it is
/// not.
///
/// The C++ definition is written the way `bindgen` writes a template
/// instantiation rather than as a string, so that the C++ codegen renders it
/// with the nesting and the unshadowing aliases it knows about and this phase
/// does not - and so that the string it does derive here is the same hash key
/// the type converter would derive for the same instantiation.
fn holder_for(
    cpp_ty: &QualifiedName,
    name_map: &CppNameMap,
    holders: &mut HashMap<String, Option<QualifiedName>>,
    names_taken: &mut HashSet<QualifiedName>,
    new_holders: &mut ApiVec<PodPhase>,
) -> Result<QualifiedName, ConvertErrorFromCpp> {
    let payload = cpp_ty.to_type_path();
    let rs_definition: Type = parse_quote! {
        std::reference_wrapper < __bindgen_marker_Const < #payload > >
    };
    let cpp_definition = name_map.type_to_cpp(&rs_definition)?;
    // Only a holder which already carries this surface can be reused: a
    // concrete type for the same C++ specialization made by the type converter
    // - which a header can ask for by writing the specialization out - carries
    // no accessors, and handing that one back would produce a getter whose
    // result has no `get`.
    if let Some(existing) = holders.get(&cpp_definition) {
        return existing
            .clone()
            .ok_or_else(|| ConvertErrorFromCpp::StaticDataHolderAlreadyTaken(cpp_ty.clone()));
    }
    let stem = concrete_type_ident(&cpp_definition);
    let mut name = QualifiedName::new(&Namespace::new(), make_ident(&stem));
    let mut n = 1;
    while names_taken.contains(&name) {
        name = QualifiedName::new(&Namespace::new(), make_ident(format!("{stem}{n}")));
        n += 1;
    }
    names_taken.insert(name.clone());
    holders.insert(cpp_definition.clone(), Some(name.clone()));
    new_holders.push(Api::ConcreteType {
        name: ApiName::new_from_qualified_name(name.clone()),
        rs_definition: Some(Box::new(rs_definition)),
        cpp_definition,
        holder_surface: Some(HolderSurface::ConstRef {
            payload: Box::new(syn::Type::Path(payload).into()),
            deps: std::iter::once(cpp_ty.clone()).collect(),
        }),
        // Filled in by `decorate_types_with_constructor_deps`, as for every
        // other concrete type.
        constructor_and_allocator_deps: Vec::new(),
        // The payload of a `const T&` holder is a type which was itself
        // converted, so an incomplete one was turned down before we got here.
        incomplete_argument: None,
    });
    Ok(name)
}

/// The getter itself, which the user calls by the variable's own name.
///
/// It takes the variable's `ApiName`, so a `generate!` directive naming the
/// variable is answered by this and the user writes `ffi::BOB()` where a POD
/// variable would have been `ffi::BOB`.
fn getter(name: ApiName, holder: &QualifiedName) -> Api<PodPhase> {
    let variable = name.name.clone();
    let holder_path = holder.to_type_path();
    let doc = format!(
        "Reads the C++ variable `{}`. autocxx generated this getter because \
         the variable's type reaches Rust as an opaque one, which Rust cannot \
         be handed by value; what comes back refers to the variable rather \
         than copying it.",
        variable.to_cpp_name()
    );
    let doc: syn::Attribute = parse_quote! { #[doc = #doc] };
    let doc: Attribute = doc.into();
    Api::Function {
        fun: Box::new(FuncToConvert {
            provenance: Provenance::SynthesizedOther,
            ident: variable.get_final_ident(),
            doc_attrs: vec![doc],
            inputs: Default::default(),
            variadic: false,
            output: parse_quote! { -> #holder_path },
            vis: parse_quote! { pub },
            virtualness: None,
            cpp_vis: CppVisibility::Public,
            special_member: None,
            method_kind: None,
            original_name: None,
            self_ty: None,
            synthesized_this_type: None,
            add_to_trait: None,
            synthetic_cpp: Some((
                CppFunctionBody::VariableRead(variable),
                CppFunctionKind::Function,
            )),
            is_deleted: None,
            deprecation: None,
            ref_qualifier: CppRefQualifier::None,
        }),
        name,
        analysis: (),
    }
}
