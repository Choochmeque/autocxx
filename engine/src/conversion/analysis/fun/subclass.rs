// Copyright 2021 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ops::DerefMut;

use indexmap::map::IndexMap as HashMap;

use syn::{parse_quote, FnArg, PatType, Type, TypePtr};

use crate::conversion::analysis::fun::{ReceiverMutability, UnsafePolicy};
use crate::conversion::analysis::pod::PodPhase;
use crate::conversion::api::{
    CppVisibility, FuncToConvert, Provenance, RustSubclassFnDetails, SubclassConstructorDetails,
    SubclassName, SuperclassBinding, SuperclassMethod, UnsafetyNeeded,
};
use crate::conversion::apivec::ApiVec;
use crate::conversion::parse::CppRefQualifier;
use crate::conversion::ConvertErrorFromCpp;
use crate::conversion::CppEffectiveName;
use crate::minisyn::{minisynize_punctuated, Ident};
use crate::parse_callbacks::CppOriginalName;
use crate::vendored_bindgen::callbacks::ExceptionSpecification;
use crate::{
    conversion::{
        analysis::fun::function_wrapper::{
            CppExceptionSpecification, CppFunction, CppFunctionBody, CppFunctionKind,
            TypeConversionPolicy,
        },
        api::{Api, ApiName},
    },
    types::{make_ident, Namespace, QualifiedName},
};

use super::{FnAnalysis, FnPrePhase1};

pub(super) fn subclasses_by_superclass(
    apis: &ApiVec<PodPhase>,
) -> HashMap<QualifiedName, Vec<SubclassName>> {
    let mut subclasses_per_superclass: HashMap<QualifiedName, Vec<SubclassName>> = HashMap::new();

    for api in apis.iter() {
        if let Api::Subclass {
            name, superclass, ..
        } = api
        {
            subclasses_per_superclass
                .entry(superclass.clone())
                .or_default()
                .push(name.clone());
        }
    }
    subclasses_per_superclass
}

/// What a subclass override has to say about exceptions, given the
/// specification C++ declared on the superclass virtual method it overrides.
///
/// C++ requires an override to allow no more exceptions than the method it
/// overrides, so only a non-throwing specification constrains it; an override
/// of a method which may throw needs none. A specification libclang reports
/// without resolving cannot be answered either way - writing `noexcept` would
/// promise more than the superclass does, and omitting it would be ill-formed
/// if the superclass promised anything - so it refuses instead of guessing.
pub(super) fn override_exception_specification(
    superclass_method: ExceptionSpecification,
) -> Result<CppExceptionSpecification, ConvertErrorFromCpp> {
    match superclass_method {
        // Non-throwing, however C++ spelled it. `noexcept` satisfies an
        // inherited `throw()` and an inherited `__attribute__((nothrow))`
        // alike.
        ExceptionSpecification::BasicNoexcept
        | ExceptionSpecification::DynamicNone
        | ExceptionSpecification::NoThrow => Ok(CppExceptionSpecification::Noexcept),
        // Allows every exception, so the override is unconstrained.
        ExceptionSpecification::None | ExceptionSpecification::MsAny => {
            Ok(CppExceptionSpecification::None)
        }
        // `throw(A, B)` restricts the override to A and B, and libclang reports
        // the kind without the types. Writing no specification passes an
        // ordinary compiler's check only because it fails it - one in
        // Microsoft-compatibility mode accepts it with a warning - so the
        // restriction would be lost quietly there.
        ExceptionSpecification::Dynamic
        // `noexcept(expr)`, reported without saying which way the operand
        // resolved, and a specification clang has not computed or instantiated.
        // A defaulted member's specification is among those clang computes only
        // on demand, and a defaulted member can be virtual: `virtual A&
        // operator=(const A&) = default` reports `Unevaluated`.
        | ExceptionSpecification::ComputedNoexcept
        | ExceptionSpecification::Unevaluated
        | ExceptionSpecification::Uninstantiated
        | ExceptionSpecification::Unparsed => {
            Err(ConvertErrorFromCpp::UnreproducibleExceptionSpecification)
        }
    }
}

