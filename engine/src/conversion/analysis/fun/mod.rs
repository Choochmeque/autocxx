// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

mod bridge_name_tracker;
pub(crate) mod function_wrapper;
mod implicit_constructors;
mod overload_tracker;
mod subclass;

use crate::vendored_bindgen::callbacks::Visibility as CppVisibility;
use crate::vendored_bindgen::callbacks::{
    Explicitness, MethodKind as CppMethodKind, SpecialMemberKind, Virtualness,
};
use crate::{
    conversion::{
        analysis::{
            fun::function_wrapper::{BridgePointer, CppFunctionKind},
            type_converter::{self, add_analysis, TypeConversionContext, TypeConverter},
        },
        api::{
            ApiName, CastMutability, FuncToConvert, NestedCppNames, NullPhase, Provenance,
            SubclassName, TraitImplSignature, TraitSynthesis, UnsafetyNeeded,
        },
        apivec::ApiVec,
        convert_error::{ConvertErrorWithContext, ErrorContext, ErrorContextType},
        error_reporter::{convert_apis, report_any_error},
        parse::CppRefQualifier,
        type_helpers::extract_pinned_mutable_reference_type,
        type_helpers::{type_is_reference, unwrap_has_opaque},
        CppEffectiveName, CppOriginalName,
    },
    known_types::known_types,
    minisyn::{minisynize_punctuated, FnArg},
    types::validate_ident_ok_for_rust,
};
use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;

use autocxx_parser::{ExternCppType, IncludeCppConfig, UnsafePolicy};
use function_wrapper::{CppFunction, CppFunctionBody, TypeConversionPolicy, RECEIVER_ARG_NAME};
use itertools::Itertools;
use proc_macro2::Span;
use quote::{quote, ToTokens};
use syn::{
    parse_quote, punctuated::Punctuated, token::Comma, Ident, Pat, PatType, ReturnType, Type,
    TypePtr, TypeReference, Visibility,
};

use crate::{
    conversion::{
        api::{AnalysisPhase, Api, TypeKind},
        ConvertErrorFromCpp,
    },
    types::{make_ident, validate_ident_ok_for_cxx, Namespace, QualifiedName},
};

use self::{
    bridge_name_tracker::BridgeNameTracker,
    function_wrapper::{
        ForcedRustConversion, PointerCppConversion, PointerRustConversion, WholeCppConversion,
        WholeRustConversion,
    },
    implicit_constructors::{
        discard_deleted_defaulted_members, find_constructors_present, ItemsFound, WhyNoConstructors,
    },
    overload_tracker::OverloadTracker,
    subclass::{
        create_subclass_constructor, create_subclass_fn_wrapper, create_subclass_function,
        create_subclass_trait_item,
    },
};

use super::{
    depth_first::HasFieldsAndBases,
    doc_label::make_doc_attrs,
    pod::{PodAnalysis, PodPhase},
    tdef::TypedefAnalysis,
    type_converter::Annotated,
};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum ReceiverMutability {
    Const,
    Mutable,
}

#[derive(Clone, Debug)]
pub(crate) enum MethodKind {
    Normal,
    Constructor { is_default: bool },
    Static,
    Virtual(ReceiverMutability),
    PureVirtual(ReceiverMutability),
}

#[derive(Clone, Debug)]
pub(crate) enum TraitMethodKind {
    CopyConstructor,
    MoveConstructor,
    Cast,
    Destructor,
    Alloc,
    Dealloc,
}

#[derive(Clone, Debug)]
pub(crate) struct TraitMethodDetails {
    pub(crate) trt: TraitImplSignature,
    pub(crate) avoid_self: bool,
    pub(crate) method_name: crate::minisyn::Ident,
    /// For traits, where we're trying to implement a specific existing
    /// interface, we may need to reorder the parameters to fit that
    /// interface.
    pub(crate) parameter_reordering: Option<Vec<usize>>,
}

#[derive(Clone, Debug)]
pub(crate) enum FnKind {
    Function,
    Method {
        method_kind: MethodKind,
        impl_for: QualifiedName,
    },
    TraitMethod {
        kind: TraitMethodKind,
        /// The name of the type T for which we're implementing a trait,
        /// though we may be actually implementing the trait for &mut T or
        /// similar, so we store more details of both the type and the
        /// method in `details`
        impl_for: QualifiedName,
        details: Box<TraitMethodDetails>,
    },
}

/// Strategy for ensuring that the final, callable, Rust name
/// is what the user originally expected.
#[derive(Clone, Debug)]

pub(crate) enum RustRenameStrategy {
    /// cxx::bridge name matches user expectations
    None,
    /// Even the #[rust_name] attribute would cause conflicts, and we need
    /// to use a 'use XYZ as ABC'
    RenameInOutputMod(crate::minisyn::Ident),
    /// This function requires us to generate a Rust function to do
    /// parameter conversion.
    RenameUsingWrapperFunction,
}

#[derive(Clone, Debug)]
pub(crate) struct FnAnalysis {
    /// Each entry in the cxx::bridge needs to have a unique name, even if
    /// (from the perspective of Rust and C++) things are in different
    /// namespaces/mods.
    pub(crate) cxxbridge_name: crate::minisyn::Ident,
    /// ... so record also the name under which we wish to expose it in Rust.
    pub(crate) rust_name: String,
    /// And also the name of the underlying C++ function if it differs
    /// from the cxxbridge_name.
    pub(crate) cpp_call_name: Option<CppOriginalName>,
    pub(crate) rust_rename_strategy: RustRenameStrategy,
    pub(crate) params: Punctuated<FnArg, Comma>,
    pub(crate) kind: FnKind,
    pub(crate) ret_type: crate::minisyn::ReturnType,
    pub(crate) param_details: Vec<ArgumentAnalysis>,
    pub(crate) ret_conversion: Option<TypeConversionPolicy>,
    pub(crate) requires_unsafe: UnsafetyNeeded,
    pub(crate) vis: Visibility,
    pub(crate) cpp_wrapper: Option<CppFunction>,
    pub(crate) deps: HashSet<QualifiedName>,
    /// Some methods still need to be recorded because we want
    /// to (a) generate the ability to call superclasses, (b) create
    /// subclass entries for them. But we do not want to have them
    /// be externally callable.
    pub(crate) ignore_reason: Result<(), ConvertErrorWithContext>,
    /// Whether this can be called by external code. Not so for
    /// protected methods.
    pub(crate) externally_callable: bool,
    /// Whether we need to generate a Rust-side calling function
    pub(crate) rust_wrapper_needed: bool,
    /// Whether this function may throw C++ exceptions
    pub(crate) may_throw: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ArgumentAnalysis {
    pub(crate) conversion: TypeConversionPolicy,
    pub(crate) name: crate::minisyn::Pat,
    pub(crate) self_type: Option<(QualifiedName, ReceiverMutability)>,
    pub(crate) has_lifetime: bool,
    pub(crate) is_mutable_reference: bool,
    pub(crate) deps: HashSet<QualifiedName>,
    pub(crate) requires_unsafe: UnsafetyNeeded,
    pub(crate) is_placement_return_destination: bool,
}

pub(crate) struct ReturnTypeAnalysis {
    rt: ReturnType,
    conversion: Option<TypeConversionPolicy>,
    was_reference: bool,
    was_mutable_reference: bool,
    /// Whether C++ declared this return an rvalue reference, `T&&`, which
    /// only the type converter can say for certain: written out it arrives as
    /// one of bindgen's markers, but behind a typedef it arrives as an
    /// ordinary path and the alias has to be resolved first.
    was_rvalue_reference: bool,
    /// Whether C++ qualified the return type itself `const`. That qualifier
    /// is part of the function's type, and cxx takes the address of every
    /// function it declares, so a `const int f()` declared to cxx as
    /// `-> c_int` is rejected by C++ at `int (*f$)() = ::f;`. Calling it
    /// through a wrapper of our own sidesteps that. See google/autocxx#1191.
    was_const: bool,
    deps: HashSet<QualifiedName>,
    placement_param_needed: Option<(FnArg, ArgumentAnalysis)>,
}

impl Default for ReturnTypeAnalysis {
    fn default() -> Self {
        Self {
            rt: parse_quote! {},
            conversion: None,
            was_reference: false,
            was_mutable_reference: false,
            was_rvalue_reference: false,
            was_const: false,
            deps: Default::default(),
            placement_param_needed: None,
        }
    }
}

#[derive(std::fmt::Debug)]
pub(crate) struct PodAndConstructorAnalysis {
    pub(crate) pod: PodAnalysis,
    pub(crate) constructors: PublicConstructors,
}

/// What we've worked out about the superclass of a `subclass!()`ed type.
#[derive(std::fmt::Debug)]
pub(crate) struct SubclassAnalysis {
    /// Who C++ lets destroy a superclass instance, or `None` if we never
    /// found a destructor for it at all. A subclass may call a `protected`
    /// destructor, but only a `public` one lets anyone else - in particular
    /// `std::unique_ptr<Superclass>` - do so.
    pub(crate) superclass_destructor_visibility: Option<CppVisibility>,
    /// Whether that destructor is virtual, whether the superclass declares it
    /// or inherits it. A `std::unique_ptr<Superclass>` made from a peer owns a
    /// peer, never a plain superclass, so `delete`ing one runs the wrong
    /// destructor unless this is true. Filled in by
    /// [`crate::conversion::analysis::abstract_types::mark_types_abstract`],
    /// which is where inherited virtualness is worked out; this pass leaves it
    /// `false`.
    pub(crate) superclass_destructor_virtual: bool,
}

/// An analysis phase where we've analyzed each function, but
/// haven't yet determined which constructors/etc. belong to each type.
#[derive(std::fmt::Debug)]
pub(crate) struct FnPrePhase1;

impl AnalysisPhase for FnPrePhase1 {
    type TypedefAnalysis = TypedefAnalysis;
    type StructAnalysis = PodAnalysis;
    type FunAnalysis = FnAnalysis;
    type SubclassAnalysis = ();
}

/// An analysis phase where we've analyzed each function, and identified
/// what implicit constructors/destructors are present in each type.
#[derive(std::fmt::Debug)]
pub(crate) struct FnPrePhase2;

impl AnalysisPhase for FnPrePhase2 {
    type TypedefAnalysis = TypedefAnalysis;
    type StructAnalysis = PodAndConstructorAnalysis;
    type FunAnalysis = FnAnalysis;
    type SubclassAnalysis = ();
}

/// An analysis phase where we've additionally annotated each subclass with
/// its superclass's destructor visibility.
#[derive(std::fmt::Debug)]
pub(crate) struct FnPrePhase3;

impl AnalysisPhase for FnPrePhase3 {
    type TypedefAnalysis = TypedefAnalysis;
    type StructAnalysis = PodAndConstructorAnalysis;
    type FunAnalysis = FnAnalysis;
    type SubclassAnalysis = SubclassAnalysis;
}

#[derive(Debug)]
pub(crate) struct PodAndDepAnalysis {
    pub(crate) pod: PodAnalysis,
    pub(crate) constructor_and_allocator_deps: Vec<QualifiedName>,
    pub(crate) constructors: PublicConstructors,
}

/// Analysis phase after we've finished analyzing functions and determined
/// which constructors etc. belong to them.
#[derive(std::fmt::Debug)]
pub(crate) struct FnPhase;

/// Indicates which kinds of public constructors are known to exist for a type.
#[derive(Debug, Default, Clone)]
pub(crate) struct PublicConstructors {
    pub(crate) move_constructor: bool,
    /// Whether anyone outside the class can destroy one of these.
    /// Beware: `false` also arises for the many types we never analyzed at
    /// all (generics, opaque types, types in anonymous namespaces, ...), so
    /// this may only be used to decide whether to *add* API surface.
    pub(crate) destructor: bool,
    /// Whether we positively established that nobody outside the class may
    /// destroy one of these, because the destructor is private, protected,
    /// deleted, or implicitly deleted by a base or member. Unlike
    /// [`Self::destructor`] this is never set for types we didn't analyze,
    /// so it is safe to use it to *remove* API surface.
    /// See <https://github.com/google/autocxx/issues/829>.
    pub(crate) destructor_inaccessible: bool,
    /// Why C++'s rules withheld each of the constructors we'd otherwise have
    /// offered, where we know and it's worth saying. Purely for documenting
    /// the absence to whoever reads the bindings; nothing is generated from
    /// it. See <https://github.com/google/autocxx/issues/1034>.
    pub(crate) why_no_constructors: WhyNoConstructors,
    /// Whether we left this type without a destructor - and so without an
    /// `impl Drop` - because we worked out that C++ destroys one trivially.
    /// The generated C++ has to say so out loud: see
    /// [`crate::conversion::codegen_cpp::CppCodeGenerator::generate_trivial_destructor_assertion`].
    pub(crate) destructor_omitted_as_trivial: bool,
    /// Whether the class is abstract and no virtual destructor was found for
    /// it, declared or inherited. No object of an abstract class exists, so
    /// every pointer to one points at some derived object; if the destructor
    /// is not virtual, `delete` through that pointer runs the wrong one, which
    /// is undefined behaviour and which clang and cl both diagnose.
    ///
    /// "Not found" rather than "not virtual": a base which was never analyzed,
    /// because it is off the allowlist or because bindgen could not name it,
    /// could be carrying the `virtual` we are looking for. Withdrawing is the
    /// safe direction either way - the compiler refuses the generated code in
    /// the first case and would have accepted it in the second - and what is
    /// generated from this says which of the two it knows.
    ///
    /// Filled in by
    /// [`crate::conversion::analysis::abstract_types::mark_types_abstract`],
    /// which is where abstractness and destructor virtualness are both worked
    /// out; `from_items_found` leaves it `false`.
    pub(crate) abstract_without_virtual_destructor: bool,
}

impl PublicConstructors {
    fn from_items_found(items_found: &ItemsFound, destructor_omitted_as_trivial: bool) -> Self {
        Self {
            move_constructor: items_found.move_constructor.callable_any(),
            destructor: items_found.destructor.callable_any(),
            destructor_inaccessible: !items_found.destructor.callable_any(),
            why_no_constructors: items_found.why_no_constructors.clone(),
            destructor_omitted_as_trivial,
            abstract_without_virtual_destructor: false,
        }
    }
}

impl AnalysisPhase for FnPhase {
    type TypedefAnalysis = TypedefAnalysis;
    type StructAnalysis = PodAndDepAnalysis;
    type FunAnalysis = FnAnalysis;
    type SubclassAnalysis = SubclassAnalysis;
}

/// Whether to allow highly optimized calls because this is a simple Rust->C++ call,
/// or to use a simpler set of policies because this is a subclass call where
/// we may have C++->Rust->C++ etc.
#[derive(Copy, Clone)]
enum TypeConversionSophistication {
    Regular,
    SimpleForSubclasses,
}

pub(crate) struct FnAnalyzer<'a> {
    unsafe_policy: &'a UnsafePolicy,
    extra_apis: ApiVec<NullPhase>,
    type_converter: TypeConverter<'a>,
    bridge_name_tracker: BridgeNameTracker,
    pod_safe_types: HashSet<QualifiedName>,
    moveit_safe_types: HashSet<QualifiedName>,
    config: &'a IncludeCppConfig,
    overload_trackers_by_mod: HashMap<Namespace, OverloadTracker>,
    subclasses_by_superclass: HashMap<QualifiedName, Vec<SubclassName>>,
    nested_type_name_map: HashMap<QualifiedName, String>,
    nested_cpp_names: NestedCppNames<'a>,
    generic_types: HashSet<QualifiedName>,
    types_in_anonymous_namespace: HashSet<QualifiedName>,
    existing_superclass_trait_api_names: HashSet<QualifiedName>,
    cpp_names_taken_on_peer_classes: HashSet<String>,
    force_wrapper_generation: bool,
}

impl<'a> FnAnalyzer<'a> {
    pub(crate) fn analyze_functions(
        apis: ApiVec<PodPhase>,
        unsafe_policy: &'a UnsafePolicy,
        config: &'a IncludeCppConfig,
        force_wrapper_generation: bool,
    ) -> ApiVec<FnPrePhase3> {
        let mut me = Self {
            unsafe_policy,
            extra_apis: ApiVec::new(),
            type_converter: TypeConverter::new(config, &apis),
            bridge_name_tracker: BridgeNameTracker::new(),
            config,
            overload_trackers_by_mod: HashMap::new(),
            pod_safe_types: Self::build_pod_safe_type_set(&apis),
            moveit_safe_types: Self::build_correctly_sized_type_set(&apis),
            subclasses_by_superclass: subclass::subclasses_by_superclass(&apis),
            nested_type_name_map: Self::build_nested_type_map(&apis),
            nested_cpp_names: NestedCppNames::new(config, apis.iter().map(|api| api.name_info())),
            generic_types: Self::build_generic_type_set(&apis),
            existing_superclass_trait_api_names: HashSet::new(),
            cpp_names_taken_on_peer_classes: Self::build_virtual_method_cpp_names(&apis),
            types_in_anonymous_namespace: Self::build_types_in_anonymous_namespace(&apis),
            force_wrapper_generation,
        };
        me.reserve_ideal_names(&apis);
        let mut results = ApiVec::new();
        convert_apis(
            apis,
            &mut results,
            |name, fun, _| me.analyze_foreign_fn_and_subclasses(name, fun),
            Api::struct_unchanged,
            Api::enum_unchanged,
            Api::typedef_unchanged,
            Api::subclass_unchanged,
        );
        let results = me.add_constructors_present(results);
        let mut results = me.add_subclass_constructors(results);
        results.extend(me.extra_apis.into_iter().map(add_analysis));
        results
    }

    /// The C++ names of every virtual method we've been given.
    ///
    /// A Rust subclass's peer class declares an override for each of its
    /// superclass's virtual methods, under that method's own C++ name, so
    /// these are the names its `_super` helpers must steer clear of.
    ///
    /// Gathered across every class rather than per superclass, because which
    /// receiver a method belongs to isn't settled until its parameters have
    /// been converted, and that happens long after this. Erring wide only ever
    /// costs an unrelated helper an extra `autocxx` in a name nobody was going
    /// to collide with; erring narrow would let a real collision through.
    fn build_virtual_method_cpp_names(apis: &ApiVec<PodPhase>) -> HashSet<String> {
        apis.iter()
            .filter_map(|api| match api {
                Api::Function { name, fun, .. } if fun.virtualness.is_some() => {
                    Some(name.cpp_name().to_string_for_cpp_generation().to_string())
                }
                _ => None,
            })
            .collect()
    }

    fn build_pod_safe_type_set(apis: &ApiVec<PodPhase>) -> HashSet<QualifiedName> {
        apis.iter()
            .filter_map(|api| match api {
                Api::Struct {
                    analysis:
                        PodAnalysis {
                            kind: TypeKind::Pod,
                            ..
                        },
                    ..
                } => Some(api.name().clone()),
                Api::Enum { .. } => Some(api.name().clone()),
                Api::ExternCppType { pod: true, .. } => Some(api.name().clone()),
                _ => None,
            })
            .chain(
                known_types()
                    .get_pod_safe_types()
                    .filter_map(
                        |(tn, is_pod_safe)| {
                            if is_pod_safe {
                                Some(tn)
                            } else {
                                None
                            }
                        },
                    ),
            )
            .collect()
    }

    /// Return the set of 'moveit safe' types. That must include only types where
    /// the size is known to be correct.
    fn build_correctly_sized_type_set(apis: &ApiVec<PodPhase>) -> HashSet<QualifiedName> {
        apis.iter()
            .filter(|api| {
                matches!(
                    api,
                    Api::Struct { .. }
                        | Api::Enum { .. }
                        | Api::ExternCppType {
                            details: ExternCppType { opaque: false, .. },
                            ..
                        }
                )
            })
            .map(|api| api.name().clone())
            .chain(known_types().get_moveit_safe_types())
            .collect()
    }

    fn build_generic_type_set(apis: &ApiVec<PodPhase>) -> HashSet<QualifiedName> {
        apis.iter()
            .filter_map(|api| match api {
                Api::Struct {
                    analysis: PodAnalysis { num_generics, .. },
                    ..
                } if *num_generics > 0 => Some(api.name().clone()),
                _ => None,
            })
            .collect()
    }

    fn build_types_in_anonymous_namespace(apis: &ApiVec<PodPhase>) -> HashSet<QualifiedName> {
        apis.iter()
            .filter_map(|api| match api {
                Api::Struct {
                    analysis:
                        PodAnalysis {
                            in_anonymous_namespace: true,
                            ..
                        },
                    ..
                } => Some(api.name().clone()),
                _ => None,
            })
            .collect()
    }

    /// Builds a mapping from a qualified type name to the last 'nest'
    /// of its name, if it has multiple elements.
    fn build_nested_type_map(apis: &ApiVec<PodPhase>) -> HashMap<QualifiedName, String> {
        apis.iter()
            .filter_map(|api| match api {
                Api::Struct { name, .. } | Api::Enum { name, .. } => name
                    .cpp_name()
                    .final_segment_if_any()
                    .map(|suffix| (name.name.clone(), suffix.to_string())),
                _ => None,
            })
            .collect()
    }

    fn convert_boxed_type(
        &mut self,
        ty: Box<Type>,
        ns: &Namespace,
    ) -> Result<Annotated<Box<Type>>, ConvertErrorFromCpp> {
        let ctx = TypeConversionContext::OuterType {};
        let mut annotated = self.type_converter.convert_boxed_type(ty, ns, &ctx)?;
        self.extra_apis.append(&mut annotated.extra_apis);
        Ok(annotated)
    }

    fn get_cxx_bridge_name(
        &mut self,
        type_name: Option<&str>,
        found_name: &str,
        ns: &Namespace,
    ) -> String {
        self.bridge_name_tracker
            .get_unique_cxx_bridge_name(type_name, found_name, ns)
    }

    fn is_on_allowlist(&self, type_name: &QualifiedName) -> bool {
        self.nested_cpp_names.is_on_allowlist(type_name)
    }

    fn is_generic_type(&self, type_name: &QualifiedName) -> bool {
        self.generic_types.contains(type_name)
    }

    #[allow(clippy::if_same_then_else)] // clippy bug doesn't notice the two
                                        // closures below are different.
    fn should_be_unsafe(
        &self,
        param_details: &[ArgumentAnalysis],
        kind: &FnKind,
    ) -> UnsafetyNeeded {
        let unsafest_non_placement_param = UnsafetyNeeded::from_param_details(param_details, true);
        let unsafest_param = UnsafetyNeeded::from_param_details(param_details, false);
        match kind {
            // Trait unsafety must always correspond to the norms for the
            // trait we're implementing.
            FnKind::TraitMethod {
                kind:
                    TraitMethodKind::CopyConstructor
                    | TraitMethodKind::MoveConstructor
                    | TraitMethodKind::Alloc
                    | TraitMethodKind::Dealloc,
                ..
            } => UnsafetyNeeded::Always,
            FnKind::TraitMethod { .. } => match unsafest_param {
                UnsafetyNeeded::Always => UnsafetyNeeded::JustBridge,
                _ => unsafest_param,
            },
            _ if matches!(self.unsafe_policy, UnsafePolicy::AllFunctionsUnsafe) => {
                UnsafetyNeeded::Always
            }
            _ => match unsafest_non_placement_param {
                UnsafetyNeeded::Always => UnsafetyNeeded::Always,
                UnsafetyNeeded::JustBridge => match unsafest_param {
                    UnsafetyNeeded::Always => UnsafetyNeeded::JustBridge,
                    _ => unsafest_non_placement_param,
                },
                UnsafetyNeeded::None => match unsafest_param {
                    UnsafetyNeeded::Always => UnsafetyNeeded::JustBridge,
                    _ => unsafest_param,
                },
            },
        }
    }

    fn add_subclass_constructors(&mut self, apis: ApiVec<FnPrePhase2>) -> ApiVec<FnPrePhase3> {
        let mut results = ApiVec::new();

        // Pre-assemble a list of superclass destructor visibility, to avoid
        // having to do a O(n^2) nested loop.
        let destructor_visibility_by_class: HashMap<_, _> = apis
            .iter()
            .filter_map(|api| match api {
                Api::Function {
                    fun,
                    analysis:
                        FnAnalysis {
                            kind: FnKind::TraitMethod { impl_for, .. },
                            // A `= default`ed destructor may have been
                            // withdrawn by `add_constructors_present` as one
                            // C++ actually deletes; it's no use to us then.
                            ignore_reason: Ok(()),
                            ..
                        },
                    ..
                } if matches!(
                    **fun,
                    FuncToConvert {
                        special_member: Some(SpecialMemberKind::Destructor),
                        is_deleted: None | Some(Explicitness::Defaulted),
                        ..
                    }
                ) =>
                {
                    Some((impl_for.clone(), fun.cpp_vis))
                }
                _ => None,
            })
            .collect();

        for api in apis.iter() {
            if let Api::Function {
                fun,
                analysis:
                    analysis @ FnAnalysis {
                        kind:
                            FnKind::Method {
                                impl_for: sup,
                                method_kind: MethodKind::Constructor { .. },
                                ..
                            },
                        ..
                    },
                ..
            } = api
            {
                // If we don't have an accessible destructor, then the subclass
                // itself cannot be destroyed, so there is no point synthesizing
                // a constructor for it. Both public and protected destructors
                // are accessible from a subclass's own destructor.
                if !matches!(
                    destructor_visibility_by_class.get(sup),
                    Some(CppVisibility::Public | CppVisibility::Protected)
                ) {
                    continue;
                }

                for sub in self.subclasses_by_superclass(sup) {
                    // Create a subclass constructor. This is a synthesized function
                    // which didn't exist in the original C++.
                    let (subclass_constructor_func, subclass_constructor_name) =
                        create_subclass_constructor(sub, analysis, sup, fun);
                    self.analyze_and_add(
                        subclass_constructor_name.clone(),
                        subclass_constructor_func.clone(),
                        &mut results,
                        TypeConversionSophistication::Regular,
                        None,
                    );
                }
            }
        }

        // Carry every other API across unchanged, annotating each subclass
        // with what we just learned about its superclass's destructor.
        convert_apis(
            apis,
            &mut results,
            Api::fun_unchanged,
            Api::struct_unchanged,
            Api::enum_unchanged,
            Api::typedef_unchanged,
            |name, superclass, _| {
                Ok(Box::new(std::iter::once(Api::Subclass {
                    name,
                    analysis: SubclassAnalysis {
                        superclass_destructor_visibility: destructor_visibility_by_class
                            .get(&superclass)
                            .copied(),
                        superclass_destructor_virtual: false,
                    },
                    superclass,
                })))
            },
        );

        results
    }