pub(super) fn create_subclass_fn_wrapper(
    sub: &SubclassName,
    super_fn_name: &QualifiedName,
    fun: &FuncToConvert,
) -> Box<FuncToConvert> {
    let self_ty = Some(sub.cpp());
    Box::new(FuncToConvert {
        synthesized_this_type: self_ty.clone(),
        self_ty,
        ident: super_fn_name.get_final_ident(),
        doc_attrs: fun.doc_attrs.clone(),
        inputs: fun.inputs.clone(),
        output: fun.output.clone(),
        vis: fun.vis.clone(),
        virtualness: None,
        cpp_vis: CppVisibility::Public,
        special_member: None,
        method_kind: None,
        original_name: None,
        add_to_trait: fun.add_to_trait.clone(),
        is_deleted: fun.is_deleted,
        deprecation: fun.deprecation.clone(),
        synthetic_cpp: None,
        provenance: Provenance::SynthesizedOther,
        variadic: fun.variadic,
        // We're wrapping `Sub::foo_super`, a method autocxx generates itself
        // and never ref-qualifies. We carry the superclass method's qualifier
        // across regardless, because `Sub::foo_super`'s body calls the
        // superclass method on an lvalue and so inherits its restrictions: if
        // that method is `&&`-qualified, this wrapper has to go, and the
        // subclass override goes with it as a dependent. Only non-pure virtual
        // methods reach here; a pure virtual one has no `_super` helper, and
        // its override survives (and has to, or the subclass stays abstract).
        ref_qualifier: fun.ref_qualifier,
        // `Sub::foo_super` calls the superclass method rather than overriding
        // it, so C++ puts no specification on it whatever the superclass
        // method promises.
        exception_specification: ExceptionSpecification::None,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn create_subclass_trait_item(
    name: ApiName,
    analysis: &FnAnalysis,
    superclass_analysis: &FnAnalysis,
    receiver_mutability: &ReceiverMutability,
    receiver: QualifiedName,
    has_super_helper: bool,
    unsafe_policy: &UnsafePolicy,
) -> Api<FnPrePhase1> {
    let requires_unsafe = if matches!(unsafe_policy, UnsafePolicy::AllFunctionsUnsafe) {
        UnsafetyNeeded::Always
    } else {
        UnsafetyNeeded::from_param_details(&analysis.param_details, false)
    };
    // `analysis` describes the shape the trait uses, which is the simplified
    // one we give subclasses; `superclass_analysis` describes the binding the
    // superclass got for the very same C++ method. Note down how to call the
    // latter. Whether the superclass ends up with that binding at all isn't
    // known until codegen, which checks before making any use of this.
    let superclass_binding = SuperclassBinding {
        // A non-POD return value is handed back to Rust by constructing it
        // into a placement parameter, which codegen turns into an `impl New`
        // return type. The trait's simplified shape always uses a `UniquePtr`
        // instead.
        returns_new: superclass_analysis
            .param_details
            .iter()
            .any(|pd| pd.is_placement_return_destination),
    };
    Api::SubclassTraitItem {
        name,
        details: SuperclassMethod {
            name: make_ident(&analysis.rust_name),
            params: minisynize_punctuated(analysis.params.clone()),
            param_conversions: analysis
                .param_details
                .iter()
                .map(|pd| pd.conversion.clone())
                .collect(),
            ret_type: analysis.ret_type.clone(),
            ret_conversion: analysis.ret_conversion.clone(),
            receiver_mutability: *receiver_mutability,
            requires_unsafe,
            has_super_helper,
            receiver,
            superclass_binding,
        },
    }
}

// A note on `UnsafePolicy::ReferencesWrappedAllFunctionsSafe`, which turns
// every C++ reference into a pointer at the bridge and a `CppRef`/`CppMutRef`
// in Rust. A subclass is the one place where calls run from C++ into Rust, so
// each conversion has to be read backwards there:
//
// * the C++ signatures - the subclass constructor and the `_super` helpers,
//   both generated in the `CppCallsCpp` direction, and the virtual overrides,
//   generated in `CppCallsRust` - are spelled from the reference the pointer
//   stands for, not from the pointer. See `TypeConversionPolicy`'s
//   `converted_type` and `unconverted_type`.
// * the `extern "Rust"` function each override reaches Rust by has a pointer
//   parameter, so cxx requires it to be declared unsafe; the `_methods` trait
//   item it forwards to is safe, because by then the pointer is a `CppRef`
//   again. See `TypeConversionPolicy::inverse_rust_conversion`.
#[allow(clippy::too_many_arguments)]
pub(super) fn create_subclass_function(
    sub: &SubclassName,
    analysis: &super::FnAnalysis,
    name: &ApiName,
    receiver_mutability: &ReceiverMutability,
    superclass: &QualifiedName,
    dependencies: Vec<QualifiedName>,
    unsafe_policy: &UnsafePolicy,
    ref_qualifier: CppRefQualifier,
    exception_specification: CppExceptionSpecification,
    cpp_super_fn_name: Option<Ident>,
    superclass_fn_is_deprecated: bool,
) -> Api<FnPrePhase1> {
    let cpp = sub.cpp();
    let holder_name = sub.holder();
    let rust_call_name = format!(
        "{}_{}",
        sub.0.name.get_final_item(),
        name.name.get_final_item()
    );
    let params = std::iter::once(crate::minisyn::FnArg(parse_quote! {
        me: & #holder_name
    }))
    .chain(analysis.params.iter().skip(1).cloned())
    .collect();
    let kind = if matches!(receiver_mutability, ReceiverMutability::Mutable) {
        CppFunctionKind::Method
    } else {
        CppFunctionKind::ConstMethod
    };
    let argument_conversion = analysis
        .param_details
        .iter()
        .skip(1)
        .map(|p| p.conversion.clone())
        .collect();
    let requires_unsafe = if matches!(unsafe_policy, UnsafePolicy::AllFunctionsUnsafe) {
        UnsafetyNeeded::Always
    } else {
        UnsafetyNeeded::from_param_details(&analysis.param_details, false)
    };
    Api::RustSubclassFn {
        name: ApiName::new_in_root_namespace(make_ident(rust_call_name.clone())),
        subclass: sub.clone(),
        details: Box::new(RustSubclassFnDetails {
            params,
            ret: analysis.ret_type.clone(),
            method_name: make_ident(&analysis.rust_name),
            cpp_impl: CppFunction {
                payload: CppFunctionBody::FunctionCall(
                    Namespace::new(),
                    CppEffectiveName::from_cxxbridge_name(&rust_call_name),
                ),
                wrapper_function_name: make_ident(&analysis.rust_name),
                original_cpp_name: name.cpp_name(),
                return_conversion: analysis.ret_conversion.clone(),
                argument_conversion,
                kind,
                pass_obs_field: true,
                qualification: Some(cpp),
                // This method overrides the superclass's, so if that one is
                // ref-qualified then this one must be too - and if it promises
                // not to throw, this one must promise the same or C++ rejects
                // it.
                ref_qualifier,
                exception_specification,
                is_virtual_override: true,
                // The override's own body calls into Rust and names the
                // superclass method nowhere, but the `_super` helper generated
                // from this same `CppFunction` calls it by name, and declaring
                // an override of a deprecated method is itself a use of it.
                calls_deprecated: superclass_fn_is_deprecated,
            },
            superclass: superclass.clone(),
            receiver_mutability: *receiver_mutability,
            dependencies,
            requires_unsafe,
            cpp_super_fn_name,
        }),
    }
}

pub(super) fn create_subclass_constructor(
    sub: SubclassName,
    analysis: &FnAnalysis,
    sup: &QualifiedName,
    fun: &FuncToConvert,
) -> (Box<FuncToConvert>, ApiName) {
    let holder = sub.holder();
    let cpp = sub.cpp();
    let wrapper_function_name = cpp.get_final_ident();
    let initial_arg = TypeConversionPolicy::new_unconverted(parse_quote! {
        rust::Box< #holder >
    });
    let args = std::iter::once(initial_arg).chain(
        analysis
            .param_details
            .iter()
            .skip(1) // skip placement new destination
            .map(|aa| aa.conversion.clone()),
    );
    let cpp_impl = CppFunction {
        payload: CppFunctionBody::ConstructSuperclass(sup.to_cpp_name()),
        wrapper_function_name,
        return_conversion: None,
        argument_conversion: args.collect(),
        kind: CppFunctionKind::SynthesizedConstructor,
        pass_obs_field: false,
        qualification: Some(cpp.clone()),
        original_cpp_name: CppEffectiveName::from_fully_qualified_name_for_subclass(
            &cpp.to_cpp_name(),
        ),
        ref_qualifier: CppRefQualifier::None,
        // A constructor overrides nothing.
        exception_specification: CppExceptionSpecification::None,
        is_virtual_override: false,
        // The body calls the superclass constructor by name.
        calls_deprecated: fun.deprecation.is_some(),
    };
    let subclass_constructor_details = Box::new(SubclassConstructorDetails {
        subclass: sub.clone(),
        is_trivial: analysis.param_details.len() == 1, // just placement new
        // destination, no other parameters
        cpp_impl,
    });
    let subclass_constructor_name =
        make_ident(format!("{}_{}", cpp.get_final_item(), cpp.get_final_item()));
    let mut existing_params = fun.inputs.clone();
    if let Some(FnArg::Typed(PatType { ty, .. })) =
        existing_params.first_mut().map(DerefMut::deref_mut)
    {
        if let Type::Ptr(TypePtr { elem, .. }) = &mut **ty {
            **elem = Type::Path(sub.cpp().to_type_path());
        } else {
            panic!("Unexpected self type parameter when creating subclass constructor");
        }
    } else {
        panic!("Unexpected self type parameter when creating subclass constructor");
    }
    let mut existing_params = existing_params.into_iter();
    let self_param = existing_params.next();
    let boxed_holder_param: FnArg = parse_quote! {
        peer: rust::Box<#holder>
    };
    let inputs = self_param
        .into_iter()
        .chain(std::iter::once(boxed_holder_param.into()))
        .chain(existing_params)
        .collect();
    let maybe_wrap = Box::new(FuncToConvert {
        ident: subclass_constructor_name.clone(),
        doc_attrs: fun.doc_attrs.clone(),
        inputs,
        output: fun.output.clone(),
        vis: fun.vis.clone(),
        virtualness: None,
        cpp_vis: CppVisibility::Public,
        special_member: fun.special_member,
        method_kind: None,
        original_name: None,
        synthesized_this_type: Some(cpp.clone()),
        self_ty: Some(cpp),
        add_to_trait: None,
        is_deleted: fun.is_deleted,
        deprecation: fun.deprecation.clone(),
        synthetic_cpp: None,
        provenance: Provenance::SynthesizedSubclassConstructor(subclass_constructor_details),
        variadic: fun.variadic,
        ref_qualifier: CppRefQualifier::None,
        exception_specification: ExceptionSpecification::None,
    });
    let subclass_constructor_name = ApiName::new_with_cpp_name(
        &Namespace::new(),
        subclass_constructor_name,
        Some(CppOriginalName::from_final_item_of_generated_cpp_name(
            &sub.cpp(),
        )),
    );
    (maybe_wrap, subclass_constructor_name)
}

#[cfg(test)]
mod tests {
    use super::{override_exception_specification, CppExceptionSpecification};
    use crate::conversion::ConvertErrorFromCpp;
    use crate::vendored_bindgen::callbacks::ExceptionSpecification;

    /// Every kind libclang reports, including the ones no portable C++ fixture
    /// can reach: `throw(X)` is gone in C++17 and `throw()` in C++20,
    /// `throw(...)` is a Microsoft extension and `__attribute__((nothrow))` is
    /// not MSVC's spelling, so a fixture written with any of them would not
    /// compile on every platform autocxx's own tests run on.
    #[test]
    fn every_specification_kind_decides_one_way_or_refuses() {
        for (specification, expected) in [
            (
                ExceptionSpecification::BasicNoexcept,
                Ok(CppExceptionSpecification::Noexcept),
            ),
            (
                ExceptionSpecification::DynamicNone,
                Ok(CppExceptionSpecification::Noexcept),
            ),
            (
                ExceptionSpecification::NoThrow,
                Ok(CppExceptionSpecification::Noexcept),
            ),
            (
                ExceptionSpecification::None,
                Ok(CppExceptionSpecification::None),
            ),
            (
                ExceptionSpecification::MsAny,
                Ok(CppExceptionSpecification::None),
            ),
            (
                ExceptionSpecification::Dynamic,
                Err(ConvertErrorFromCpp::UnreproducibleExceptionSpecification),
            ),
            (
                ExceptionSpecification::Unevaluated,
                Err(ConvertErrorFromCpp::UnreproducibleExceptionSpecification),
            ),
            (
                ExceptionSpecification::ComputedNoexcept,
                Err(ConvertErrorFromCpp::UnreproducibleExceptionSpecification),
            ),
            (
                ExceptionSpecification::Uninstantiated,
                Err(ConvertErrorFromCpp::UnreproducibleExceptionSpecification),
            ),
            (
                ExceptionSpecification::Unparsed,
                Err(ConvertErrorFromCpp::UnreproducibleExceptionSpecification),
            ),
        ] {
            let got = override_exception_specification(specification);
            match (&got, &expected) {
                (Ok(got), Ok(expected)) => assert_eq!(got, expected, "{specification:?}"),
                (Err(got), Err(expected)) => {
                    assert_eq!(got.to_string(), expected.to_string(), "{specification:?}")
                }
                _ => panic!("{specification:?} answered {got:?}"),
            }
        }
    }
}