    /// Analyze a given function, and any permutations of that function which
    /// we might additionally generate (e.g. for subclasses.)
    ///
    /// Leaves the [`FnKind::Method::type_constructors`] at its default for [`add_constructors_present`]
    /// to fill out.
    fn analyze_foreign_fn_and_subclasses(
        &mut self,
        name: ApiName,
        fun: Box<FuncToConvert>,
    ) -> Result<Box<dyn Iterator<Item = Api<FnPrePhase1>>>, ConvertErrorWithContext> {
        let (analysis, name) =
            self.analyze_foreign_fn(name, &fun, TypeConversionSophistication::Regular, None);
        let mut results = ApiVec::new();

        // Consider whether we need to synthesize subclass items.
        if let FnKind::Method {
            impl_for: sup,
            method_kind:
                MethodKind::Virtual(receiver_mutability) | MethodKind::PureVirtual(receiver_mutability),
            ..
        } = &analysis.kind
        {
            let (simpler_analysis, _) = self.analyze_foreign_fn(
                name.clone(),
                &fun,
                TypeConversionSophistication::SimpleForSubclasses,
                Some(analysis.rust_name.clone()),
            );
            let is_pure_virtual = matches!(
                &simpler_analysis.kind,
                FnKind::Method {
                    method_kind: MethodKind::PureVirtual(..),
                    ..
                }
            );
            // Whether the peer class can offer a `foo_super` helper which
            // calls the superclass's own implementation. A pure virtual
            // method has no such implementation; a `private` one has one
            // the peer isn't allowed to call, even though C++ does let it
            // override the method. `protected` is fine - access from a
            // derived class is precisely what it permits.
            let has_super_helper = !is_pure_virtual
                && !matches!(fun.cpp_vis, CppVisibility::Private)
                // Nobody subclasses this in Rust, so there is no peer class to
                // put a helper on - and naming one anyway would move every
                // later helper's name along for nothing.
                && self.subclasses_by_superclass(sup).next().is_some();

            // The peer class's method keeps its plain `foo_super` name in
            // Rust - that's what subclass authors write - but in C++ it
            // needs a name which can't collide with the superclass's own
            // virtual methods, which the peer must declare under their
            // real names in order to override them. So the two differ, and
            // cxx bridges them with a #[cxx_name].
            //
            // Named once for all the subclasses of this superclass, not once
            // per subclass: each peer class is a class of its own, so they can
            // share the name, and minting it repeatedly would walk the escape
            // in `get_cpp_super_fn_name` further along every time.
            let super_fn_cpp_name = has_super_helper.then(|| {
                let name = SubclassName::get_cpp_super_fn_name(
                    &Namespace::new(),
                    &analysis.rust_name,
                    |candidate| self.cpp_names_taken_on_peer_classes.contains(candidate),
                );
                self.cpp_names_taken_on_peer_classes
                    .insert(name.get_final_item().to_string());
                name
            });

            for sub in self.subclasses_by_superclass(sup) {
                // For each subclass, we need to create a plain-C++ method to call its superclass
                // and a Rust/C++ bridge API to call _that_.
                // What we're generating here is entirely about the subclass, so the
                // superclass's namespace is irrelevant. We generate
                // all subclasses in the root namespace.
                let super_fn_rust_name =
                    SubclassName::get_super_fn_name(&Namespace::new(), &analysis.rust_name);
                let super_fn_api_name = SubclassName::get_super_fn_name(
                    &Namespace::new(),
                    &analysis.cxxbridge_name.to_string(),
                );
                let trait_api_name = SubclassName::get_trait_api_name(sup, &analysis.rust_name);

                let mut subclass_fn_deps = vec![trait_api_name.clone()];
                if let Some(super_fn_cpp_name) = &super_fn_cpp_name {
                    // Create a C++ API representing the superclass implementation (allowing
                    // calls from Rust->C++)
                    let maybe_wrap = create_subclass_fn_wrapper(&sub, super_fn_cpp_name, &fun);
                    let super_fn_name = ApiName::new_from_qualified_name_and_cpp_name(
                        super_fn_api_name,
                        Some(CppOriginalName::from_final_item_of_generated_cpp_name(
                            super_fn_cpp_name,
                        )),
                    );
                    let super_fn_call_api_name = self.analyze_and_add(
                        super_fn_name,
                        maybe_wrap,
                        &mut results,
                        TypeConversionSophistication::SimpleForSubclasses,
                        Some(super_fn_rust_name.get_final_item().to_string()),
                    );
                    subclass_fn_deps.push(super_fn_call_api_name);
                }

                // Create the Rust API representing the subclass implementation (allowing calls
                // from C++ -> Rust)
                results.push(create_subclass_function(
                    // RustSubclassFn
                    &sub,
                    &simpler_analysis,
                    &name,
                    receiver_mutability,
                    sup,
                    subclass_fn_deps,
                    self.unsafe_policy,
                    fun.ref_qualifier,
                    super_fn_cpp_name
                        .as_ref()
                        .map(QualifiedName::get_final_ident),
                ));

                // Create the trait item for the <superclass>_methods and <superclass>_supers
                // traits. This is required per-superclass, not per-subclass, so don't
                // create it if it already exists.
                if !self
                    .existing_superclass_trait_api_names
                    .contains(&trait_api_name)
                {
                    self.existing_superclass_trait_api_names
                        .insert(trait_api_name.clone());
                    results.push(create_subclass_trait_item(
                        ApiName::new_from_qualified_name(trait_api_name),
                        &simpler_analysis,
                        &analysis,
                        receiver_mutability,
                        sup.clone(),
                        has_super_helper,
                        self.unsafe_policy,
                    ));
                }
            }
        }

        results.push(Api::Function {
            fun,
            analysis,
            name,
        });

        Ok(Box::new(results.into_iter()))
    }

    /// Adds an API, usually a synthesized API. Returns the final calculated API name, which can be used
    /// for others to depend on this.
    fn analyze_and_add<P: AnalysisPhase<FunAnalysis = FnAnalysis>>(
        &mut self,
        name: ApiName,
        new_func: Box<FuncToConvert>,
        results: &mut ApiVec<P>,
        sophistication: TypeConversionSophistication,
        predetermined_rust_name: Option<String>,
    ) -> QualifiedName {
        let (analysis, name) =
            self.analyze_foreign_fn(name, &new_func, sophistication, predetermined_rust_name);
        results.push(Api::Function {
            fun: new_func,
            analysis,
            name: name.clone(),
        });
        name.name
    }

    /// Determine how to materialize a function.
    ///
    /// The main job here is to determine whether a function can simply be noted
    /// in the [cxx::bridge] mod and passed directly to cxx, or if it needs a Rust-side
    /// wrapper function, or if it needs a C++-side wrapper function, or both.
    /// We aim for the simplest case but, for example:
    /// * We'll need a C++ wrapper for static methods
    /// * We'll need a C++ wrapper for parameters which need to be wrapped and unwrapped
    ///   to [cxx::UniquePtr]
    /// * We'll need a Rust wrapper if we've got a C++ wrapper and it's a method.
    /// * We may need wrappers if names conflict.
    /// * etc.
    ///
    /// The other major thing we do here is figure out naming for the function.
    /// This depends on overloads, and what other functions are floating around.
    /// The output of this analysis phase is used by both Rust and C++ codegen.
    fn analyze_foreign_fn(
        &mut self,
        name: ApiName,
        fun: &FuncToConvert,
        sophistication: TypeConversionSophistication,
        predetermined_rust_name: Option<String>,
    ) -> (FnAnalysis, ApiName) {
        let cpp_original_name = name.cpp_name_if_present();
        let ns = name.name.get_namespace();

        // Let's gather some pre-wisdom about the name of the function.
        // We're shortly going to plunge into analyzing the parameters,
        // and it would be nice to have some idea of the function name
        // for diagnostics whilst we do that.
        let initial_rust_name = fun.ident.to_string();
        let diagnostic_name = cpp_original_name
            .as_ref()
            .map(|n| n.diagnostic_display_name())
            .unwrap_or(&initial_rust_name);
        let diagnostic_name = QualifiedName::new(ns, make_ident(diagnostic_name));

        // Now let's analyze all the parameters.
        // See if any have annotations which our fork of bindgen has craftily inserted...
        let (param_details, bads): (Vec<_>, Vec<_>) = fun
            .inputs
            .iter()
            .map(|i| {
                self.convert_fn_arg(
                    i,
                    ns,
                    &diagnostic_name,
                    &fun.synthesized_this_type,
                    true,
                    false,
                    None,
                    sophistication,
                    false,
                )
                .map_err(|err| ConvertErrorFromCpp::Argument {
                    arg: describe_arg(i),
                    err: Box::new(err),
                })
            })
            .partition(Result::is_ok);
        let (mut params, mut param_details): (Punctuated<_, Comma>, Vec<_>) =
            param_details.into_iter().map(Result::unwrap).unzip();

        let params_deps: HashSet<_> = param_details
            .iter()
            .flat_map(|p| p.deps.iter().cloned())
            .collect();
        let self_ty = param_details
            .iter()
            .filter_map(|pd| pd.self_type.as_ref())
            .next()
            .cloned();

        // End of parameter processing.
        // Work out naming, part one.
        // bindgen may have mangled the name either because it's invalid Rust
        // syntax (e.g. a keyword like 'async') or it's an overload.
        // If the former, we respect that mangling. If the latter, we don't,
        // because we'll add our own overload counting mangling later.
        // Cases:
        //   function, IRN=foo,    CN=<none>                    output: foo    case 1
        //   function, IRN=move_,  CN=move   (keyword problem)  output: move_  case 2
        //   function, IRN=foo1,   CN=foo    (overload)         output: foo    case 3
        //   method,   IRN=A_foo,  CN=foo                       output: foo    case 4
        //   method,   IRN=A_move, CN=move   (keyword problem)  output: move_  case 5
        //   method,   IRN=A_foo1, CN=foo    (overload)         output: foo    case 6
        let ideal_rust_name = match cpp_original_name {
            None => initial_rust_name, // case 1
            Some(cpp_original_name) => {
                if initial_rust_name.ends_with('_') {
                    initial_rust_name // case 2
                } else if validate_ident_ok_for_rust(cpp_original_name).is_err() {
                    format!("{}_", cpp_original_name.to_string_for_rust_name()) // case 5
                } else {
                    cpp_original_name.to_string_for_rust_name() // cases 3, 4, 6
                }
            }
        };

        // Let's spend some time figuring out the kind of this function (i.e. method,
        // virtual function, etc.)
        // Part one, work out if this is a static method.
        let (is_static_method, self_ty, receiver_mutability) = match self_ty {
            None => {
                // Even if we can't find a 'self' parameter this could conceivably
                // be a static method.
                let self_ty = fun.self_ty.clone();
                (self_ty.is_some(), self_ty, None)
            }
            Some((self_ty, receiver_mutability)) => {
                (false, Some(self_ty), Some(receiver_mutability))
            }
        };

        // Part two, work out if this is a function, or method, or whatever.
        // First determine if this is actually a trait implementation.
        let trait_details = self.trait_creation_details_for_synthetic_function(
            &fun.add_to_trait,
            ns,
            &ideal_rust_name,
            &self_ty,
        );
        let (kind, error_context, rust_name) = if let Some(trait_details) = trait_details {
            trait_details
        } else if let Some(self_ty) = self_ty {
            // Some kind of method or static method.
            let type_ident = self_ty.get_final_item();
            // bindgen generates methods with the name:
            // {class}_{method name}
            // It then generates an impl section for the Rust type
            // with the original name, but we currently discard that impl section.
            // We want to feed cxx methods with just the method name, so let's
            // strip off the class name.
            let mut rust_name = ideal_rust_name;
            let nested_type_ident = self
                .nested_type_name_map
                .get(&self_ty)
                .map(|s| s.as_str())
                .unwrap_or_else(|| self_ty.get_final_item());
            if matches!(
                fun.special_member,
                Some(SpecialMemberKind::CopyConstructor | SpecialMemberKind::MoveConstructor)
            ) {
                let is_move =
                    matches!(fun.special_member, Some(SpecialMemberKind::MoveConstructor));
                if let Some(constructor_suffix) = rust_name.strip_prefix(nested_type_ident) {
                    rust_name = format!("new{constructor_suffix}");
                }
                rust_name = predetermined_rust_name
                    .unwrap_or_else(|| self.get_overload_name(ns, type_ident, rust_name));
                let error_context = self.error_context_for_method(&self_ty, &rust_name);

                // A copy constructor taking a non-const source - `T(T&)` or
                // `T(volatile T&)` - cannot implement CopyNew, which copies from
                // a `&self`, so we treat it as a regular constructor. The
                // const-qualified forms, `T(const T&)` and `T(const volatile
                // T&)`, both can and do implement CopyNew.
                //
                // Ask what C++ wrote, rather than what the parameter has been
                // translated to: under `ReferencesWrappedAllFunctionsSafe`
                // every C++ reference becomes a pointer, so a source spelled
                // `const T&` would answer "not a reference" and every copy
                // constructor in that mode would quietly become an ordinary
                // one. If this is `None`, then something weird is going on;
                // we'll check for that later when we have enough context to
                // generate useful errors.
                let arg_is_const_reference = param_details
                    .get(1)
                    .is_some_and(|param| param.has_lifetime && !param.is_mutable_reference);
                if is_move || arg_is_const_reference {
                    let (kind, method_name, trait_id) = if is_move {
                        (
                            TraitMethodKind::MoveConstructor,
                            "move_new",
                            quote! { MoveNew },
                        )
                    } else {
                        (
                            TraitMethodKind::CopyConstructor,
                            "copy_new",
                            quote! { CopyNew },
                        )
                    };
                    let ty = Type::Path(self_ty.to_type_path());
                    (
                        FnKind::TraitMethod {
                            kind,
                            impl_for: self_ty,
                            details: Box::new(TraitMethodDetails {
                                trt: TraitImplSignature {
                                    ty: ty.into(),
                                    trait_signature: parse_quote! {
                                        autocxx::moveit::new:: #trait_id
                                    },
                                    unsafety: Some(parse_quote! { unsafe }),
                                },
                                avoid_self: true,
                                method_name: make_ident(method_name),
                                parameter_reordering: Some(vec![1, 0]),
                            }),
                        },
                        error_context,
                        rust_name,
                    )
                } else {
                    (
                        FnKind::Method {
                            impl_for: self_ty,
                            method_kind: MethodKind::Constructor { is_default: false },
                        },
                        error_context,
                        rust_name,
                    )
                }
            } else if matches!(fun.special_member, Some(SpecialMemberKind::Destructor)) {
                rust_name = predetermined_rust_name
                    .unwrap_or_else(|| self.get_overload_name(ns, type_ident, rust_name));
                let error_context = self.error_context_for_method(&self_ty, &rust_name);
                let ty = Type::Path(self_ty.to_type_path());
                (
                    FnKind::TraitMethod {
                        kind: TraitMethodKind::Destructor,
                        impl_for: self_ty,
                        details: Box::new(TraitMethodDetails {
                            trt: TraitImplSignature {
                                ty: ty.into(),
                                trait_signature: parse_quote! {
                                    Drop
                                },
                                unsafety: None,
                            },
                            avoid_self: false,
                            method_name: make_ident("drop"),
                            parameter_reordering: None,
                        }),
                    },
                    error_context,
                    rust_name,
                )
            } else {
                let method_kind = if let Some(constructor_suffix) =
                    constructor_with_suffix(&rust_name, nested_type_ident, fun.method_kind)
                {
                    // It's a constructor. bindgen generates
                    // fn Type(this: *mut Type, ...args)
                    // We want
                    // fn new(this: *mut Type, ...args)
                    // Later code will spot this and re-enter us, and we'll make
                    // a duplicate function in the above 'if' clause like this:
                    // fn make_unique(...args) -> Type
                    // which later code will convert to
                    // fn make_unique(...args) -> UniquePtr<Type>
                    // A type's several constructors all arrive under the type's
                    // own name, so the suffix is empty and `get_overload_name`
                    // below numbers them as it numbers every other overload.
                    rust_name = format!("new{constructor_suffix}");
                    MethodKind::Constructor {
                        is_default: matches!(
                            fun.special_member,
                            Some(SpecialMemberKind::DefaultConstructor)
                        ),
                    }
                } else if is_static_method {
                    MethodKind::Static
                } else {
                    let receiver_mutability =
                        receiver_mutability.expect("Failed to find receiver details");
                    match fun.virtualness {
                        None => MethodKind::Normal,
                        Some(Virtualness::Virtual) => MethodKind::Virtual(receiver_mutability),
                        Some(Virtualness::PureVirtual) => {
                            MethodKind::PureVirtual(receiver_mutability)
                        }
                    }
                };
                // Disambiguate overloads.
                let rust_name = predetermined_rust_name
                    .unwrap_or_else(|| self.get_overload_name(ns, type_ident, rust_name));
                let error_context = self.error_context_for_method(&self_ty, &rust_name);
                (
                    FnKind::Method {
                        impl_for: self_ty,
                        method_kind,
                    },
                    error_context,
                    rust_name,
                )
            }
        } else {
            // Not a method.
            // What shall we call this function? It may be overloaded.
            let rust_name = self.get_function_overload_name(ns, ideal_rust_name);
            (
                FnKind::Function,
                ErrorContext::new_for_item(make_ident(&rust_name)),
                rust_name,
            )
        };

        // Whether this is C++'s `operator=`. Three of the ignore reasons below
        // turn on it, because the reason recorded for an assignment operator is
        // read back later: `implicit_constructors` counts a class as having one
        // only if it finds this function ignored for being an assignment
        // operator or not ignored at all, and treats any other reason as
        // meaning it cannot tell, which changes which of that class's *other*
        // special members autocxx believes C++ implicitly defines.
        //
        // Nothing reaches any of the three, and the reason is mechanical.
        // bindgen tags an `operator=` as an assignment operator in the same
        // block that renames it to something spellable in Rust, and that block
        // runs after `ParseCallbacks::generated_name_override` - which autocxx
        // implements for every function. So bindgen sees
        // `operator=_bindgen_original`, not `operator=`, matches no arm, and
        // drops the function rather than reporting it. The classification
        // stays because it is right either way and is what the reader-back
        // needs the moment such a function does arrive.
        let is_assignment_operator = matches!(
            fun.special_member,
            Some(SpecialMemberKind::AssignmentOperator)
        );

        // If we encounter errors from here on, we can give some context around
        // where the error occurred such that we can put a marker in the output
        // Rust code to indicate that a problem occurred (benefiting people using
        // rust-analyzer or similar). Make a closure to make this easy.
        let mut ignore_reason = Ok(());
        let mut set_ignore_reason =
            |err| ignore_reason = Err(ConvertErrorWithContext(err, Some(error_context.clone())));

        // Now we have figured out the type of function (from its parameters)
        // we might have determined that we have a constructor. If so,
        // annoyingly, we need to go back and fiddle with the parameters in a
        // different way. This is because we want the first parameter to be a
        // pointer not a reference. For copy + move constructors, we also
        // enforce Rust-side conversions to comply with moveit traits.
        match kind {
            FnKind::Method {
                method_kind: MethodKind::Constructor { .. },
                ..
            } => {
                self.reanalyze_parameter(
                    0,
                    fun,
                    ns,
                    &diagnostic_name,
                    &mut params,
                    &mut param_details,
                    None,
                    sophistication,
                    true,
                    false,
                )
                .unwrap_or_else(&mut set_ignore_reason);
            }

            FnKind::TraitMethod {
                kind: TraitMethodKind::Destructor,
                ..
            } => {
                self.reanalyze_parameter(
                    0,
                    fun,
                    ns,
                    &diagnostic_name,
                    &mut params,
                    &mut param_details,
                    Some(ForcedRustConversion::Pointer(
                        PointerRustConversion::FromTypeToPtr,
                    )),
                    sophistication,
                    false,
                    false,
                )
                .unwrap_or_else(&mut set_ignore_reason);
            }
            FnKind::TraitMethod {
                kind: TraitMethodKind::CopyConstructor,
                ..
            } => {
                if param_details.len() < 2 {
                    set_ignore_reason(ConvertErrorFromCpp::ConstructorWithOnlyOneParam);
                }
                if param_details.len() > 2 {
                    set_ignore_reason(ConvertErrorFromCpp::ConstructorWithMultipleParams);
                }
                self.reanalyze_parameter(
                    0,
                    fun,
                    ns,
                    &diagnostic_name,
                    &mut params,
                    &mut param_details,
                    Some(ForcedRustConversion::Pointer(
                        PointerRustConversion::FromPinMaybeUninitToPtr,
                    )),
                    sophistication,
                    false,
                    false,
                )
                .unwrap_or_else(&mut set_ignore_reason);
                // `CopyNew::copy_new` copies from a `&Self`, so the source has
                // to arrive as a Rust reference whatever the unsafety policy
                // says about C++ references in general. Naming a conversion
                // for it - even the identity one - is how this asks
                // `argument_conversion_details` not to wrap it, which is what
                // `ReferencesWrappedAllFunctionsSafe` would otherwise do. Every
                // other policy leaves it as a `&T` anyway, so this changes
                // nothing there.
                self.reanalyze_parameter(
                    1,
                    fun,
                    ns,
                    &diagnostic_name,
                    &mut params,
                    &mut param_details,
                    Some(ForcedRustConversion::Identity),
                    sophistication,
                    false,
                    false,
                )
                .unwrap_or_else(&mut set_ignore_reason);
            }

            FnKind::TraitMethod {
                kind: TraitMethodKind::MoveConstructor,
                ..
            } => {
                if param_details.len() < 2 {
                    set_ignore_reason(ConvertErrorFromCpp::ConstructorWithOnlyOneParam);
                }
                if param_details.len() > 2 {
                    set_ignore_reason(ConvertErrorFromCpp::ConstructorWithMultipleParams);
                }
                self.reanalyze_parameter(
                    0,
                    fun,
                    ns,
                    &diagnostic_name,
                    &mut params,
                    &mut param_details,
                    Some(ForcedRustConversion::Pointer(
                        PointerRustConversion::FromPinMaybeUninitToPtr,
                    )),
                    sophistication,
                    false,
                    false,
                )
                .unwrap_or_else(&mut set_ignore_reason);
                self.reanalyze_parameter(
                    1,
                    fun,
                    ns,
                    &diagnostic_name,
                    &mut params,
                    &mut param_details,
                    Some(ForcedRustConversion::Pointer(
                        PointerRustConversion::FromPinMoveRefToPtr,
                    )),
                    sophistication,
                    false,
                    true,
                )
                .unwrap_or_else(&mut set_ignore_reason);
            }
            _ => {}
        }

        // Now we can add context to the error, check for a variety of error
        // cases. In each case, we continue to record the API, because it might
        // influence our later decisions to generate synthetic constructors
        // or note whether the type is abstract.
        let externally_callable = match fun.cpp_vis {
            CppVisibility::Private => {
                set_ignore_reason(ConvertErrorFromCpp::PrivateMethod);
                false
            }
            CppVisibility::Protected => false,
            CppVisibility::Public => true,
        };
        if fun.variadic {
            set_ignore_reason(ConvertErrorFromCpp::Variadic);
        }
        if let Some(problem) = bads.into_iter().next() {
            match problem {
                Ok(_) => panic!("No error in the error"),
                Err(problem) => set_ignore_reason(problem),
            }
        } else if is_assignment_operator {
            // Be careful with the order of this if-else tree. Anything above here means we won't
            // treat it as an assignment operator, but anything below we still consider when
            // deciding which other C++ special member functions are implicitly defined.
            set_ignore_reason(ConvertErrorFromCpp::AssignmentOperator)
        } else if return_type_is_reference(&fun.output) {
            set_ignore_reason(ConvertErrorFromCpp::RValueReturn)
        } else if matches!(fun.ref_qualifier, CppRefQualifier::RValue) {
            // `void foo() &&` can only be called on an rvalue. autocxx only
            // ever has a reference or a smart pointer to a C++ object, and
            // moving out of one of those isn't something we can express, so
            // there's nothing useful we could generate. `&`-qualified methods
            // are fine, and are handled by forcing a C++ wrapper below.
            set_ignore_reason(ConvertErrorFromCpp::RValueRefQualifiedMethod)
        } else if matches!(fun.is_deleted, Some(Explicitness::Deleted)) {
            set_ignore_reason(ConvertErrorFromCpp::Deleted)
        } else {
            match kind {
                // A method whose receiver is one of the types we substitute
                // for something of our own - `std::string` for
                // [`cxx::CxxString`], `rust::Str` for `&str` and so on. The
                // substitute is cxx's to define, so anything we attached to
                // it would be a method on a Rust type we don't own, or worse
                // a `Drop`/`CopyNew`/`MoveNew` impl fighting with the one cxx
                // already provides (google/autocxx#1097). The receiver check
                // has to cover the special member functions as well as the
                // ordinary methods, since those are exactly the ones a C++
                // class declares without the user asking for anything.
                FnKind::Method {
                    ref impl_for,
                    method_kind:
                        MethodKind::Constructor { .. }
                        | MethodKind::Normal
                        | MethodKind::PureVirtual(..)
                        | MethodKind::Virtual(..),
                    ..
                }
                | FnKind::TraitMethod { ref impl_for, .. }
                    if !known_types().is_cxx_acceptable_receiver(impl_for) =>
                {
                    set_ignore_reason(ConvertErrorFromCpp::UnsupportedReceiver);
                }
                FnKind::Method { ref impl_for, .. } if !self.is_on_allowlist(impl_for) => {
                    // Bindgen will output methods for types which have been encountered
                    // virally as arguments on other allowlisted types. But we don't want
                    // to generate methods unless the user has specifically asked us to.
                    // It may, for instance, be a private type.
                    set_ignore_reason(ConvertErrorFromCpp::MethodOfNonAllowlistedType);
                }
                FnKind::Method { ref impl_for, .. } | FnKind::TraitMethod { ref impl_for, .. } => {
                    if self.is_generic_type(impl_for) {
                        set_ignore_reason(ConvertErrorFromCpp::MethodOfGenericType);
                    }
                    if self.types_in_anonymous_namespace.contains(impl_for) {
                        set_ignore_reason(ConvertErrorFromCpp::MethodInAnonymousNamespace);
                    }
                }
                _ => {}
            }
        };

        // The name we use within the cxx::bridge mod may be different
        // from both the C++ name and the Rust name, because it's a flat
        // namespace so we might need to prepend some stuff to make it unique.
        let cxxbridge_name = self.get_cxx_bridge_name(
            match kind {
                FnKind::Method { ref impl_for, .. } => Some(impl_for.get_final_item()),
                FnKind::Function => None,
                FnKind::TraitMethod { ref impl_for, .. } => Some(impl_for.get_final_item()),
            },
            &rust_name,
            ns,
        );

        // bindgen reports an original C++ name for every function it emits, so
        // both of the following fall back on `rust_name` only where no such
        // report reached us. In the ordinary pipeline that means a function
        // autocxx synthesized: across the whole integration suite, every one
        // is a cast, an allocator or a special member autocxx filled in, and
        // none of those has a C++ original for these two to be wrong about.
        // Nothing carries either value into the output for one of them, at
        // that: each needs a C++ wrapper, which suppresses the `#[cxx_name]`
        // `api_name_cpp_override` would otherwise produce (see `gen_function`),
        // and takes its body from `synthetic_cpp`, as a cast or an allocator
        // does, or from a kind - constructor, destructor - whose body names a
        // type rather than a function.
        //
        // `conversion_tests` reaches the same fallback by another route: it
        // converts a hand-written bindgen mod with no callback results
        // recorded at all, so an ordinary function pasted in there has no
        // reported name either, and codegen may well call the C++ name from
        // here. That is right too - such a mod stands for C++ which spells the
        // function the way bindgen's identifier does.
        let api_name_cpp_override = match cpp_original_name {
            Some(name) => Some(name.clone()),
            None if cxxbridge_name != rust_name => Some(
                CppOriginalName::from_function_without_a_reported_cpp_name(&rust_name),
            ),
            None => None,
        };
        let underlying_cpp_function_name = match cpp_original_name {
            Some(name) => name.to_effective_name(),
            None => CppEffectiveName::from_function_without_a_reported_cpp_name(&rust_name),
        };
        let mut cxxbridge_name = make_ident(&cxxbridge_name);

        // Analyze the return type, just as we previously did for the
        // parameters.
        let mut return_analysis = self
            .convert_return_type(&fun.output, ns, &diagnostic_name, sophistication)
            .unwrap_or_else(|err| {
                set_ignore_reason(err);
                ReturnTypeAnalysis::default()
            });
        let mut deps = params_deps;
        deps.extend(return_analysis.deps.drain(..));

        // Sometimes, the return type will actually be a value type
        // for which we instead want to _pass_ a pointer into which the value
        // can be constructed. Handle that case here.
        if let Some((extra_param, extra_param_details)) = return_analysis.placement_param_needed {
            param_details.push(extra_param_details);
            params.push(extra_param);
        }

        let requires_unsafe = self.should_be_unsafe(&param_details, &kind);

        // The refusal further up caught a `T&&` return spelled out, which
        // reaches us as one of bindgen's markers. Behind a typedef it does
        // not: it arrives as an ordinary path, and only the type converter,
        // having resolved the alias, can say what it was. So say it again
        // here, for the same reason: the shim such a function would need has
        // to turn the `T&&` it returned into a pointer, and `&` wants an
        // lvalue, so the result would first have to be given a name - which
        // a wrapper body, one expression built around the call, has nowhere
        // to put. A subclass peer's override is unaffected - it
        // is generated for a pure virtual method whether or not the
        // superclass got a binding, and goes the other way about. See
        // google/autocxx#1363 for the parameter half of the same story.
        //
        // Not for an assignment operator, though. The chain above classifies
        // one before it ever looks at the return type, and relabelling it here
        // would lose the reason that gets read back. Nothing escapes by
        // leaving it alone - the function is ignored either way, which is all
        // this refusal is for.
        if return_analysis.was_rvalue_reference && !is_assignment_operator {
            set_ignore_reason(ConvertErrorFromCpp::RValueReturn);
        }

        // The following sections reject some types of function because of the arrangement
        // of Rust references. We could lift these restrictions when/if we switch to using
        // CppRef to represent C++ references.
        //
        // Both skip an assignment operator for the same reason as the refusal
        // above, and both would otherwise relabel every one there is:
        // `T& operator=(const T&)` returns a mutable reference and takes two
        // references, so it fails the first count for having more than one and
        // would have failed the second had it taken one fewer.
        if return_analysis.was_reference && !is_assignment_operator {
            // cxx only allows functions to return a reference if they take exactly
            // one reference as a parameter. Let's see.
            let num_input_references = param_details.iter().filter(|pd| pd.has_lifetime).count();
            if num_input_references == 0 {
                set_ignore_reason(ConvertErrorFromCpp::NoInputReference(rust_name.clone()));
            }
            if num_input_references > 1 {
                set_ignore_reason(ConvertErrorFromCpp::MultipleInputReferences(
                    rust_name.clone(),
                ));
            }
        }
        if return_analysis.was_mutable_reference && !is_assignment_operator {
            // This one's a bit more subtle. We can't have:
            //    fn foo(thing: &Thing) -> &mut OtherThing
            // because Rust doesn't allow it.
            // We could probably allow:
            //    fn foo(thing: &mut Thing, thing2: &mut OtherThing) -> &mut OtherThing
            // but probably cxx doesn't allow that. (I haven't checked). Even if it did,
            // there's ambiguity here so won't allow it.
            let num_input_mutable_references = param_details
                .iter()
                .filter(|pd| pd.has_lifetime && pd.is_mutable_reference)
                .count();
            if num_input_mutable_references == 0 {
                set_ignore_reason(ConvertErrorFromCpp::NoMutableInputReference(
                    rust_name.clone(),
                ));
            }
            if num_input_mutable_references > 1 {
                set_ignore_reason(ConvertErrorFromCpp::MultipleMutableInputReferences(
                    rust_name.clone(),
                ));
            }
        }

        let mut ret_type = return_analysis.rt;
        let ret_type_conversion = return_analysis.conversion;
        let ret_type_was_const = return_analysis.was_const;

        // Do we need to convert either parameters or return type?
        let param_conversion_needed = param_details.iter().any(|b| b.conversion.cpp_work_needed());
        let ret_type_conversion_needed = ret_type_conversion
            .as_ref()
            .is_some_and(|x| x.cpp_work_needed());
        let return_needs_rust_conversion = ret_type_conversion
            .as_ref()
            .map(|ra| ra.rust_work_needed())
            .unwrap_or_default();

        // See https://github.com/dtolnay/cxx/issues/878 for the reason for this next line.
        let cpp_name_incompatible_with_cxx = cpp_original_name
            .map(|n| validate_ident_ok_for_rust(n).is_err())
            .unwrap_or_default();

        // Check if this function is marked as potentially throwing C++ exceptions.
        // For methods, we also check with the class name prepended (e.g., "MyClass::method").
        let designated_as_throwing = self
            .config
            .is_on_throws_list(&diagnostic_name.to_cpp_name())
            || match &kind {
                FnKind::Method { impl_for, .. } | FnKind::TraitMethod { impl_for, .. } => {
                    let method_qualified_name = format!(
                        "{}::{}",
                        impl_for.to_cpp_name(),
                        diagnostic_name.get_final_item()
                    );
                    self.config.is_on_throws_list(&method_qualified_name)
                }
                FnKind::Function => false,
            };
        // A designation cannot be honoured for a function whose Rust shape is
        // fixed by a trait we do not own. Every one of these implements a
        // `moveit` trait, `Drop` or `MakeCppStorage`, and none of those has a
        // method which returns a `Result`; wrapping the return type in one
        // anyway produces an impl which does not match its trait.
        //
        // This matters because `throws!("MyClass::MyClass")` designates every
        // constructor sharing that C++ name, which includes a copy or move
        // constructor the class declares for itself. Those become
        // `moveit::CopyNew` and `moveit::MoveNew`, whose `copy_new` and
        // `move_new` return nothing, so the exception has nowhere to go and a
        // copy or move constructor which throws still terminates the process.
        // Making them fallible needs fallible counterparts of those traits,
        // which `moveit` does not have. A destructor is the same story via
        // `Drop` - and is implicitly `noexcept` in C++ anyway, so throwing from
        // one calls `std::terminate` before autocxx is involved at all.
        let designation_can_be_honoured = !matches!(kind, FnKind::TraitMethod { .. });
        let may_throw = designated_as_throwing && designation_can_be_honoured;

        // If possible, we'll put knowledge of the C++ API directly into the cxx::bridge
        // mod. However, there are various circumstances where cxx can't work with the existing
        // C++ API and we need to create a C++ wrapper function which is more cxx-compliant.
        // That wrapper function is included in the cxx::bridge, and calls through to the
        // original function.
        let wrapper_function_needed = match kind {
            FnKind::Method {
                method_kind:
                    MethodKind::Static
                    | MethodKind::Constructor { .. }
                    | MethodKind::Virtual(_)
                    | MethodKind::PureVirtual(_),
                ..
            }
            | FnKind::TraitMethod {
                kind:
                    TraitMethodKind::CopyConstructor
                    | TraitMethodKind::MoveConstructor
                    | TraitMethodKind::Destructor,
                ..
            } => true,
            FnKind::Method { .. } if cxxbridge_name != rust_name => true,
            // cxx calls a method through a pointer-to-member-function, and a
            // pointer-to-member type can't carry a ref-qualifier, so the C++
            // it generates for `void foo() &` doesn't compile. Our own wrapper
            // calls the method directly on an lvalue, which is fine.
            // google/autocxx#837.
            _ if matches!(fun.ref_qualifier, CppRefQualifier::LValue) => true,
            _ if param_conversion_needed => true,
            _ if ret_type_conversion_needed => true,
            // cxx takes the address of every function it declares, and a
            // top-level `const` on the return type is part of the function's
            // type, so `int (*f$)() = ::f;` does not compile for a `const int
            // f()`. Our own wrapper returns the unqualified type - which the
            // caller is copying out of C++ anyway - and calls through.
            // google/autocxx#1191.
            _ if ret_type_was_const => true,
            _ if cpp_name_incompatible_with_cxx => true,
            _ if fun.synthetic_cpp.is_some() => true,
            _ if self.force_wrapper_generation => true,
            _ => false,
        };

        let cpp_wrapper = if wrapper_function_needed {
            // Generate a new layer of C++ code to wrap/unwrap parameters
            // and return values into/out of std::unique_ptrs.
            let joiner = if cxxbridge_name.to_string().ends_with('_') {
                ""
            } else {
                "_"
            };
            cxxbridge_name = make_ident(
                self.config
                    .uniquify_name_per_mod(&format!("{cxxbridge_name}{joiner}autocxx_wrapper")),
            );
            let (payload, cpp_function_kind) = match fun.synthetic_cpp.as_ref().cloned() {
                Some((payload, cpp_function_kind)) => (payload, cpp_function_kind),
                None => match kind {
                    FnKind::Method {
                        ref impl_for,
                        method_kind: MethodKind::Constructor { .. },
                        ..
                    }
                    | FnKind::TraitMethod {
                        kind: TraitMethodKind::CopyConstructor | TraitMethodKind::MoveConstructor,
                        ref impl_for,
                        ..
                    } => (
                        CppFunctionBody::PlacementNew(ns.clone(), impl_for.get_final_ident()),
                        CppFunctionKind::Constructor,
                    ),
                    FnKind::TraitMethod {
                        kind: TraitMethodKind::Destructor,
                        ref impl_for,
                        ..
                    } => (
                        CppFunctionBody::Destructor(ns.clone(), impl_for.get_final_ident()),
                        CppFunctionKind::Function,
                    ),
                    FnKind::Method {
                        ref impl_for,
                        method_kind: MethodKind::Static,
                        ..
                    } => (
                        CppFunctionBody::StaticMethodCall(
                            ns.clone(),
                            impl_for.get_final_ident(),
                            underlying_cpp_function_name,
                        ),
                        CppFunctionKind::Function,
                    ),
                    FnKind::Method { .. } => (
                        CppFunctionBody::FunctionCall(ns.clone(), underlying_cpp_function_name),
                        CppFunctionKind::Method,
                    ),
                    _ => (
                        CppFunctionBody::FunctionCall(ns.clone(), underlying_cpp_function_name),
                        CppFunctionKind::Function,
                    ),
                },
            };
            // Now modify the cxx::bridge entry we're going to make.
            if let Some(ref conversion) = ret_type_conversion {
                if conversion.populate_return_value() {
                    let new_ret_type = conversion.unconverted_rust_type();
                    ret_type = parse_quote!(
                        -> #new_ret_type
                    );
                }
            }

            // Amend parameters for the function which we're asking cxx to generate.
            params.clear();
            for pd in &param_details {
                let type_name = pd.conversion.converted_rust_type();
                let arg_name: syn::Pat = if pd.self_type.is_some() {
                    let receiver = make_ident(RECEIVER_ARG_NAME);
                    parse_quote!(#receiver)
                } else {
                    pd.name.clone().into()
                };
                params.push(parse_quote!(
                    #arg_name: #type_name
                ));
            }

            // Nothing reads this for a wrapper - see `CppFunction` - so it
            // holds the name bindgen gave the function, or, for a synthesized
            // function which has none, the wrapper's own bridge name.
            let original_cpp_name = cpp_original_name
                .cloned()
                .map(|n| n.to_effective_name())
                .unwrap_or_else(|| {
                    CppEffectiveName::from_cxxbridge_name(&cxxbridge_name.to_string())
                });

            Some(CppFunction {
                payload,
                wrapper_function_name: cxxbridge_name.clone(),
                original_cpp_name,
                return_conversion: ret_type_conversion.clone(),
                argument_conversion: param_details.iter().map(|d| d.conversion.clone()).collect(),
                kind: cpp_function_kind,
                pass_obs_field: false,
                qualification: None,
                // Our wrapper is a free function which calls the method on an
                // lvalue, so it never needs a ref-qualifier of its own.
                ref_qualifier: CppRefQualifier::None,
                is_virtual_override: false,
            })
        } else {
            None
        };

        let vis = fun.vis.clone();

        let any_param_needs_rust_conversion = param_details
            .iter()
            .any(|pd| pd.conversion.rust_work_needed());

        let rust_wrapper_needed = match kind {
            _ if any_param_needs_rust_conversion || return_needs_rust_conversion => true,
            FnKind::TraitMethod { .. } => true,
            FnKind::Method { .. } => cxxbridge_name != rust_name,
            _ if self.force_wrapper_generation => true,
            _ => false,
        };

        // Naming, part two.
        // Work out our final naming strategy.
        validate_ident_ok_for_cxx(&cxxbridge_name.to_string())
            .map_err(ConvertErrorFromCpp::InvalidIdent)
            .unwrap_or_else(set_ignore_reason);
        let rust_name_ident = make_ident(&rust_name);
        let rust_rename_strategy = match kind {
            _ if rust_wrapper_needed => RustRenameStrategy::RenameUsingWrapperFunction,
            FnKind::Function if cxxbridge_name != rust_name => {
                RustRenameStrategy::RenameInOutputMod(rust_name_ident)
            }
            _ => RustRenameStrategy::None,
        };

        let analysis = FnAnalysis {
            cxxbridge_name: cxxbridge_name.clone(),
            rust_name: rust_name.clone(),
            cpp_call_name: api_name_cpp_override,
            rust_rename_strategy,
            params,
            ret_conversion: ret_type_conversion,
            kind,
            ret_type: ret_type.into(),
            param_details,
            requires_unsafe,
            vis: vis.into(),
            cpp_wrapper,
            deps,
            ignore_reason,
            externally_callable,
            rust_wrapper_needed,
            may_throw,
        };
        // For everything other than functions, the API name is immutable.
        // It would be nice to get to that point with functions, but at present
        // the API name is used in Rust codegen to generate "use" statements,
        // so we override it.
        let name = ApiName::new_with_cpp_name(ns, cxxbridge_name, cpp_original_name.cloned());
        (analysis, name)
    }

    fn error_context_for_method(&self, self_ty: &QualifiedName, rust_name: &str) -> ErrorContext {
        if self.is_generic_type(self_ty) {
            // A 'method' error context would end up in an
            //   impl A {
            //      fn error_thingy
            //   }
            // block. We can't impl A if it would need to be impl A<B>
            ErrorContext::new_for_item(make_ident(rust_name))
        } else {
            ErrorContext::new_for_method(self_ty.get_final_ident(), make_ident(rust_name))
        }
    }

    /// Applies a specific `force_rust_conversion` to the parameter at index
    /// `param_idx`. Modifies `param_details` and `params` in place.
    #[allow(clippy::too_many_arguments)] // it's true, but sticking with it for now
    fn reanalyze_parameter(
        &mut self,
        param_idx: usize,
        fun: &FuncToConvert,
        ns: &Namespace,
        diagnostic_name: &QualifiedName,
        params: &mut Punctuated<FnArg, Comma>,
        param_details: &mut [ArgumentAnalysis],
        force_rust_conversion: Option<ForcedRustConversion>,
        sophistication: TypeConversionSophistication,
        construct_into_self: bool,
        is_move_constructor: bool,
    ) -> Result<(), ConvertErrorFromCpp> {
        self.convert_fn_arg(
            fun.inputs.iter().nth(param_idx).unwrap(),
            ns,
            diagnostic_name,
            &fun.synthesized_this_type,
            false,
            is_move_constructor,
            force_rust_conversion,
            sophistication,
            construct_into_self,
        )
        .map(|(new_arg, new_analysis)| {
            param_details[param_idx] = new_analysis;
            let mut params_before = params.clone().into_iter();
            let prefix = params_before
                .by_ref()
                .take(param_idx)
                .collect_vec()
                .into_iter();
            let suffix = params_before.skip(1);
            *params = prefix
                .chain(std::iter::once(new_arg))
                .chain(suffix)
                .collect()
        })
    }

    /// Reserve, in each namespace's overload tracker, the name every
    /// real function will ideally take, so that overload suffix
    /// generation cannot collide with a real name which happens to be
    /// processed later (google/autocxx#1316, e.g. overloads of
    /// `byteSwap` alongside a real `byteSwap2`). This mirrors the
    /// `ideal_rust_name` derivation in
    /// [`Self::analyze_foreign_fn_and_subclasses`]. Reservations are
    /// namespace-wide and over-reservation is harmless (a generated
    /// suffix just skips a number), so approximations err in that
    /// direction.
    fn reserve_ideal_names(&mut self, apis: &ApiVec<PodPhase>) {
        for api in apis.iter() {
            if let Api::Function { name, fun, .. } = api {
                let initial_rust_name = fun.ident.to_string();
                let bare = match name.cpp_name_if_present() {
                    None => initial_rust_name,
                    Some(cpp_original_name) => {
                        if initial_rust_name.ends_with('_') {
                            initial_rust_name
                        } else if validate_ident_ok_for_rust(cpp_original_name).is_err() {
                            format!("{}_", cpp_original_name.to_string_for_rust_name())
                        } else {
                            cpp_original_name.to_string_for_rust_name()
                        }
                    }
                };
                let ns = name.name.get_namespace().clone();
                // Methods reserve within their type's scope; free
                // functions within the namespace's function scope --
                // mirroring how names are later assigned, so a real
                // name on one type cannot perturb numbering on an
                // unrelated type.
                let type_scope = fun
                    .self_ty
                    .as_ref()
                    .map(|ty| ty.get_final_item().to_string());
                self.overload_trackers_by_mod
                    .entry(ns)
                    .or_default()
                    .reserve(type_scope.as_deref(), &bare);
            }
        }
    }

    fn get_overload_name(&mut self, ns: &Namespace, type_ident: &str, rust_name: String) -> String {
        let overload_tracker = self.overload_trackers_by_mod.entry(ns.clone()).or_default();
        overload_tracker.get_method_real_name(type_ident, rust_name)
    }

    /// Determine if this synthetic function should actually result in the implementation
    /// of a trait, rather than a function/method.
    fn trait_creation_details_for_synthetic_function(
        &mut self,
        synthesis: &Option<TraitSynthesis>,
        ns: &Namespace,
        ideal_rust_name: &str,
        self_ty: &Option<QualifiedName>,
    ) -> Option<(FnKind, ErrorContext, String)> {
        synthesis.as_ref().and_then(|synthesis| match synthesis {
            TraitSynthesis::Cast { to_type, mutable } => {
                let rust_name = self.get_function_overload_name(ns, ideal_rust_name.to_string());
                let from_type = self_ty.as_ref().unwrap();
                let from_type_path = from_type.to_type_path();
                let to_type = to_type.to_type_path();
                let (trait_signature, ty, method_name) = match *mutable {
                    CastMutability::ConstToConst => (
                        parse_quote! {
                            AsRef < #to_type >
                        },
                        Type::Path(from_type_path),
                        "as_ref",
                    ),
                    CastMutability::MutToConst => (
                        parse_quote! {
                            AsRef < #to_type >
                        },
                        parse_quote! {
                            &'a mut ::core::pin::Pin < &'a mut #from_type_path >
                        },
                        "as_ref",
                    ),
                    CastMutability::MutToMut => (
                        parse_quote! {
                            autocxx::PinMut < #to_type >
                        },
                        parse_quote! {
                            ::core::pin::Pin < &'a mut #from_type_path >
                        },
                        "pin_mut",
                    ),
                };
                let method_name = make_ident(method_name);
                Some((
                    FnKind::TraitMethod {
                        kind: TraitMethodKind::Cast,
                        impl_for: from_type.clone(),
                        details: Box::new(TraitMethodDetails {
                            trt: TraitImplSignature {
                                ty: ty.into(),
                                trait_signature,
                                unsafety: None,
                            },
                            avoid_self: false,
                            method_name,
                            parameter_reordering: None,
                        }),
                    },
                    ErrorContext::new_for_item(make_ident(&rust_name)),
                    rust_name,
                ))
            }
            TraitSynthesis::AllocUninitialized(ty) => self.generate_alloc_or_deallocate(
                ideal_rust_name,
                ty,
                "allocate_uninitialized_cpp_storage",
                TraitMethodKind::Alloc,
            ),
            TraitSynthesis::FreeUninitialized(ty) => self.generate_alloc_or_deallocate(
                ideal_rust_name,
                ty,
                "free_uninitialized_cpp_storage",
                TraitMethodKind::Dealloc,
            ),
        })
    }

    fn generate_alloc_or_deallocate(
        &mut self,
        ideal_rust_name: &str,
        ty: &QualifiedName,
        method_name: &str,
        kind: TraitMethodKind,
    ) -> Option<(FnKind, ErrorContext, String)> {
        let rust_name =
            self.get_function_overload_name(ty.get_namespace(), ideal_rust_name.to_string());
        let typ = ty.to_type_path();
        Some((
            FnKind::TraitMethod {
                impl_for: ty.clone(),
                details: Box::new(TraitMethodDetails {
                    trt: TraitImplSignature {
                        ty: Type::Path(typ).into(),
                        trait_signature: parse_quote! { autocxx::moveit::MakeCppStorage },
                        unsafety: Some(parse_quote! { unsafe }),
                    },
                    avoid_self: false,
                    method_name: make_ident(method_name),
                    parameter_reordering: None,
                }),
                kind,
            },
            ErrorContext::new_for_item(make_ident(&rust_name)),
            rust_name,
        ))
    }

    fn get_function_overload_name(&mut self, ns: &Namespace, ideal_rust_name: String) -> String {
        let overload_tracker = self.overload_trackers_by_mod.entry(ns.clone()).or_default();
        overload_tracker.get_function_real_name(ideal_rust_name)
    }

    fn subclasses_by_superclass(&self, sup: &QualifiedName) -> impl Iterator<Item = SubclassName> {
        match self.subclasses_by_superclass.get(sup) {
            Some(subs) => subs.clone().into_iter(),
            None => Vec::new().into_iter(),
        }
    }

    #[allow(clippy::too_many_arguments)] // currently reasonably clear
    fn convert_fn_arg(
        &mut self,
        arg: &FnArg,
        ns: &Namespace,
        diagnostic_name: &QualifiedName,
        virtual_this: &Option<QualifiedName>,
        treat_this_as_reference: bool,
        is_move_constructor: bool,
        force_rust_conversion: Option<ForcedRustConversion>,
        sophistication: TypeConversionSophistication,
        construct_into_self: bool,
    ) -> Result<(FnArg, ArgumentAnalysis), ConvertErrorFromCpp> {
        Ok(match &arg.0 {
            syn::FnArg::Typed(pt) => {
                let mut pt = pt.clone();
                let mut self_type = None;
                let old_pat = *pt.pat;
                let mut is_placement_return_destination = false;
                let (new_pat, ty_to_convert) = match old_pat {
                    syn::Pat::Ident(mut pp) if pp.ident == "this" => {
                        let this_type = match pt.ty.as_ref() {
                            Type::Ptr(TypePtr {
                                elem, mutability, ..
                            }) => match elem.as_ref() {
                                // bindgen could not name the type this method
                                // belongs to and put an opaque blob of the
                                // right size and alignment in its place. A
                                // class with a non-type template parameter is
                                // one way to arrive here: bindgen tracks only
                                // type parameters, so it generates the methods
                                // and then has no name to write for the
                                // receiver.
                                //
                                // There is no receiver to be had from a blob.
                                // What it is spelled with in Rust - `u8` for a
                                // one-byte class - names no C++ type, so
                                // reading a name out of it would say the
                                // method belongs to `u8`, and everything
                                // derived from that name (the impl block, the
                                // overload name, the dependencies) would be
                                // worked out from a fiction. Converting the
                                // `this` parameter refuses the blob a moment
                                // later, which is what saves us today; say it
                                // here, where the pretense would start.
                                Type::Path(typ) => match unwrap_has_opaque(typ) {
                                    Some(blob) => Err(ConvertErrorFromCpp::BindgenOpaqueBlob(
                                        blob.to_token_stream().to_string(),
                                    )),
                                    None => {
                                        let receiver_mutability = if mutability.is_some() {
                                            ReceiverMutability::Mutable
                                        } else {
                                            ReceiverMutability::Const
                                        };

                                        let this_type = if let Some(virtual_this) = virtual_this {
                                            let this_type_path = virtual_this.to_type_path();
                                            let const_token = if mutability.is_some() {
                                                None
                                            } else {
                                                Some(syn::Token![const](Span::call_site()))
                                            };
                                            pt.ty = Box::new(parse_quote! {
                                                * #mutability #const_token #this_type_path
                                            });
                                            virtual_this.clone()
                                        } else {
                                            QualifiedName::from_type_path(typ)
                                        };
                                        Ok((this_type, receiver_mutability))
                                    }
                                },
                                _ => Err(ConvertErrorFromCpp::UnexpectedThisType(
                                    diagnostic_name.clone(),
                                )),
                            },
                            _ => Err(ConvertErrorFromCpp::UnexpectedThisType(
                                diagnostic_name.clone(),
                            )),
                        }?;
                        self_type = Some(this_type);
                        is_placement_return_destination = construct_into_self;
                        if treat_this_as_reference {
                            pp.ident = Ident::new("self", pp.ident.span());
                            let pt_ty = pt.ty.as_ref();
                            (
                                syn::Pat::Ident(pp),
                                Box::new(
                                    syn::parse_quote! { __bindgen_marker_Reference < #pt_ty >},
                                ),
                            )
                        } else {
                            (syn::Pat::Ident(pp), pt.ty)
                        }
                    }
                    syn::Pat::Ident(pp) => {
                        validate_ident_ok_for_cxx(&pp.ident.to_string())
                            .map_err(ConvertErrorFromCpp::InvalidIdent)?;
                        (syn::Pat::Ident(pp), pt.ty)
                    }
                    _ => (old_pat, pt.ty),
                };

                let is_placement_return_destination = is_placement_return_destination
                    || matches!(
                        force_rust_conversion,
                        Some(ForcedRustConversion::Pointer(
                            PointerRustConversion::FromPlacementParamToNewReturn
                        ))
                    );
                let annotated_type = self.convert_boxed_type(ty_to_convert, ns)?;
                let conversion = self.argument_conversion_details(
                    &annotated_type,
                    is_move_constructor,
                    force_rust_conversion,
                    sophistication,
                    self_type.is_some(),
                    is_placement_return_destination,
                )?;
                let new_ty = annotated_type.ty;
                pt.pat = Box::new(new_pat.clone());
                pt.ty = new_ty;
                let requires_unsafe =
                    if matches!(annotated_type.kind, type_converter::TypeKind::Pointer)
                        && !is_placement_return_destination
                    {
                        UnsafetyNeeded::Always
                    } else if conversion.bridge_unsafe_needed() || is_placement_return_destination {
                        UnsafetyNeeded::JustBridge
                    } else {
                        UnsafetyNeeded::None
                    };
                (
                    syn::FnArg::Typed(pt).into(),
                    ArgumentAnalysis {
                        self_type,
                        name: new_pat.into(),
                        conversion,
                        has_lifetime: matches!(
                            annotated_type.kind,
                            type_converter::TypeKind::Reference
                                | type_converter::TypeKind::MutableReference
                        ),
                        is_mutable_reference: matches!(
                            annotated_type.kind,
                            type_converter::TypeKind::MutableReference
                        ),
                        deps: annotated_type.types_encountered,
                        requires_unsafe,
                        is_placement_return_destination,
                    },
                )
            }
            _ => panic!("Did not expect FnArg::Receiver to be generated by bindgen"),
        })
    }

    fn argument_conversion_details(
        &self,
        annotated_type: &Annotated<Box<Type>>,
        is_move_constructor: bool,
        force_rust_conversion: Option<ForcedRustConversion>,
        sophistication: TypeConversionSophistication,
        is_self: bool,
        is_placement_return_destination: bool,
    ) -> Result<TypeConversionPolicy, ConvertErrorFromCpp> {
        let is_subclass_holder = match &annotated_type.kind {
            type_converter::TypeKind::SubclassHolder(holder) => Some(holder),
            _ => None,
        };
        let is_rvalue_ref = matches!(
            annotated_type.kind,
            type_converter::TypeKind::RValueReference
        );
        // Every C++ reference in a parameter position, const or mutable.
        // What reads it is `unsafe_references_wrapped`, in the two branches
        // below which test it: there this decides which parameters become a
        // wrapper rather than a Rust reference. A mutable one has to be in
        // here. Left out, it stayed `Pin<&mut T>` - a Rust mutable
        // reference to an object C++ is entitled to hold other references to,
        // and ruling out exactly that aliasing is what the mode is for.
        //
        // The receiver `is_self` stands for is a C++ reference too, and a
        // mutable one whenever the method is non-const, and it has always been
        // wrapped. A parameter now takes that same route through those same
        // two branches, which is why nothing below had to learn about
        // mutability. The C++ side is unchanged either way: the shim takes a
        // pointer and dereferences it into the reference the function asked
        // for.
        //
        // The third reader is the POD branch further down, and it sees no
        // difference: what arrives there for a mutable reference is
        // `Pin<&mut T>`, and `Pin` is a known type, so that arm was already
        // taken by its other disjunct under every policy.
        let is_reference = matches!(
            annotated_type.kind,
            type_converter::TypeKind::Reference | type_converter::TypeKind::MutableReference
        ) || is_self;
        let rust_conversion_forced = force_rust_conversion.is_some();
        let ty = &*annotated_type.ty;
        if let Some(holder_id) = is_subclass_holder {
            let subclass = SubclassName::from_holder_name(holder_id);
            return Ok({
                let ty = parse_quote! {
                    rust::Box<#holder_id>
                };
                TypeConversionPolicy::whole(
                    ty,
                    WholeCppConversion::Move,
                    WholeRustConversion::ToBoxedUpHolder(subclass),
                )
            });
        } else if matches!(
            force_rust_conversion,
            Some(ForcedRustConversion::Pointer(
                PointerRustConversion::FromPlacementParamToNewReturn
            ))
        ) && matches!(sophistication, TypeConversionSophistication::Regular)
        {
            return Ok(TypeConversionPolicy::pointer(
                BridgePointer::from_type(ty).ok_or_else(|| {
                    ConvertErrorFromCpp::ParameterWasNotAPointer(ty.to_token_stream().to_string())
                })?,
                PointerCppConversion::IgnoredPlacementPtrParameter,
                PointerRustConversion::FromPlacementParamToNewReturn,
            ));
        }
        Ok(match ty {
            Type::Path(p) => {
                let ty = ty.clone();
                let tn = QualifiedName::from_type_path(p);
                if matches!(
                    self.config.unsafe_policy,
                    UnsafePolicy::ReferencesWrappedAllFunctionsSafe
                ) && is_reference
                    && !rust_conversion_forced
                // must be std::pin::Pin<&mut T>
                {
                    let unwrapped_type =
                        extract_pinned_mutable_reference_type(p).ok_or_else(|| {
                            ConvertErrorFromCpp::ParameterWasNotAPointer(
                                ty.to_token_stream().to_string(),
                            )
                        })?;
                    TypeConversionPolicy::pointer(
                        BridgePointer::to(unwrapped_type.clone(), true),
                        PointerCppConversion::FromPointerToReference,
                        PointerRustConversion::FromReferenceWrapperToPointer,
                    )
                } else if self.pod_safe_types.contains(&tn) {
                    if known_types().lacks_copy_constructor(&tn) {
                        TypeConversionPolicy::whole(
                            ty,
                            WholeCppConversion::Move,
                            WholeRustConversion::None,
                        )
                    } else if is_reference || known_types().is_known_type(&tn) {
                        // A reference parameter - a `Pin<&mut T>` reaches us
                        // as a path, and `Pin` is itself POD-safe - is handed
                        // straight on: there is no value of ours to move, and
                        // the callee may want an lvalue. A built-in scalar is
                        // handed straight on too, being always copyable.
                        TypeConversionPolicy::new_unconverted(ty)
                    } else {
                        // A POD out of the user's own headers, by value. The
                        // Rust side owns it and destroys it after the call, so
                        // a wrapper should hand it over by move where C++
                        // allows one. Passing it bare asks for a copy
                        // constructor, which a type that opts into
                        // relocatability by declaring its own move constructor
                        // no longer has - and forcing wrapper generation then
                        // failed to compile. See google/autocxx#1252.
                        TypeConversionPolicy::whole(
                            ty,
                            WholeCppConversion::MoveOrCopy,
                            WholeRustConversion::None,
                        )
                    }
                } else if known_types().convertible_from_strs(&tn)
                    && !self.config.exclude_utilities()
                {
                    TypeConversionPolicy::whole(
                        ty,
                        WholeCppConversion::FromUniquePtrToValue,
                        WholeRustConversion::FromStr,
                    )
                } else if matches!(
                    sophistication,
                    TypeConversionSophistication::SimpleForSubclasses
                ) {
                    TypeConversionPolicy::whole(
                        ty,
                        WholeCppConversion::FromUniquePtrToValue,
                        WholeRustConversion::None,
                    )
                } else {
                    TypeConversionPolicy::whole(
                        ty,
                        WholeCppConversion::FromPtrToValue,
                        WholeRustConversion::FromValueParamToPtr,
                    )
                }
            }
            Type::Ptr(tp) => {
                let pointer = BridgePointer::to((*tp.elem).clone(), tp.mutability.is_some());
                let rust_conversion = force_rust_conversion.map_or(
                    PointerRustConversion::None,
                    ForcedRustConversion::on_pointer,
                );
                if is_move_constructor {
                    TypeConversionPolicy::pointer(
                        pointer,
                        PointerCppConversion::FromPtrToMove,
                        rust_conversion,
                    )
                } else if is_rvalue_ref {
                    TypeConversionPolicy::whole(
                        (*tp.elem).clone(),
                        WholeCppConversion::FromPtrToValue,
                        WholeRustConversion::FromRValueParamToPtr,
                    )
                } else if matches!(
                    self.config.unsafe_policy,
                    UnsafePolicy::ReferencesWrappedAllFunctionsSafe
                ) && is_reference
                    && !rust_conversion_forced
                    && !is_placement_return_destination
                {
                    TypeConversionPolicy::pointer(
                        pointer,
                        PointerCppConversion::FromPointerToReference,
                        PointerRustConversion::FromReferenceWrapperToPointer,
                    )
                } else {
                    TypeConversionPolicy::pointer(
                        pointer,
                        PointerCppConversion::None,
                        rust_conversion,
                    )
                }
            }
            Type::Reference(TypeReference {
                elem, mutability, ..
            }) if matches!(
                self.config.unsafe_policy,
                UnsafePolicy::ReferencesWrappedAllFunctionsSafe
            ) && !rust_conversion_forced
                && !is_placement_return_destination =>
            {
                // A mutable C++ reference is a `Pin<&mut T>` by now, and took
                // the path branch above. `&mut T` here would mean the type
                // converter had stopped doing that, which is an autocxx bug
                // rather than anything wrong with the C++.
                if mutability.is_some() {
                    return Err(ConvertErrorFromCpp::ParameterWasNotAPointer(
                        ty.to_token_stream().to_string(),
                    ));
                }
                TypeConversionPolicy::pointer(
                    BridgePointer::to((**elem).clone(), false),
                    PointerCppConversion::FromPointerToReference,
                    PointerRustConversion::FromReferenceWrapperToPointer,
                )
            }
            _ => TypeConversionPolicy::whole(
                ty.clone(),
                WholeCppConversion::None,
                force_rust_conversion
                    .map_or(Ok(WholeRustConversion::None), |forced| forced.on_whole(ty))?,
            ),
        })
    }

    fn convert_return_type(
        &mut self,
        rt: &ReturnType,
        ns: &Namespace,
        diagnostic_name: &QualifiedName,
        sophistication: TypeConversionSophistication,
    ) -> Result<ReturnTypeAnalysis, ConvertErrorFromCpp> {
        Ok(match rt {
            ReturnType::Default => ReturnTypeAnalysis::default(),
            ReturnType::Type(rarrow, boxed_type) => {
                let annotated_type = self.convert_boxed_type(boxed_type.clone(), ns)?;
                let was_const = annotated_type.is_const;
                let boxed_type = annotated_type.ty;
                let ty: &Type = boxed_type.as_ref();
                match ty {
                    Type::Path(p)
                        if !self
                            .pod_safe_types
                            .contains(&QualifiedName::from_type_path(p)) =>
                    {
                        let tn = QualifiedName::from_type_path(p);
                        if self.moveit_safe_types.contains(&tn)
                            && matches!(sophistication, TypeConversionSophistication::Regular)
                        {
                            // This is a non-POD type we want to return to Rust as an `impl New` so that callers
                            // can decide whether to store this on the stack or heap.
                            // That means, we do not literally _return_ it from C++ to Rust. Instead, our call
                            // from Rust to C++ will include an extra placement parameter into which the object
                            // is constructed.
                            let fnarg = parse_quote! {
                                placement_return_type: *mut #ty
                            };
                            let (fnarg, analysis) = self.convert_fn_arg(
                                &fnarg,
                                ns,
                                diagnostic_name,
                                &None,
                                false,
                                false,
                                Some(ForcedRustConversion::Pointer(
                                    PointerRustConversion::FromPlacementParamToNewReturn,
                                )),
                                TypeConversionSophistication::Regular,
                                false,
                            )?;
                            ReturnTypeAnalysis {
                                rt: ReturnType::Default,
                                conversion: Some(TypeConversionPolicy::new_for_placement_return(
                                    ty.clone(),
                                )),
                                was_const,
                                deps: annotated_type.types_encountered,
                                placement_param_needed: Some((fnarg, analysis)),
                                ..Default::default()
                            }
                        } else {
                            // There are some types which we can't currently represent within a moveit::new::New.
                            // That's either because we are obliged to stick to existing protocols for compatibility
                            // (CxxString) or because they're a concrete type where we haven't attempted to do
                            // the analysis to work out the type's size. For these, we always return a plain old
                            // UniquePtr<T>. These restrictions may be fixed in future.
                            let conversion =
                                Some(TypeConversionPolicy::new_to_unique_ptr(ty.clone()));
                            ReturnTypeAnalysis {
                                rt: ReturnType::Type(*rarrow, boxed_type),
                                conversion,
                                was_const,
                                deps: annotated_type.types_encountered,
                                ..Default::default()
                            }
                        }
                    }
                    _ => {
                        let was_mutable_reference = matches!(
                            annotated_type.kind,
                            type_converter::TypeKind::MutableReference
                        );
                        let was_reference = was_mutable_reference
                            || matches!(annotated_type.kind, type_converter::TypeKind::Reference);
                        // An *rvalue* reference is neither of those kinds. It
                        // reaches here as the same pointer the type converter
                        // makes of a `T&`, and nothing in Rust spells the
                        // difference, so the conversion has to carry it: a
                        // subclass peer's override must repeat the
                        // superclass's `T&&` exactly, and the `T* f()
                        // override` we used to generate overrides nothing and
                        // does not compile. See google/autocxx#837 for the
                        // ref-qualifier half of the same story.
                        let was_rvalue_reference = matches!(
                            annotated_type.kind,
                            type_converter::TypeKind::RValueReference
                        );
                        let wraps_references = matches!(
                            self.config.unsafe_policy,
                            UnsafePolicy::ReferencesWrappedAllFunctionsSafe
                        );
                        let conversion = Some(if was_rvalue_reference {
                            TypeConversionPolicy::return_rvalue_reference(
                                ty.clone(),
                                wraps_references,
                            )?
                        } else if was_reference && wraps_references {
                            TypeConversionPolicy::return_reference_into_wrapper(ty.clone())?
                        } else {
                            TypeConversionPolicy::new_unconverted(ty.clone())
                        });
                        ReturnTypeAnalysis {
                            rt: ReturnType::Type(*rarrow, boxed_type),
                            conversion,
                            was_reference,
                            was_mutable_reference,
                            was_rvalue_reference,
                            was_const,
                            deps: annotated_type.types_encountered,
                            placement_param_needed: None,
                        }
                    }
                }
            }
        })
    }

    /// If a type has explicit constructors, bindgen will generate corresponding
    /// constructor functions, which we'll have already converted to make_unique methods.
    /// C++ mandates the synthesis of certain implicit constructors, to which we
    /// need to create bindings too. We do that here.
    /// It is tempting to make this a separate analysis phase, to be run later than
    /// the function analysis; but that would make the code much more complex as it
    /// would need to output a `FnAnalysisBody`. By running it as part of this phase
    /// we can simply generate the sort of thing bindgen generates, then ask
    /// the existing code in this phase to figure out what to do with it.
    ///
    /// Also fills out the [`PodAndConstructorAnalysis::constructors`] fields with information useful
    /// for further analysis phases.
    fn add_constructors_present(&mut self, apis: ApiVec<FnPrePhase1>) -> ApiVec<FnPrePhase2> {
        let all_items_found = find_constructors_present(&apis);
        // The types Rust holds by value, and so the only ones for which an
        // `impl Drop` costs anything - see the destructor case below.
        let pod_types: HashSet<QualifiedName> = apis
            .iter()
            .filter_map(|api| match api {
                Api::Struct {
                    name,
                    analysis:
                        PodAnalysis {
                            kind: TypeKind::Pod,
                            ..
                        },
                    ..
                } => Some(name.name.clone()),
                _ => None,
            })
            .collect();
        // C++ may have deleted some of the special members it declared for
        // `= default`. Withdraw those before we consider what to synthesize.
        let mut apis = discard_deleted_defaulted_members(apis, &all_items_found);
        // Filled in by the destructor case below, and read by the pass which
        // annotates each struct, so that codegen can assert what we assumed.
        let mut destructors_omitted_as_trivial: HashSet<QualifiedName> = HashSet::new();
        for (self_ty, items_found) in all_items_found.iter() {
            if self.config.exclude_impls {
                // Only the synthesis below is skipped. The analysis above runs
                // either way, because withdrawing the special members C++
                // deletes is right whether or not we go on to add any.
                continue;
            }
            if self
                .config
                .is_on_constructor_blocklist(&self_ty.to_cpp_name())
            {
                continue;
            }
            // `enum_style!(NewtypeEnum, ...)` and its bitfield sibling make
            // `bindgen` render a C++ `enum` as a Rust struct, so it arrives
            // here looking like a class. It isn't one: C++ has no constructors
            // or destructor to call on an enum, and writing what we normally
            // write - `p->Inner::~Inner()` for a nested type - doesn't compile.
            // The enumerators are all the API such a type has.
            //
            // The name is matched exactly, and spelled the way `generate!`
            // spells it; see `enum_style!`'s documentation.
            if self
                .config
                .enum_style(&self_ty.to_string())
                .is_some_and(|style| style.is_newtype())
            {
                continue;
            }
            let path = self_ty.to_type_path();
            if items_found.implicit_default_constructor_needed() {
                self.synthesize_special_member(
                    items_found,
                    "default_ctor",
                    &mut apis,
                    SpecialMemberKind::DefaultConstructor,
                    parse_quote! { this: *mut #path },
                );
            }
            if items_found.implicit_move_constructor_needed() {
                self.synthesize_special_member(
                    items_found,
                    "move_ctor",
                    &mut apis,
                    SpecialMemberKind::MoveConstructor,
                    parse_quote! { this: *mut #path, other: __bindgen_marker_RValueReference < *mut #path > },
                )
            }
            if items_found.implicit_const_copy_constructor_needed() {
                self.synthesize_special_member(
                    items_found,
                    "const_copy_ctor",
                    &mut apis,
                    SpecialMemberKind::CopyConstructor,
                    parse_quote! { this: *mut #path, other: __bindgen_marker_Reference < *const #path > },
                )
            }
            // A destructor which does nothing, on a type Rust holds by value,
            // is worse than useless. The `impl Drop` it needs costs the user
            // everything a type which implements `Drop` may not do: move a
            // field out, build one with `..other` update syntax, be `Copy`.
            // Nothing is bought with that, because `~T()` is trivial - C++
            // itself elides the call. Non-POD types keep their destructor: they
            // live behind a pointer or a `UniquePtr`, where an `impl Drop`
            // costs nothing, and `moveit!` on the stack wants it.
            //
            // We are not the last word on whether C++ destroys one trivially:
            // where bindgen replaced a field's type with a blob of bytes, the
            // rules were run over a fiction. The generated C++ therefore
            // asserts what we assumed - see
            // `codegen_cpp::generate_trivial_destructor_assertion` - which is
            // why the decision has to travel out of this loop.
            let destructor_would_do_nothing =
                items_found.destructor_is_trivial && pod_types.contains(self_ty);
            if destructor_would_do_nothing {
                destructors_omitted_as_trivial.insert(self_ty.clone());
            } else if items_found.implicit_destructor_needed() {
                self.synthesize_special_member(
                    items_found,
                    "destructor",
                    &mut apis,
                    SpecialMemberKind::Destructor,
                    parse_quote! { this: *mut #path },
                );
            }
        }

        // Also, annotate each type with the constructors we found.
        let mut results = ApiVec::new();
        convert_apis(
            apis,
            &mut results,
            Api::fun_unchanged,
            |name, details, analysis| {
                let items_found = all_items_found.get(&name.name);
                let destructor_omitted_as_trivial =
                    destructors_omitted_as_trivial.contains(&name.name);
                Ok(Box::new(std::iter::once(Api::Struct {
                    name,
                    details,
                    analysis: PodAndConstructorAnalysis {
                        pod: analysis,
                        constructors: if let Some(items_found) = items_found {
                            PublicConstructors::from_items_found(
                                items_found,
                                destructor_omitted_as_trivial,
                            )
                        } else {
                            PublicConstructors::default()
                        },
                    },
                })))
            },
            Api::enum_unchanged,
            Api::typedef_unchanged,
            Api::subclass_unchanged,
        );
        results
    }

    #[allow(clippy::too_many_arguments)] // it's true, but sticking with it for now
    fn synthesize_special_member(
        &mut self,
        items_found: &ItemsFound,
        label: &str,
        apis: &mut ApiVec<FnPrePhase1>,
        special_member: SpecialMemberKind,
        inputs: Punctuated<FnArg, Comma>,
    ) {
        let self_ty = items_found.name.as_ref().unwrap();
        let ident = make_ident(self.config.uniquify_name_per_mod(&format!(
            "{}_synthetic_{}",
            self_ty.name.get_final_item(),
            label
        )));
        let cpp_name = if matches!(special_member, SpecialMemberKind::DefaultConstructor) {
            // Constructors (other than move or copy) are identified in `analyze_foreign_fn` by
            // being suffixed with the cpp_name, so we have to produce that.
            self.nested_type_name_map
                .get(&self_ty.name)
                .cloned()
                .or_else(|| Some(self_ty.name.get_final_item().to_string()))
                .map(CppOriginalName::from_type_name_for_constructor)
        } else {
            None
        };
        let fake_api_name =
            ApiName::new_with_cpp_name(self_ty.name.get_namespace(), ident.clone(), cpp_name);
        let self_ty = &self_ty.name;
        let ns = self_ty.get_namespace().clone();
        let mut any_errors = ApiVec::new();
        apis.extend(
            report_any_error(&ns, &mut any_errors, || {
                let special_member_desc = special_member_to_string(special_member);
                self.analyze_foreign_fn_and_subclasses(
                    fake_api_name,
                    Box::new(FuncToConvert {
                        self_ty: Some(self_ty.clone()),
                        ident,
                        doc_attrs: make_doc_attrs(format!("Synthesized {special_member_desc}."))
                            .into_iter()
                            .map(Into::into)
                            .collect(),
                        inputs: minisynize_punctuated(inputs),
                        output: ReturnType::Default.into(),
                        vis: parse_quote! { pub },
                        virtualness: None,
                        cpp_vis: CppVisibility::Public,
                        special_member: Some(special_member),
                        method_kind: None,
                        original_name: None,
                        synthesized_this_type: None,
                        is_deleted: None,
                        add_to_trait: None,
                        synthetic_cpp: None,
                        provenance: Provenance::SynthesizedOther,
                        variadic: false,
                        ref_qualifier: CppRefQualifier::None,
                    }),
                )
            })
            .into_iter()
            .flatten(),
        );
        apis.append(&mut any_errors);
    }
}

fn special_member_to_string(special_member: SpecialMemberKind) -> &'static str {
    match special_member {
        SpecialMemberKind::DefaultConstructor => "default constructor",
        SpecialMemberKind::CopyConstructor => "copy constructor",
        SpecialMemberKind::MoveConstructor => "move constructor",
        SpecialMemberKind::Destructor => "destructor",
        SpecialMemberKind::AssignmentOperator => "assignment operator",
    }
}

/// Whether this function is a constructor, and if so the suffix which
/// distinguishes it from the type's other constructors.
///
/// bindgen classifies every method it saw, so `method_kind` answers the first
/// question outright. The suffix is then whatever the name has beyond the
/// type's own, which is nothing for every constructor bindgen reports - their
/// names are the type's - leaving them to be numbered by the overload tracker
/// which numbers everything else.
///
/// A name is consulted only where there is no kind, which means a function
/// autocxx synthesized and bindgen never saw. (A free function arrives without
/// one too, but this is reached only once a receiver type has been found.)
/// That fallback is a guess - a method C++ calls `Widget3` on class `Widget`
/// reads exactly like `Widget`'s fourth constructor - and google/autocxx#995
/// is that guess being wrong. Nothing autocxx synthesizes has a name it can be
/// wrong about: a synthesized constructor is named for its type on purpose
/// (see [`CppOriginalName::from_type_name_for_constructor`]), and every other
/// synthesized function is a wrapper whose name autocxx also chose.
fn constructor_with_suffix<'a>(
    rust_name: &'a str,
    nested_type_ident: &str,
    method_kind: Option<CppMethodKind>,
) -> Option<&'a str> {
    match method_kind {
        Some(CppMethodKind::Constructor) => Some(
            rust_name
                .strip_prefix(nested_type_ident)
                .unwrap_or_default(),
        ),
        Some(_) => None,
        None => rust_name
            .strip_prefix(nested_type_ident)
            .filter(|suffix| suffix.is_empty() || suffix.parse::<u32>().is_ok()),
    }
}

impl Api<FnPhase> {
    pub(crate) fn name_for_allowlist(&self) -> QualifiedName {
        match &self {
            Api::Function { fun, analysis, .. } => match analysis.kind {
                FnKind::Method { ref impl_for, .. } => impl_for.clone(),
                FnKind::TraitMethod { ref impl_for, .. } => impl_for.clone(),
                FnKind::Function => {
                    QualifiedName::new(self.name().get_namespace(), fun.ident.clone())
                }
            },
            Api::RustSubclassFn { subclass, .. } => subclass.0.name.clone(),
            Api::IgnoredItem {
                name,
                ctx: Some(ctx),
                ..
            } => match ctx.get_type() {
                ErrorContextType::Method { self_ty, .. } => {
                    QualifiedName::new(name.name.get_namespace(), self_ty.clone())
                }
                // `lookup`, not the sanitized name we generate code under:
                // the user's directive names the item as they know it, and
                // has never heard of our scrubbed spelling.
                ErrorContextType::Item(id) | ErrorContextType::SanitizedItem { lookup: id, .. } => {
                    QualifiedName::new(name.name.get_namespace(), id.clone())
                }
            },
            _ => self.name().clone(),
        }
    }

    /// Every name by which one of the user's allowlist directives
    /// (`generate!` and friends) might refer to this API.
    ///
    /// Most APIs answer to a single name, but a free function answers to
    /// two more:
    /// * the name it actually gets in the generated `ffi` mod. For an
    ///   overload that name is invented here, by the overload tracker, and
    ///   need not resemble anything bindgen produced: the third `daft`
    ///   overload becomes `daft3` if real `daft1` and `daft2` functions
    ///   are in the way. This is the name the user writes.
    /// * its C++ name, which all its overloads share, so that
    ///   `generate!("daft")` pulls in the whole family without the user
    ///   having to name each overload.
    pub(crate) fn allowlist_names(
        &self,
        nested_cpp_names: &NestedCppNames,
    ) -> impl Iterator<Item = String> {
        // Both of these may be nested types, and so answer to the name C++
        // knows them by as well as to the `Outer_Inner` bindgen flattened them
        // into. `name_for_allowlist` in particular is the type a method hangs
        // off, which is how `generate!("ns::Outer::Inner")` reaches the
        // methods of a nested class. See google/autocxx#1422.
        let mut names: Vec<String> = nested_cpp_names
            .spellings(self.name())
            .chain(nested_cpp_names.spellings(&self.name_for_allowlist()))
            .collect();
        let ns = self.name().get_namespace();
        // Assembled by hand rather than via QualifiedName because a
        // C++ name need not be a legal Rust identifier (`operator==`).
        // Such a name simply never matches a directive, which is fine.
        let qualify = |name: &str| ns.iter().chain(std::iter::once(name)).join("::");
        match self {
            Api::Function {
                fun,
                analysis:
                    FnAnalysis {
                        kind: FnKind::Function,
                        rust_name,
                        ..
                    },
                ..
            } => {
                names.push(qualify(rust_name));
                if let Some(original_name) = fun.original_name.as_ref() {
                    names.push(qualify(original_name.for_original_name_map()));
                }
            }
            // `name_for_allowlist` above went through `to_cpp_name`, which
            // rewrites anything sharing a name with a built-in type back to
            // its C++ spelling - the overload named `i8` would only answer
            // to `int8_t`. The user wrote the Rust name, so offer that too.
            Api::IgnoredItem { ctx: Some(ctx), .. } => match ctx.get_type() {
                ErrorContextType::Item(id) | ErrorContextType::SanitizedItem { lookup: id, .. } => {
                    names.push(qualify(&id.to_string()))
                }
                // A method answers to its type, and a type whose name
                // collided would have been sanitized into the arm above,
                // so `to_cpp_name` left this one alone.
                ErrorContextType::Method { .. } => {}
            },
            _ => {}
        }
        names.into_iter()
    }

    /// Whether this API requires generation of additional C++.
    /// This seems an odd place for this function (as opposed to in the [codegen_cpp]
    /// module) but, as it happens, even our Rust codegen phase needs to know if
    /// more C++ is needed (so it can add #includes in the cxx mod).
    /// And we can't answer the question _prior_ to this function analysis phase.
    pub(crate) fn needs_cpp_codegen(&self) -> bool {
        matches!(
            &self,
            Api::Function {
                analysis: FnAnalysis {
                    cpp_wrapper: Some(..),
                    ignore_reason: Ok(_),
                    externally_callable: true,
                    ..
                },
                ..
            } | Api::StringConstructor { .. }
                | Api::ConcreteType { .. }
                | Api::CType { .. }
                | Api::RustSubclassFn { .. }
                | Api::Subclass { .. }
                | Api::Struct {
                    analysis: PodAndDepAnalysis {
                        pod: PodAnalysis {
                            kind: TypeKind::Pod,
                            ..
                        },
                        ..
                    },
                    ..
                }
        )
    }
}

fn return_type_is_reference(output: &crate::minisyn::ReturnType) -> bool {
    if let ReturnType::Type(_, ty) = &output.0 {
        type_is_reference(ty.as_ref(), true)
    } else {
        false
    }
}

impl HasFieldsAndBases for Api<FnPrePhase1> {
    fn name(&self) -> &QualifiedName {
        self.name()
    }

    fn field_and_base_deps(&self) -> Box<dyn Iterator<Item = &QualifiedName> + '_> {
        match self {
            Api::Struct {
                analysis:
                    PodAnalysis {
                        field_definition_deps,
                        bases,
                        ..
                    },
                ..
            } => Box::new(field_definition_deps.iter().chain(bases.iter())),
            _ => Box::new(std::iter::empty()),
        }
    }
}

impl HasFieldsAndBases for Api<FnPrePhase3> {
    fn name(&self) -> &QualifiedName {
        self.name()
    }

    fn field_and_base_deps(&self) -> Box<dyn Iterator<Item = &QualifiedName> + '_> {
        match self {
            Api::Struct {
                analysis:
                    PodAndConstructorAnalysis {
                        pod:
                            PodAnalysis {
                                field_definition_deps,
                                bases,
                                ..
                            },
                        ..
                    },
                ..
            } => Box::new(field_definition_deps.iter().chain(bases.iter())),
            _ => Box::new(std::iter::empty()),
        }
    }
}

/// Stringify a function argument for diagnostics
fn describe_arg(arg: &syn::FnArg) -> String {
    match arg {
        syn::FnArg::Receiver(_) => "the function receiver (this/self paramter)".into(),
        syn::FnArg::Typed(PatType { pat, .. }) => match pat.as_ref() {
            Pat::Ident(pti) => pti.ident.to_string(),
            _ => "another argument we don't know how to describe".into(),
        },
    }
}
