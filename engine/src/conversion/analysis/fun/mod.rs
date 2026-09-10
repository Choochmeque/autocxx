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
    Explicitness, MethodKind as CppMethodKind, RefQualifier, SpecialMemberKind, Virtualness,
};
use crate::{
    conversion::{
        analysis::{
            fun::function_wrapper::{BridgePointer, CppFunctionKind},
            type_converter::{
                self, add_analysis, attach_deferred_holder_surfaces, TypeConversionContext,
                TypeConverter, UndestroyableMember,
            },
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
        type_helpers::{
            cpp_array_element, denotes_cpp_array_behind_pointer, is_volatile_qualified,
            type_is_reference, unwrap_has_opaque,
        },
        CppEffectiveName, CppOriginalName,
    },
    known_types::known_types,
    minisyn::{minisynize_punctuated, FnArg},
    parse_callbacks::{MemberFunctionTemplate, TemplateMemberFunction, UsingDeclaration},
    types::validate_ident_ok_for_rust,
};
use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;

use crate::vendored_bindgen::callbacks::ExceptionSpecification;
use autocxx_parser::{ExternCppType, IncludeCppConfig, UnsafePolicy};
use function_wrapper::{
    CppExceptionSpecification, CppFunction, CppFunctionBody, TypeConversionPolicy,
    RECEIVER_ARG_NAME,
};
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
    ParseCallbackResults,
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
        create_subclass_trait_item, override_exception_specification,
    },
};

use super::{
    depth_first::HasFieldsAndBases,
    doc_label::make_doc_attrs,
    pod::{pod_safe_types, PodAnalysis, PodPhase},
    tdef::{instantiable_concrete_types, TypedefAnalysis},
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
    /// Whether C++ qualified the return type itself `volatile`, which is the
    /// same problem as `was_const` and takes the same way out. The qualifier
    /// is spent once the value has been copied out of C++, so the wrapper
    /// returns the unqualified type and calls through.
    was_volatile: bool,
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
            was_volatile: false,
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
    /// Set where destroying one of these is C++ a compiler will not accept:
    /// the class holds by value a template instantiation built on a type
    /// nothing defines, and declares no destructor of its own, so destroying
    /// one is where C++ writes the destructor which destroys that member. See
    /// [`ConvertErrorFromCpp::MemberOfInstantiationOnIncompleteType`], which
    /// is what the positions refusing it say.
    pub(crate) undestroyable_member: Option<UndestroyableMember>,
}

impl PublicConstructors {
    fn from_items_found(
        items_found: &ItemsFound,
        destructor_omitted_as_trivial: bool,
        undestroyable_member: Option<UndestroyableMember>,
    ) -> Self {
        Self {
            move_constructor: items_found.move_constructor.callable_any(),
            destructor: items_found.destructor.callable_any(),
            destructor_inaccessible: !items_found.destructor.callable_any(),
            why_no_constructors: items_found.why_no_constructors.clone(),
            destructor_omitted_as_trivial,
            abstract_without_virtual_destructor: false,
            undestroyable_member,
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
    /// The template instantiations the user declared `instantiable!`. No
    /// allowlist directive can name one, so the allowlist check on methods
    /// exempts them - see `analyze_foreign_fn`.
    instantiable_concrete_types: HashSet<QualifiedName>,
    types_in_anonymous_namespace: HashSet<QualifiedName>,
    existing_superclass_trait_api_names: HashSet<QualifiedName>,
    cpp_names_taken_on_peer_classes: HashSet<String>,
    /// For each class named as the source of a `using Base::foo;`, the classes
    /// which wrote one and the name each imported. Keyed on the base because
    /// that is where the member's signature is, and a member is only visible
    /// here when it is analyzed.
    using_declarations_by_base: HashMap<QualifiedName, Vec<ImportedMember>>,
    /// Every class's base classes, for the two passes which have to answer
    /// what a name means when it is looked up in a derived class.
    ancestry: HashMap<QualifiedName, Ancestry>,
    /// The enumerations C++ declared `enum class` or `enum struct`, whose
    /// enumerators are members of the enumeration rather than of the class it
    /// is nested in.
    scoped_enums: HashSet<QualifiedName>,
    /// Every enumeration, scoped or not. An enum is a scalar, which is what
    /// decides whether a `volatile` value of it can be copied out of C++.
    enums: HashSet<QualifiedName>,
    /// For each class template some concrete instantiation in these APIs
    /// instantiates, the member functions bindgen reported for it. Keyed on the
    /// template, because that is the only thing bindgen says anything about: it
    /// announces no item for an instantiation at all.
    template_member_functions: HashMap<QualifiedName, Vec<TemplateMemberFunction>>,
    /// For each class these APIs give methods to, the member function templates
    /// bindgen reported for it. Keyed on the class which declares them, which
    /// for an instantiation is the class template: bindgen says nothing about
    /// an instantiation, so the template's declaration is all there is.
    member_function_templates: HashMap<QualifiedName, Vec<MemberFunctionTemplate>>,
    force_wrapper_generation: bool,
}

/// What one class's declaration says about where its members come from.
struct Ancestry {
    /// Every base bindgen named, whatever the access. This is what C++ member
    /// name lookup walks: it finds a name first and applies access control to
    /// what it found afterwards, so a private base still hides and still makes
    /// a name ambiguous.
    bases: HashSet<QualifiedName>,
    /// The subset C++ inherits publicly, which is the only ancestry a member
    /// can actually be *called* through from outside the class.
    public_bases: HashSet<QualifiedName>,
    /// Whether bindgen reported a base it could not name - a template
    /// instantiation, which it announces through no callback. Such a base may
    /// declare anything and lead anywhere, so nothing here can be counted.
    has_unnamed_base: bool,
}

impl Ancestry {
    fn bases(&self, inheritance: Inheritance) -> &HashSet<QualifiedName> {
        match inheritance {
            Inheritance::Any => &self.bases,
            Inheritance::Public => &self.public_bases,
        }
    }
}

/// Which bases a walk up an ancestry may pass through.
#[derive(Clone, Copy)]
enum Inheritance {
    /// Any base. A `using Base::foo;` may name a private one - re-exporting a
    /// private base's member is what the declaration is for.
    Any,
    /// Only bases C++ inherits publicly, which is what an outside caller can
    /// reach a member through without the class saying so.
    Public,
}

/// One `using Base::foo;`: the class which wrote it and the name it imported.
struct ImportedMember {
    importer: QualifiedName,
    name: String,
    /// The access the using-declaration gives the name, which is the access
    /// the imported member has on the importer and need not be the access it
    /// has on the base: widening a `protected` member is one of the things a
    /// using-declaration is for.
    visibility: CppVisibility,
}

impl<'a> FnAnalyzer<'a> {
    pub(crate) fn analyze_functions(
        apis: ApiVec<PodPhase>,
        unsafe_policy: &'a UnsafePolicy,
        config: &'a IncludeCppConfig,
        parse_callback_results: &ParseCallbackResults,
        force_wrapper_generation: bool,
    ) -> ApiVec<FnPrePhase3> {
        let ancestry = Self::build_ancestry(&apis);
        let scoped_enums = apis
            .iter()
            .filter_map(|api| match api {
                Api::Enum { name, .. } if parse_callback_results.is_scoped_enum(&name.name) => {
                    Some(name.name.clone())
                }
                _ => None,
            })
            .collect();
        let mut me = Self {
            unsafe_policy,
            extra_apis: ApiVec::new(),
            type_converter: TypeConverter::new(config, &apis, parse_callback_results),
            bridge_name_tracker: BridgeNameTracker::new(),
            config,
            overload_trackers_by_mod: HashMap::new(),
            pod_safe_types: pod_safe_types(&apis),
            moveit_safe_types: Self::build_correctly_sized_type_set(&apis),
            subclasses_by_superclass: subclass::subclasses_by_superclass(&apis),
            nested_type_name_map: Self::build_nested_type_map(&apis),
            nested_cpp_names: NestedCppNames::new(config, apis.iter().map(|api| api.name_info())),
            generic_types: Self::build_generic_type_set(&apis),
            instantiable_concrete_types: instantiable_concrete_types(&apis, config),
            existing_superclass_trait_api_names: HashSet::new(),
            cpp_names_taken_on_peer_classes: Self::build_virtual_method_cpp_names(&apis),
            types_in_anonymous_namespace: Self::build_types_in_anonymous_namespace(&apis),
            using_declarations_by_base: Self::build_using_declarations_by_base(&apis, &ancestry),
            ancestry,
            scoped_enums,
            enums: apis
                .iter()
                .filter_map(|api| match api {
                    Api::Enum { name, .. } => Some(name.name.clone()),
                    _ => None,
                })
                .collect(),
            template_member_functions: Self::build_template_member_functions(
                &apis,
                parse_callback_results,
            ),
            member_function_templates: Self::build_member_function_templates(
                &apis,
                parse_callback_results,
            ),
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
        // Before the passes below, which decide what else to put on a concrete
        // type by asking whether it has a surface: one worked out for a type
        // which already existed is not on it yet, and a holder which acquired
        // its `get` afterwards would have been given the template's own `get`
        // as well - two methods of one name, in generated Rust which does not
        // compile.
        let results = attach_deferred_holder_surfaces(&mut me.type_converter, results);
        let results = me.add_using_declaration_imports(results);
        let results = me.add_inherited_member_imports(results);
        let results = me.add_template_instantiation_members(results);
        let results = me.add_member_function_template_notes(results);
        let results = me.add_constructors_present(results);
        let mut results = me.add_subclass_constructors(results);
        results.extend(me.extra_apis.into_iter().map(add_analysis));
        // Again, because the passes above convert types of their own and may
        // have worked out a surface for a holder since.
        attach_deferred_holder_surfaces(&mut me.type_converter, results)
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

    /// Return the set of 'moveit safe' types. That must include only types where
    /// the size is known to be correct.
    ///
    /// Read for return values only. A constructor asks
    /// `find_types_with_no_rust_storage` instead, and
    /// `generate_constructor_impl` hands back a `UniquePtr` rather than a
    /// `New` for anything in it.
    ///
    /// A struct stays in here even if `mark_types_abstract` later makes it a
    /// `TypeKind::Abstract`, whose Rust side *is* cxx's zero-sized opaque
    /// type - this set is built before abstractness is worked out. A function
    /// returning such a type by value therefore gets an `impl New` over a
    /// zero-sized Rust type, and the only bar to that is the C++ compiler:
    /// the shim autocxx generates *calls* that function, which C++ refuses for
    /// a genuinely abstract class (the declaration alone is accepted by both
    /// gcc and clang, whatever [class.abstract]/3 says; the diagnosis comes at
    /// the call). Constructors, `CopyNew` and `MoveNew` are removed for such a
    /// type, so the by-value return is the only way in at all.
    ///
    /// What that leaves is a class autocxx calls abstract while C++ does not,
    /// where the shim would compile. Two attempts to build one failed - a
    /// private override and a typedef'd parameter type, both of which
    /// `mark_types_abstract` sees through - but "no counterexample found" is
    /// not the same as "cannot happen", and that analysis reconstructs
    /// inheritance and compares converted `syn::Type` signatures rather than
    /// asking C++. The fix if one is ever found is to decide the return shape
    /// from the final Rust representation rather than from a set built four
    /// phases earlier.
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

    /// The member functions bindgen reported for each class template which
    /// some concrete instantiation here instantiates.
    ///
    /// Collected from the instantiations rather than from the class templates,
    /// because an instantiation is the only thing which is going to ask: a
    /// class template's own members are never bound - see
    /// `ConvertErrorFromCpp::MethodOfGenericType`.
    fn build_template_member_functions(
        apis: &ApiVec<PodPhase>,
        parse_callback_results: &ParseCallbackResults,
    ) -> HashMap<QualifiedName, Vec<TemplateMemberFunction>> {
        apis.iter()
            .filter_map(|api| match api {
                Api::ConcreteType {
                    rs_definition,
                    cpp_definition,
                    holder_surface: None,
                    ..
                } => instantiated_template(rs_definition.as_deref(), cpp_definition),
                _ => None,
            })
            .filter_map(|template| {
                let members = parse_callback_results.template_member_functions(&template);
                (!members.is_empty()).then(|| (template, members.to_vec()))
            })
            .collect()
    }

    /// The member function templates bindgen reported for each class which is
    /// going to have an `impl` block to put a note in.
    ///
    /// Two kinds of class ask: an ordinary one, which declares its own, and a
    /// concrete instantiation, whose members are the class template's - keyed
    /// on the template for the same reason
    /// [`Self::build_template_member_functions`] is.
    fn build_member_function_templates(
        apis: &ApiVec<PodPhase>,
        parse_callback_results: &ParseCallbackResults,
    ) -> HashMap<QualifiedName, Vec<MemberFunctionTemplate>> {
        apis.iter()
            .filter_map(|api| match api {
                Api::Struct { name, .. } => Some(name.name.clone()),
                Api::ConcreteType {
                    rs_definition,
                    cpp_definition,
                    holder_surface: None,
                    ..
                } => instantiated_template(rs_definition.as_deref(), cpp_definition),
                _ => None,
            })
            .filter_map(|owner| {
                let members = parse_callback_results.member_function_templates(&owner);
                (!members.is_empty()).then(|| (owner, members.to_vec()))
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

    /// Every class's bases, as the two import passes need them.
    fn build_ancestry(apis: &ApiVec<PodPhase>) -> HashMap<QualifiedName, Ancestry> {
        apis.iter()
            .filter_map(|api| match api {
                Api::Struct {
                    name,
                    analysis:
                        PodAnalysis {
                            bases,
                            public_bases,
                            has_unnamed_base,
                            ..
                        },
                    ..
                } => Some((
                    name.name.clone(),
                    Ancestry {
                        bases: bases.clone(),
                        public_bases: public_bases.clone(),
                        has_unnamed_base: *has_unnamed_base,
                    },
                )),
                _ => None,
            })
            .collect()
    }

    /// Index every `using Base::foo;` by the base class it names, for the
    /// declarations whose effect autocxx can be sure of.
    ///
    /// Being sure of it means knowing which member `d.foo(args)` would call in
    /// the C++ shim, and that is only knowable when nothing else contributes a
    /// `foo` to the derived class's lookup. Every declaration which leaves
    /// that open is dropped and goes on doing what it did before this map
    /// existed, which is nothing:
    ///
    /// - A name the class writes more than one using-declaration for, which is
    ///   the C++ idiom for merging two bases' overloads into one set.
    /// - A name whose declaration names a base autocxx cannot identify:
    ///   bindgen spells the base as C++ writes it and autocxx flattens a
    ///   nested class's name, so the two need not agree.
    /// - A name whose base is not reached exactly once. C++ lets a
    ///   using-declaration name any base, direct or not, so the whole ancestry
    ///   is searched, but a class reached twice is two base subobjects and C++
    ///   rejects the conversion to it at the call rather than at the
    ///   declaration. Two paths through a *virtual* base do share one
    ///   subobject and would be callable; they are declined all the same,
    ///   because bindgen reports which bases are virtual only for the class
    ///   which declares them.
    /// - A name the base itself writes a using-declaration for, because the
    ///   base's own `foo` is then a merged set too and passes the merge on.
    ///
    /// Whether the *importer* declares a `foo` of its own is not decided here:
    /// that needs the analysis which [`Self::add_using_declaration_imports`]
    /// waits for.
    fn build_using_declarations_by_base(
        apis: &ApiVec<PodPhase>,
        ancestry: &HashMap<QualifiedName, Ancestry>,
    ) -> HashMap<QualifiedName, Vec<ImportedMember>> {
        // Grouped by the name each introduces, and by the class which wrote
        // it, before any is judged: a declaration this cannot use still puts
        // its base's members in the derived class's lookup, so it has to be
        // able to veto the others.
        let mut declarations: HashMap<(&QualifiedName, &str), Vec<&UsingDeclaration>> =
            HashMap::new();
        for api in apis.iter() {
            let Api::Struct {
                name,
                analysis: PodAnalysis {
                    using_declarations, ..
                },
                ..
            } = api
            else {
                continue;
            };
            for using in using_declarations {
                // An inherited constructor - `using Base::Base;` - which clang
                // names after the derived class. Constructing a derived class
                // through one is a feature of its own, not a member import.
                if using.name == name.name.get_final_item() {
                    continue;
                }
                declarations
                    .entry((&name.name, using.name.as_str()))
                    .or_default()
                    .push(using);
            }
        }
        let mut by_base: HashMap<QualifiedName, Vec<ImportedMember>> = HashMap::new();
        for ((importer, name), written) in &declarations {
            let [using] = written.as_slice() else {
                continue;
            };
            let Some(base) = using
                .source_scope
                .as_ref()
                .filter(|base| reached_exactly_once(ancestry, importer, base, Inheritance::Any))
                .filter(|base| !declarations.contains_key(&(*base, *name)))
            else {
                continue;
            };
            by_base
                .entry(base.clone())
                .or_default()
                .push(ImportedMember {
                    importer: (*importer).clone(),
                    name: (*name).to_string(),
                    visibility: using.visibility,
                });
        }
        by_base
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
            // What the override has to say about exceptions, or the reason
            // there is no answer: see `override_exception_specification`. A
            // method with no answer still reserves the `_super` name it would
            // have used below, and leaves it unused: names are minted once for
            // all the subclasses of a superclass and the escape in
            // `get_cpp_super_fn_name` walks further each time, so letting a
            // refusal skip one would move the name a later method gets.
            let exception_specification =
                override_exception_specification(fun.exception_specification);
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
                let exception_specification = match &exception_specification {
                    Ok(exception_specification) => *exception_specification,
                    Err(err) => {
                        results.push(Api::IgnoredItem {
                            name: ApiName::new_in_root_namespace(make_ident(format!(
                                "{}_{}",
                                sub.0.name.get_final_item(),
                                name.name.get_final_item()
                            ))),
                            err: err.clone(),
                            ctx: None,
                        });
                        continue;
                    }
                };
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
                    exception_specification,
                    super_fn_cpp_name
                        .as_ref()
                        .map(QualifiedName::get_final_ident),
                    fun.deprecation.is_some(),
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

    /// Bind, against each class which wrote a `using Base::foo;`, every member
    /// of that base which the declaration makes reachable through it.
    ///
    /// Runs once every class has been through function analysis, because what
    /// the importer declares for itself decides the answer, and a class whose
    /// own methods are still being analyzed cannot be asked.
    ///
    /// A name the base overloads is not imported either. Which member
    /// `d.foo(args)` selects is C++'s overload resolution to decide, and
    /// neither the parameter types nor their number settle it: a longer
    /// overload may have default arguments which make it callable with fewer,
    /// and two of the same length may differ only in ways which make the call
    /// ambiguous, such as `foo(int)` beside `foo(const int&)`. bindgen reports
    /// neither the default arguments nor enough of the types to tell, so the
    /// one shim which can be relied on is the one whose name resolves to a
    /// single member.
    ///
    /// A class which declares a member function of the same name gets no
    /// import at all. The declaration is then the C++ idiom for merging the
    /// base's overloads into the importer's own set, and which of them is
    /// still callable through the derived class - and which call the shim's
    /// `d.foo(args)` would select - turns on hiding, on ref-qualifiers and on
    /// default arguments, of which bindgen reports only the second. Answering
    /// from the name and the parameter types alone got each of those wrong in
    /// turn, and each produced C++ which did not compile rather than a binding
    /// which was merely missing.
    ///
    /// Only member *functions* are imported. A using-declaration may also name
    /// a static member, whose call needs no receiver and so needs a shim of a
    /// different shape, or a data member, which is not a function at all;
    /// neither is bound, exactly as before. Nor does an import chain: a member
    /// `B` imported from `A` is not imported onward by a `C` which writes
    /// `using B::foo;`, because the import is made from what bindgen reported
    /// and bindgen reports no member of `B` for it.
    fn add_using_declaration_imports(&mut self, apis: ApiVec<FnPrePhase1>) -> ApiVec<FnPrePhase1> {
        if self.using_declarations_by_base.is_empty() {
            return apis;
        }

        // The member function names each class declares for itself.
        let declared: HashSet<(QualifiedName, String)> = apis
            .iter()
            .filter_map(|api| match api {
                Api::Function {
                    name,
                    analysis:
                        FnAnalysis {
                            kind: FnKind::Method { impl_for, .. },
                            ..
                        },
                    ..
                } => Some((
                    impl_for.clone(),
                    name.cpp_name().to_string_for_cpp_generation().to_string(),
                )),
                _ => None,
            })
            .collect();

        // How many members of each name a class declares. A name it overloads
        // is a name the shim cannot call: which member `d.foo(args)` selects
        // is C++'s overload resolution to decide, and neither the parameter
        // types nor their number settle it - a longer overload may have
        // default arguments which make it callable with fewer, and two of the
        // same length may differ only in ways which make the call ambiguous,
        // such as `foo(int)` beside `foo(const int&)`. bindgen reports neither
        // the default arguments nor enough of the types to tell.
        let mut members_named: HashMap<(QualifiedName, String), usize> = HashMap::new();
        for api in apis.iter() {
            if let Api::Function {
                name,
                analysis:
                    FnAnalysis {
                        kind: FnKind::Method { impl_for, .. },
                        ..
                    },
                ..
            } = api
            {
                *members_named
                    .entry((
                        impl_for.clone(),
                        name.cpp_name().to_string_for_cpp_generation().to_string(),
                    ))
                    .or_default() += 1;
            }
        }

        let mut imports = Vec::new();
        for api in apis.iter() {
            let Api::Function {
                name,
                fun,
                analysis:
                    FnAnalysis {
                        kind:
                            FnKind::Method {
                                impl_for: base,
                                method_kind:
                                    MethodKind::Normal
                                    | MethodKind::Virtual(_)
                                    | MethodKind::PureVirtual(_),
                            },
                        ..
                    },
            } = api
            else {
                continue;
            };
            let cpp_name = name.cpp_name().to_string_for_cpp_generation().to_string();
            if members_named.get(&(base.clone(), cpp_name.clone())) != Some(&1) {
                continue;
            }
            for imported in self
                .using_declarations_by_base
                .get(base)
                .into_iter()
                .flatten()
                .filter(|imported| imported.name == cpp_name)
            {
                if declared.contains(&(imported.importer.clone(), cpp_name.clone())) {
                    continue;
                }
                imports.push((
                    imported.importer.clone(),
                    import_member_into(&imported.importer, imported.visibility, name, fun, None),
                ));
            }
        }

        let mut results = apis;
        for (_, (name, fun)) in imports {
            self.analyze_and_add(
                name,
                fun,
                &mut results,
                TypeConversionSophistication::Regular,
                None,
            );
        }
        results
    }

    /// Bind each public member of each public base class a second time,
    /// against the classes which inherit it.
    ///
    /// C++ calls an inherited member on the derived object - `d.foo()` - and
    /// bindgen reports nothing of the sort: a base arrives as a field and its
    /// members as functions over the base's own type. Where the base is on the
    /// allowlist autocxx generates an upcast and the member can be called on
    /// the result; where it is not, the member is discarded outright as a
    /// `MethodOfNonAllowlistedType` and there is nothing to call at all. That
    /// second case is google/autocxx#197: a virtual function declared on a base
    /// nobody asked autocxx to generate is unreachable from Rust, and the base
    /// cannot be allowlisted into existence by every user who meets one.
    ///
    /// So each member is imported into the deriving class much as a
    /// `using Base::foo;` re-exports one, sharing that pass's
    /// [`import_member_into`]. The shim makes its call on the receiver cast to
    /// the base - `static_cast<const Base&>(d).foo(args)`, see
    /// [`CppFunctionBody::BaseClassMethodCall`] - so the name is looked up in
    /// the class which declared it rather than in the class it is being
    /// reached through. It has to be: several kinds of declaration hide an
    /// inherited member and bindgen reports none of them well enough to be
    /// sure of, a member function template not at all, and a shim written
    /// `d.foo(args)` would then call something other than the member whose
    /// signature became the Rust binding.
    ///
    /// What is left for the rules below is which members are worth binding,
    /// which is C++'s question all the same: bind one only where `d.foo(args)`
    /// would have reached it, so that Rust says what C++ says. That means
    ///
    /// - the base is *one* subobject of the deriving class, reached over
    ///   public inheritance. Those are two questions, and both are asked: a
    ///   second path is a second subobject, which C++ rejects the conversion
    ///   to however that path is inherited, and a path which is not public is
    ///   not a conversion an outside caller may make at all. Two paths through
    ///   a *virtual* base do share one subobject and would convert; they are
    ///   declined all the same, because bindgen reports which bases are
    ///   virtual only for the class which declares them. So is a path through
    ///   a base bindgen could not name, which may lead to the same class again
    ///   and would make the count an undercount rather than an answer.
    /// - the name resolves, by C++'s own member lookup, to this base and
    ///   nothing else. A member the deriving class declares hides the
    ///   inherited one; so does one an intermediate class declares; and a name
    ///   two unrelated bases both declare makes the call ambiguous rather than
    ///   choosing. Lookup runs over *every* base, public or not, because C++
    ///   looks a name up before it asks whether the caller may have it.
    ///   bindgen does not report every kind of declaration well enough for
    ///   this to be exhaustive - an anonymous union's members, an unnamed
    ///   enum's enumerators and a member function template are each invisible
    ///   here - so a hidden member is sometimes bound anyway. It is bound
    ///   correctly, the shim's cast settling what it calls, and it is bound
    ///   under a name C++ would have read as the hiding declaration's: more
    ///   than the C++ author exposed, never something other than what it says.
    /// - the base is a class bindgen reported a C++ name for, since that name
    ///   is what the cast has to write. A class it named nothing for is one
    ///   C++ hid - a private nested class, say - whose public members remain
    ///   callable on a derived object which nothing outside may cast.
    /// - the base writes no `using Other::foo;` of its own. Such a declaration
    ///   merges another class's members of that name into the base's, and the
    ///   merged set is not something the shim can select from either. One
    ///   written further down the ancestry needs no rule of its own: where the
    ///   preceding pass could bind it, it is a member function of the class
    ///   which wrote it and the lookup above stops there.
    /// - the base declares that name once. An overload set is not something
    ///   the shim can select from either: bindgen reports neither default
    ///   arguments nor enough of the types to say which member `d.foo(args)`
    ///   would pick.
    ///
    /// Only public members are imported, and only member functions: a static
    /// member's call needs no receiver and so needs a shim of a different
    /// shape, and a data member is not a function. A member autocxx discarded
    /// for a reason of its own - a parameter it could not convert, say - is
    /// left discarded, since importing it would only fail the same way against
    /// the deriving class. Being a method of a class off the allowlist is the
    /// one such reason this overrides, that being the whole point.
    ///
    /// Running after [`Self::add_using_declaration_imports`] is what keeps the
    /// two from binding one member twice: a name that pass imported is a name
    /// the importing class now declares, so lookup stops there.
    ///
    /// A Rust subclass of the deriving class still cannot override a virtual
    /// member imported this way. Subclass items are made during the analysis
    /// pass, from the class the member was declared on, and that class is the
    /// base rather than the superclass the subclass named.
    fn add_inherited_member_imports(&mut self, apis: ApiVec<FnPrePhase1>) -> ApiVec<FnPrePhase1> {
        // The classes which might import something, in API order.
        let importers: Vec<QualifiedName> = apis
            .iter()
            .filter_map(|api| match api {
                Api::Struct { name, .. } => Some(&name.name),
                _ => None,
            })
            .filter(|name| {
                self.ancestry
                    .get(*name)
                    .is_some_and(|ancestry| !ancestry.public_bases.is_empty())
            })
            // A class off the allowlist has its own members discarded, so an
            // imported one would be discarded too; and a generic class, a
            // class in an anonymous namespace and a class standing in for one
            // of cxx's own types each refuse every method they are given.
            .filter(|name| {
                self.is_on_allowlist(name)
                    && !self.is_generic_type(name)
                    && !self.types_in_anonymous_namespace.contains(*name)
                    && known_types().is_cxx_acceptable_receiver(name)
            })
            .cloned()
            .collect();
        if importers.is_empty() {
            return apis;
        }

        // Every member name each class declares, which is what hides an
        // inherited one; how many member *functions* it declares of each name,
        // which is what says an overload set cannot be imported; and the names
        // it merges with a using-declaration, which is what says the members
        // of that name cannot be enumerated at all.
        let mut declared_names: HashMap<QualifiedName, HashSet<String>> = HashMap::new();
        let mut member_functions: HashMap<(QualifiedName, String), usize> = HashMap::new();
        let mut merged_names: HashSet<(QualifiedName, String)> = HashSet::new();
        // Each class by the C++ scope its nested items are reported under.
        // Keyed by namespace too: a C++ name reported for an item carries the
        // enclosing types but not the enclosing namespaces, so `n::D` and
        // `m::D` are both reported as `D`.
        let classes_by_cpp_scope: HashMap<(&Namespace, String), &QualifiedName> = apis
            .iter()
            .filter_map(|api| match api {
                Api::Struct { name, .. } => Some((
                    (
                        name.name.get_namespace(),
                        name.cpp_name().to_string_for_cpp_generation().to_string(),
                    ),
                    &name.name,
                )),
                _ => None,
            })
            .collect();
        for api in apis.iter() {
            match api {
                Api::Function {
                    name,
                    analysis:
                        FnAnalysis {
                            kind: FnKind::Method { impl_for, .. },
                            ..
                        },
                    ..
                } => {
                    let cpp_name = name.cpp_name().to_string_for_cpp_generation().to_string();
                    declared_names
                        .entry(impl_for.clone())
                        .or_default()
                        .insert(cpp_name.clone());
                    *member_functions
                        .entry((impl_for.clone(), cpp_name))
                        .or_default() += 1;
                }
                Api::Struct {
                    name,
                    analysis:
                        PodAnalysis {
                            field_info,
                            bitfields,
                            using_declarations,
                            ..
                        },
                    ..
                } => {
                    let names = declared_names.entry(name.name.clone()).or_default();
                    for member in field_info
                        .iter()
                        .filter_map(|field| field.name.as_deref())
                        .chain(bitfields.iter().filter_map(|member| member.name.as_deref()))
                    {
                        // Under bindgen's spelling, plus the same without a
                        // trailing underscore, which is how bindgen spells a
                        // member C++ named after a Rust keyword. Guessing
                        // wrong here only ever declines an import.
                        names.insert(member.to_string());
                        if let Some(unmangled) = member.strip_suffix('_') {
                            names.insert(unmangled.to_string());
                        }
                    }
                    // A using-declaration declares the name in the class which
                    // wrote it, whatever the preceding pass was able to make of
                    // it, and its members of that name are then not all its
                    // own. So the name is a declaration for lookup, and the
                    // class is one no member of that name may be imported
                    // from: which of a merged set a call selects is no more
                    // answerable than which of an overload set.
                    for using in using_declarations {
                        names.insert(using.name.clone());
                        merged_names.insert((name.name.clone(), using.name.clone()));
                    }
                }
                Api::Enum { name, item, .. } if !self.scoped_enums.contains(&name.name) => {
                    // An unscoped enumerator is a member of the class the enum
                    // is nested in, and hides an inherited function of its
                    // name. bindgen reports the enum's own name qualified by
                    // that class, which is how the class is found.
                    //
                    // A *scoped* enum's enumerators are not members of the
                    // enclosing class and hide nothing, so this arm passes one
                    // by. Which kind of enum C++ declared is not in bindgen's
                    // output - the two generate the same Rust - and reaches us
                    // through `ParseCallbacks::denote_scoped_enum`.
                    let enclosing = name
                        .cpp_name_if_present()
                        .and_then(|cpp_name| cpp_name.enclosing_cpp_scope())
                        .and_then(|scope| {
                            classes_by_cpp_scope
                                .get(&(name.name.get_namespace(), scope.to_string()))
                        });
                    if let Some(class) = enclosing {
                        declared_names
                            .entry((*class).clone())
                            .or_default()
                            .extend(item.variants.iter().map(|v| v.ident.to_string()));
                    }
                }
                _ => {}
            }
        }
        // Everything else a class declares, found by the name bindgen gave it:
        // a member of class `X` lands in the enclosing mod as `X_member`, so
        // that is where a nested type, a nested enum or a static data member
        // which hides an inherited function is to be found. Items which carry
        // a C++ name of their own are asked instead, since an unrelated
        // `X_member` written that way in C++ says so and is nobody's member;
        // it is the ones bindgen named for itself which follow the convention.
        let flat_scope: HashSet<String> = apis
            .iter()
            .filter(|api| {
                api.name_info()
                    .cpp_name_if_present()
                    .is_none_or(|cpp_name| cpp_name.is_nested())
            })
            .map(|api| api.name().to_string())
            .collect();

        // Each class by the C++ spelling of its name, which is what the shim's
        // cast has to write. Taken here rather than left to codegen because
        // codegen's name map holds only the classes which survive garbage
        // collection, and a base nobody asked for does not; its fallback is
        // bindgen's flattened identifier, which names nothing in C++. A class
        // bindgen reported no name for is absent altogether: it is one C++ hid
        // - a private nested class, say - whose public members stay callable
        // on a derived object that nothing outside may cast.
        let nameable_classes: HashMap<&QualifiedName, String> = apis
            .iter()
            .filter_map(|api| match api {
                Api::Struct { name, .. } => name.cpp_name_if_present().map(|cpp_name| {
                    (
                        &name.name,
                        name.name
                            .get_namespace()
                            .iter()
                            .chain(std::iter::once(cpp_name.for_original_name_map()))
                            .join("::"),
                    )
                }),
                _ => None,
            })
            .collect();
        let mut imports = Vec::new();
        for api in apis.iter() {
            let Api::Function {
                name,
                fun,
                analysis:
                    FnAnalysis {
                        kind:
                            FnKind::Method {
                                impl_for: base,
                                method_kind:
                                    MethodKind::Normal
                                    | MethodKind::Virtual(_)
                                    | MethodKind::PureVirtual(_),
                            },
                        ignore_reason,
                        param_details,
                        ..
                    },
            } = api
            else {
                continue;
            };
            if !matches!(fun.cpp_vis, CppVisibility::Public) {
                continue;
            }
            // Only a member C++ declared. The shim calls the member by name on
            // the base, and a method autocxx synthesized has no member of that
            // name for it to call: a field accessor is named after a *data*
            // member, and `static_cast<const Base&>(self).b(...)` on one is
            // "called object type is not a function". An inherited field is
            // therefore read through the base rather than through the derived
            // class - the accessor exists on the base, and the upcast is how
            // to reach it.
            if !matches!(fun.provenance, Provenance::Bindgen) {
                continue;
            }
            // The receiver the base declared the member with, which is the
            // receiver the shim takes and so the constness its cast needs.
            let Some((_, receiver_mutability)) = param_details
                .first()
                .and_then(|param| param.self_type.as_ref())
            else {
                continue;
            };
            if !matches!(
                ignore_reason,
                Ok(())
                    | Err(ConvertErrorWithContext(
                        ConvertErrorFromCpp::MethodOfNonAllowlistedType,
                        _
                    ))
            ) {
                continue;
            }
            let Some(base_cpp_spelling) = nameable_classes.get(base) else {
                continue;
            };
            let cpp_name = name.cpp_name().to_string_for_cpp_generation().to_string();
            if member_functions.get(&(base.clone(), cpp_name.clone())) != Some(&1)
                || merged_names.contains(&(base.clone(), cpp_name.clone()))
            {
                continue;
            }
            for derived in &importers {
                // One base subobject, publicly reached: neither question
                // answers the other. Two paths make two subobjects whatever
                // their access, and one public path may still leave a second
                // subobject sitting behind a private one.
                if derived == base
                    || !reached_exactly_once(&self.ancestry, derived, base, Inheritance::Any)
                    || !reached_exactly_once(&self.ancestry, derived, base, Inheritance::Public)
                {
                    continue;
                }
                let resolves_here = matches!(
                    look_up_member(
                        &self.ancestry,
                        &declared_names,
                        &flat_scope,
                        derived,
                        &cpp_name
                    ),
                    MemberLookup::Found(found) if found == *base
                );
                if resolves_here {
                    imports.push(import_member_into(
                        derived,
                        CppVisibility::Public,
                        name,
                        fun,
                        Some((base_cpp_spelling, receiver_mutability)),
                    ));
                }
            }
        }

        let mut results = apis;
        for (name, fun) in imports {
            self.analyze_and_add(
                name,
                fun,
                &mut results,
                TypeConversionSophistication::Regular,
                None,
            );
        }
        results
    }

    /// Bind the member functions of each `instantiable!` concrete template
    /// instantiation, which C++ calls on the instantiation itself - `a.foo()` -
    /// and bindgen generates nothing whatsoever for.
    ///
    /// bindgen discards a class template's member functions while parsing,
    /// because there is no monomorphization for it to emit code for, and it
    /// reports nothing at all about a specialization. So an instantiation
    /// arrived with no methods however many the template declares: that is the
    /// method half of google/autocxx#723, which the constructor work beside it
    /// does not reach. `denote_template_member_function` now reports the
    /// template's members, and each is bound here as a method of the
    /// instantiation with a C++ shim of autocxx's own to make the call.
    ///
    /// Unlike the inherited-member shims above, the shim calls the member on the
    /// receiver itself rather than on a cast to anything: the receiver is an
    /// instantiation of the very class which declares the member - see
    /// [`instantiated_template`], which is what makes that true - so there is
    /// nothing to cast to, and nothing the class declares beside the member can
    /// resolve under its name, C++ having refused the class if it could.
    ///
    /// Only where the user wrote `instantiable!`, as for the constructors and
    /// for the same reason: autocxx cannot inspect a specialization, so what it
    /// generates for one is claimed rather than found and the C++ compiler is
    /// the only arbiter. What is claimed here is narrower than a constructor -
    /// that the instantiation has the member the template declares, which an
    /// explicit specialization may contradict - but it is the same kind of
    /// claim, so it takes the same permission.
    ///
    /// Constructors and the destructor are reported too, and are left alone
    /// here: an instantiation's special members are `instantiable!`'s own
    /// business (see `find_constructors_present`), and binding the ones the
    /// template declares would first have to decide which of them the
    /// specialization has, which is the question that directive exists to
    /// answer.
    fn add_template_instantiation_members(
        &mut self,
        apis: ApiVec<FnPrePhase1>,
    ) -> ApiVec<FnPrePhase1> {
        if self.template_member_functions.is_empty() {
            return apis;
        }
        // What to do with each member, in the order the members were declared,
        // because that is the order the overload tracker has to number them in.
        enum Member {
            Bind(ApiName, Box<FuncToConvert>, QualifiedName, String),
            Refuse(ApiName, QualifiedName, String, ConvertErrorFromCpp),
        }
        let mut members_to_add = Vec::new();
        for api in apis.iter() {
            let Api::ConcreteType {
                name,
                rs_definition,
                cpp_definition,
                holder_surface: None,
                ..
            } = api
            else {
                continue;
            };
            if !self.instantiable_concrete_types.contains(&name.name) {
                continue;
            }
            let Some(template) = instantiated_template(rs_definition.as_deref(), cpp_definition)
            else {
                continue;
            };
            let Some(members) = self.template_member_functions.get(&template) else {
                continue;
            };
            // The bindgen-style name each member is filed under. bindgen
            // numbers an overload set when it emits one and emitted none of
            // these, so every member of an overload set arrives under the one
            // name; this name only has to be an identifier and to be unique,
            // since the Rust name comes from the C++ name and the overload
            // tracker numbers that.
            let mut idents: HashSet<String> = HashSet::new();
            for member in members {
                if !matches!(member.visibility, CppVisibility::Public) {
                    continue;
                }
                if matches!(
                    member.kind,
                    CppMethodKind::Constructor
                        | CppMethodKind::Destructor
                        | CppMethodKind::VirtualDestructor { .. }
                ) {
                    continue;
                }
                let mut ident = format!("{}_{}", name.name.get_final_item(), member.name);
                let mut suffix = 1;
                while !idents.insert(ident.clone()) {
                    ident = format!("{}_{}{suffix}", name.name.get_final_item(), member.name);
                    suffix += 1;
                }
                let ident = make_ident(ident);
                let cpp_name = CppOriginalName::from_template_member_function_name(&member.name);
                let api_name = ApiName::new_with_cpp_name(
                    name.name.get_namespace(),
                    ident.clone(),
                    Some(cpp_name.clone()),
                );
                // The name the method wants in Rust before the overload tracker
                // numbers it, worked out exactly as it is for a method which is
                // analyzed: a refused member never reaches the analysis, and
                // the note standing in for it is a method of the instantiation
                // which has to be named as the method would have been.
                let rust_name = ideal_rust_name(ident.to_string(), Some(&cpp_name));
                members_to_add.push(
                    match template_member_function(&name.name, member, ident, cpp_name) {
                        Ok(fun) => Member::Bind(api_name, fun, name.name.clone(), rust_name),
                        Err(err) => Member::Refuse(api_name, name.name.clone(), rust_name, err),
                    },
                );
            }
        }

        // Every name these members will want, before any of them is numbered.
        // `reserve_ideal_names` ran over the APIs bindgen produced and none of
        // these were among them; without this the overload tracker hands a
        // numbered name to one member which another member declares for
        // itself, and the numbered one wins - so a call to `get1` would reach
        // an overload of `get` rather than the `get1` C++ declares.
        for member in &members_to_add {
            let (self_ty, rust_name) = match member {
                Member::Bind(_, _, self_ty, rust_name)
                | Member::Refuse(_, self_ty, rust_name, _) => (self_ty, rust_name),
            };
            self.overload_trackers_by_mod
                .entry(self_ty.get_namespace().clone())
                .or_default()
                .reserve(Some(self_ty.get_final_item()), rust_name);
        }

        let mut results = apis;
        for member in members_to_add {
            match member {
                Member::Bind(name, fun, _, _) => {
                    self.analyze_and_add(
                        name,
                        fun,
                        &mut results,
                        TypeConversionSophistication::Regular,
                        None,
                    );
                }
                // Numbered by the same overload tracker as the members which
                // were bound, in the same pass over the declarations, because
                // a refused member is one of the overload set: without that
                // the note for a refused `get` and the binding for a `get`
                // beside it are two `fn get` in one `impl` block. That is what
                // `analyze_foreign_fn` does for a method it refuses on an
                // ordinary class, and the numbering has to agree with it.
                //
                // The API itself is filed under the name the member was going
                // to get, as an ignored function is, because the
                // instantiation's own name is taken. The context is what puts
                // the note in the instantiation's `impl` block, and what keeps
                // it through garbage collection - see
                // `filter_apis_by_following_edges_from_allowlist`.
                Member::Refuse(name, self_ty, rust_name, err) => {
                    let rust_name = self.get_overload_name(
                        self_ty.get_namespace(),
                        self_ty.get_final_item(),
                        rust_name,
                    );
                    let ctx = self.error_context_for_method(&self_ty, &rust_name);
                    results.push(Api::IgnoredItem {
                        name,
                        err,
                        ctx: Some(ctx),
                    });
                }
            }
        }
        results
    }

    /// Leave a note in place of each member function template a class declares,
    /// none of which autocxx binds.
    ///
    /// The note is a method of the class like any other, so it takes its Rust
    /// name from the same calculation and its number from the same overload
    /// tracker as the methods which were bound: otherwise a note for a template
    /// `both` and the binding for the `both(int)` beside it are two `fn both`
    /// in one `impl` block. Runs after every method which is analyzed and after
    /// the instantiation members, so those names are taken before these ask.
    ///
    /// For an instantiation the members are the class template's, on the terms
    /// `add_template_instantiation_members` reads them: from the primary
    /// template, and only under `instantiable!`. A constructor template is left
    /// alone, as a class template's declared constructors are - a note named
    /// `new` would collide with the constructor autocxx generates.
    fn add_member_function_template_notes(
        &mut self,
        apis: ApiVec<FnPrePhase1>,
    ) -> ApiVec<FnPrePhase1> {
        if self.member_function_templates.is_empty() {
            return apis;
        }
        // The class the note hangs off, and the class whose declaration the
        // member was read from, which differ for an instantiation.
        let mut owners: Vec<(QualifiedName, QualifiedName)> = Vec::new();
        for api in apis.iter() {
            match api {
                // A class template's own `impl` block is never generated -
                // autocxx cannot `impl A` where C++ wrote `A<T>` - so a note
                // put there would have nowhere to go. Its members are reached
                // through the instantiations below.
                Api::Struct { name, .. } if !self.is_generic_type(&name.name) => {
                    owners.push((name.name.clone(), name.name.clone()));
                }
                Api::ConcreteType {
                    name,
                    rs_definition,
                    cpp_definition,
                    holder_surface: None,
                    ..
                } if self.instantiable_concrete_types.contains(&name.name) => {
                    if let Some(template) =
                        instantiated_template(rs_definition.as_deref(), cpp_definition)
                    {
                        owners.push((name.name.clone(), template));
                    }
                }
                _ => {}
            }
        }

        // Every name already spoken for, the types analysis manufactured
        // included: those are merged in after these notes, so a note which took
        // one would collide with it. `ApiVec::push` replaces a pair of
        // same-named APIs with one error, so a collision costs both the note
        // and whatever it landed on.
        let mut taken: HashSet<QualifiedName> = apis
            .iter()
            .map(|api| api.name().clone())
            .chain(self.extra_apis.iter().map(|api| api.name().clone()))
            .collect();
        let mut notes = Vec::new();
        for (self_ty, declarer) in owners {
            let Some(members) = self.member_function_templates.get(&declarer) else {
                continue;
            };
            for member in members {
                if !matches!(member.visibility, CppVisibility::Public) {
                    continue;
                }
                if matches!(member.kind, CppMethodKind::Constructor) {
                    continue;
                }
                let cpp_name = CppOriginalName::from_member_function_template_name(&member.name);
                // An operator template has no name here: what to call an
                // operator in Rust is `--represent-cxx-operators`' question,
                // decided on a path a member function template never reaches.
                let Some(rust_name) = note_rust_name(&cpp_name) else {
                    continue;
                };
                let stem = format!("{}_{}", self_ty.get_final_item(), member.name);
                let mut ident = stem.clone();
                let mut suffix = 1;
                while !taken.insert(QualifiedName::new(
                    self_ty.get_namespace(),
                    make_ident(&ident),
                )) {
                    ident = format!("{stem}{suffix}");
                    suffix += 1;
                }
                let api_name = ApiName::new_with_cpp_name(
                    self_ty.get_namespace(),
                    make_ident(ident),
                    Some(cpp_name.clone()),
                );
                notes.push((
                    api_name,
                    self_ty.clone(),
                    rust_name,
                    // Which of the two notes it gets: an instantiation's
                    // members were read from its class template, and only that
                    // was read.
                    self_ty != declarer,
                    member.template_parameters,
                ));
            }
        }

        let mut results = apis;
        for (name, self_ty, rust_name, from_class_template, template_parameters) in notes {
            let rust_name = self.get_overload_name(
                self_ty.get_namespace(),
                self_ty.get_final_item(),
                rust_name,
            );
            let ctx = self.error_context_for_method(&self_ty, &rust_name);
            results.push(Api::IgnoredItem {
                name,
                err: if from_class_template {
                    ConvertErrorFromCpp::MemberFunctionTemplateOfClassTemplate(template_parameters)
                } else {
                    ConvertErrorFromCpp::MemberFunctionTemplate(template_parameters)
                },
                ctx: Some(ctx),
            });
        }
        results
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
        let ideal_rust_name = ideal_rust_name(initial_rust_name, cpp_original_name);

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
                FnKind::Method { ref impl_for, .. }
                    if !self.is_on_allowlist(impl_for)
                        && !self.instantiable_concrete_types.contains(impl_for) =>
                {
                    // Bindgen will output methods for types which have been encountered
                    // virally as arguments on other allowlisted types. But we don't want
                    // to generate methods unless the user has specifically asked us to.
                    // It may, for instance, be a private type.
                    //
                    // An `instantiable!` concrete template instantiation is
                    // exempt because the allowlist usually cannot name it:
                    // autocxx invents the name of an instantiation it meets,
                    // and the user asks for one by naming the typedef which
                    // resolves to it. (A `concrete!` type is the exception -
                    // the user named it, and `is_on_allowlist` says so.) Nor
                    // can this let anything unasked-for through, since bindgen
                    // reports no members at all for a class template: every
                    // method such a type has is one autocxx synthesized for
                    // it. See google/autocxx#723.
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
        let ret_type_was_volatile = return_analysis.was_volatile;

        // Do we need to convert either parameters or return type?
        let param_conversion_needed = param_details.iter().any(|b| b.conversion.cpp_work_needed());
        let any_param_pointee_was_volatile = param_details
            .iter()
            .any(|b| b.conversion.pointee_was_volatile());
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
        // Every spelling the class answers to, because a nested class has two:
        // the `Outer_Inner` bindgen flattened it into and the `Outer::Inner`
        // C++ itself uses, and the designation is written by whoever wrote the
        // C++. See google/autocxx#1422 for the same two spellings in
        // `generate!`.
        //
        // Every name below is asked about, and no `||` short-circuits past
        // one, because the config records a designation as matched where it
        // answers the question. Stopping at the first name which answers would
        // leave a second designation naming this same function by another of
        // its spellings looking as though it matched nothing, and
        // `confirm_name_matching_directives_matched` would refuse it.
        let designated_by_own_name = self
            .config
            .is_on_throws_list(&diagnostic_name.to_cpp_name());
        let designated_by_class = match &kind {
            FnKind::Method { impl_for, .. } | FnKind::TraitMethod { impl_for, .. } => {
                let mut designated = false;
                for spelling in self.nested_cpp_names.spellings(impl_for) {
                    if self.config.is_on_throws_list(&format!(
                        "{spelling}::{}",
                        diagnostic_name.get_final_item()
                    )) {
                        designated = true;
                    }
                }
                designated
            }
            FnKind::Function => false,
        };
        let designated_by_base = match &fun.synthetic_cpp {
            // A member imported from a base class, which the C++ author
            // designates under the name they wrote it with - the base's. The
            // checks above see only the class it was imported into, which the
            // author never wrote at all.
            Some((CppFunctionBody::BaseClassMethodCall(base, name, _), _)) => self
                .config
                .is_on_throws_list(&format!("{base}::{}", name.to_string_for_cpp_generation())),
            _ => false,
        };
        let designated_as_throwing =
            designated_by_own_name || designated_by_class || designated_by_base;
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
            // `volatile` on a return type is part of the function's type in
            // exactly the same way, and cxx rejects `int (*f$)() = ::f;` for a
            // `volatile int f()` for exactly the same reason. The value has
            // been copied out of C++ by the time the wrapper returns it, so
            // there is nothing left for the qualifier to govern.
            _ if ret_type_was_volatile => true,
            // A `volatile` *pointee* is part of the function's type too, and
            // the bridge cannot spell it, so `void (*f$)(T*) = ::f;` is
            // rejected for a `void f(volatile T*)`. Unlike the cases above the
            // conversion which answers this is Rust-side only - the wrapper's
            // own C++ parameter is unqualified and the call adds the qualifier
            // back - so nothing else here asks for the wrapper this needs.
            _ if any_param_pointee_was_volatile => true,
            // cxx names the C++ function in the shim it generates, and a
            // deprecated function drawn from a file autocxx does not write is
            // a `-Wdeprecated-declarations` nobody can silence. Our own
            // wrapper is in a file we do write, so the pragma which silences
            // it can go around the one line which names the function - and the
            // marker moves to the Rust side, where `#[deprecated]` warns the
            // caller who actually asked for it.
            // google/autocxx#1403.
            _ if fun.deprecation.is_some() => true,
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
                // lvalue, so it never needs a ref-qualifier of its own - nor an
                // exception specification, which only an override is required
                // to repeat.
                ref_qualifier: CppRefQualifier::None,
                exception_specification: CppExceptionSpecification::None,
                is_virtual_override: false,
                calls_deprecated: fun.deprecation.is_some(),
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
            // A deprecated function needs a Rust item of its own to carry
            // `#[deprecated]`: the alternative is a `pub use` of the bridge
            // declaration, and the attribute on the declaration would warn at
            // that `use` - inside generated code, for every user, whether or
            // not anybody calls it, which is the complaint in the first place.
            // google/autocxx#1403.
            _ if fun.deprecation.is_some() => true,
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
                let bare = ideal_rust_name(initial_rust_name, name.cpp_name_if_present());
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

    /// The two halves of the `unsafe impl MakeCppStorage`, which promise that
    /// the block one hands out is aligned for `T` and that the other frees it
    /// through the allocator which produced it. Both rest entirely on the C++
    /// they call: `new_appropriately`/`delete_appropriately`, in
    /// `codegen_cpp::new_and_delete_prelude`. `moveit` turns the returned
    /// pointer straight into a `&mut MaybeUninit<T>`, so an under-aligned
    /// block is undefined behaviour on the Rust side and not only in C++.
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
                    if (matches!(annotated_type.kind, type_converter::TypeKind::Pointer)
                        || annotated_type.has_volatile_pointee)
                        && !is_placement_return_destination
                    {
                        // A `volatile` referent joins the pointer case rather
                        // than the reference one it arrived as: what the caller
                        // hands over is an address, and C++ dereferences it. Its
                        // validity is the caller's promise, exactly as for a raw
                        // pointer parameter.
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

    /// Whether C++ can copy an object of this type out of `volatile` storage by
    /// reading it - a built-in, an enumeration or a pointer. Copying a class
    /// calls a constructor, and an implicitly declared copy constructor takes
    /// `const T&` or `T&`, neither of which a `volatile T` lvalue binds to.
    fn copyable_out_of_volatile(&self, ty: &Type) -> bool {
        match ty {
            Type::Ptr(_) => true,
            Type::Path(p) => {
                let tn = QualifiedName::from_type_path(p);
                known_types().copyable_from_volatile(&tn) || self.enums.contains(&tn)
            }
            _ => false,
        }
    }

    /// The same question asked of the other language, for the pointee position,
    /// where *Rust* performs the copy: `VolatilePtr::read` is a
    /// `ptr::read_volatile`, so the Rust type has to be `Copy`, or reading
    /// would duplicate ownership of a value with a destructor.
    ///
    /// That is a narrower set than [`Self::copyable_out_of_volatile`], in two
    /// ways, and deliberately so in both. It leaves out enumerations, which C++
    /// copies by reading but whose generated Rust counterparts do not implement
    /// `Copy`. And it leaves out pointers, which are `Copy` and would read
    /// perfectly well, but whose C++ *spelling* is where this position's
    /// qualifiers go wrong: one written on a pointer binds to the declarator
    /// rather than reading left to right, so `T* const volatile` is not
    /// `const volatile T*` and cannot be spelled by putting the qualifiers in
    /// front. A register whose contents are an address is also not the idiom
    /// any of this exists for.
    fn readable_by_rust_out_of_volatile(&self, ty: &Type) -> bool {
        match ty {
            Type::Path(p) => {
                known_types().copyable_from_volatile(&QualifiedName::from_type_path(p))
            }
            _ => false,
        }
    }

    /// The policy for a parameter or return whose *pointee* C++ qualified
    /// `volatile`.
    ///
    /// Rust performs the access through whatever it is handed, so it is handed
    /// an `autocxx::VolatilePtr<T>`, whose `read` and `write` are the volatile
    /// access. Not a `*mut T`, which is read with an ordinary load; and
    /// emphatically not the `&T` a C++ reference would otherwise become, since
    /// a Rust shared reference promises the referent does not change while it
    /// lives, and a register changes when nothing in the program touched it.
    ///
    /// The bridge carries the bare pointer either way. What differs by
    /// direction is the wrapper's own C++: on the way in the qualifier is added
    /// back implicitly by the call, and on the way out it has to be cast off,
    /// there being no way to spell it on the bridge.
    ///
    /// `None` where the pointee is not one Rust reads - a class, an
    /// enumeration, a pointer. The handle exists for the position where Rust
    /// performs the access, and where it does not, autocxx's existing treatment
    /// of the pointer or reference is already right: it passes an address C++
    /// dereferences, which is what a `volatile` object of class type wants.
    /// Answering `None` rather than refusing is what keeps a copy constructor
    /// taking `const volatile T&` bindable.
    fn volatile_pointee_policy(
        &self,
        converted: &Type,
        is_return: bool,
    ) -> Option<TypeConversionPolicy> {
        let (pointee, is_mut, was_reference) = match converted {
            Type::Ptr(p) => ((*p.elem).clone(), p.mutability.is_some(), false),
            Type::Reference(r) => ((*r.elem).clone(), r.mutability.is_some(), true),
            Type::Path(p) => (
                extract_pinned_mutable_reference_type(p)?.clone(),
                true,
                true,
            ),
            _ => return None,
        };
        if !self.readable_by_rust_out_of_volatile(&pointee) {
            return None;
        }
        let cpp = match (is_return, was_reference) {
            (true, true) => PointerCppConversion::FromVolatileReferenceToPointer,
            (true, false) => PointerCppConversion::FromVolatilePointerToPointer,
            // The wrapper's parameter is unqualified, and the call has to put
            // the qualifier back explicitly rather than let the conversion do
            // it: implicit qualification happens after overload resolution, so
            // a name with both a `volatile`-pointee overload and a plain one
            // would resolve to the plain one and the binding for the other
            // would silently call it.
            (false, true) => PointerCppConversion::FromPointerToVolatileReference,
            (false, false) => PointerCppConversion::FromPointerToVolatilePointer,
        };
        let rust = if is_return {
            PointerRustConversion::FromPointerToVolatilePtr
        } else {
            PointerRustConversion::FromVolatilePtrToPointer
        };
        Some(TypeConversionPolicy::pointer(
            BridgePointer::to(pointee, is_mut),
            cpp,
            rust,
        ))
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
        // Nothing below decays an array, and for the shapes
        // `check_signature_array` turns down cxx would not write the C++ this
        // parameter was declared with. Said here rather than left to the
        // catch-all at the end of this match, which hands whatever it is to
        // cxx unconverted - which is where a `std::array` parameter goes, cxx
        // spelling it back as the `std::array` it came from.
        self.check_signature_array(ty)?;
        // A `std::string_view` parameter, in any of the spellings C++ has for
        // one. The view is built in the C++ wrapper over bytes Rust lends for
        // the call - see `WholeCppConversion::FromRustBytesToStringView` - so
        // this is decided here, ahead of the reference handling below, which
        // would otherwise hand out a `&`/`CppRef` to a type Rust has no way to
        // make one of.
        //
        // A `const&` binds to that temporary for the duration of the call,
        // which is as long as the view's own characters are guaranteed to be
        // there anyway. A *mutable* reference is an out-parameter: C++ would
        // write a view of its own into the slot, over storage whose lifetime
        // nothing has checked, and there is nothing for Rust to receive it
        // into.
        //
        // Every other shape is refused, and refusing is not optional here the
        // way it is for the `rust::Str*` this resembles. `rust::Str` is `&str`,
        // a type Rust has, so a pointer to one is a pointer to something;
        // `std::string_view` has no Rust spelling at all, and a parameter
        // which kept one would put a name nothing defines into the bridge, for
        // cxx to report as a bug in autocxx.
        if let Some(sv) = string_view_parameter(ty) {
            return match sv {
                StringViewParameter::Buildable(ty) => {
                    if self.config.exclude_utilities() {
                        Err(ConvertErrorFromCpp::StringViewWithoutUtilities)
                    } else {
                        Ok(TypeConversionPolicy::whole(
                            ty.clone(),
                            WholeCppConversion::FromRustBytesToStringView,
                            WholeRustConversion::FromBytes,
                        ))
                    }
                }
                StringViewParameter::Mutable => Err(ConvertErrorFromCpp::MutableStringViewRef),
                StringViewParameter::Indirect => Err(ConvertErrorFromCpp::IndirectStringView),
            };
        }
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
        // Before any of the branches below, which would otherwise make this an
        // ordinary pointer or reference and lose the qualifier with no trace.
        // The type converter recorded this, since it is the last place the
        // marker exists.
        if annotated_type.has_volatile_pointee {
            if let Some(policy) = self.volatile_pointee_policy(ty, false) {
                // An rvalue reference is turned down rather than handled.
                // Conversion has already made it the same pointer a `T&`
                // becomes, so the handle could not say which it was, and what
                // distinguishes one - that it may be moved out of - is not
                // something `volatile` storage offers.
                if is_rvalue_ref {
                    return Err(ConvertErrorFromCpp::VolatileRValueReference);
                }
                return Ok(policy);
            }
            // Otherwise this is a pointee Rust never reads, and the branches
            // below already treat it correctly.
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
                // Asked of the type as bindgen wrote it, since conversion peels
                // the marker off. Looked for underneath the `const` marker too,
                // because a `const volatile` return carries both. Only the top
                // level: a qualifier deeper in - on a pointee - is a position
                // autocxx does not handle at all, and a wrapper would not
                // rescue it, since the pointer type itself would still differ.
                let was_volatile = is_volatile_qualified(boxed_type);
                // A qualifier one level in, on what is pointed or referred to,
                // is the position Rust would perform the access through. Asked
                // here for the same reason as the one above: conversion peels
                // the marker and no Rust pointer type carries it on.
                // Asked of the raw type, because conversion makes an rvalue
                // reference the same pointer a `T&` becomes and nothing
                // downstream could tell them apart.
                let was_rvalue_reference_spelling = type_is_reference(boxed_type, true);
                let annotated_type = self.convert_boxed_type(boxed_type.clone(), ns)?;
                let pointee_was_volatile = annotated_type.has_volatile_pointee;
                let pointee_was_volatile_rvalue_reference =
                    pointee_was_volatile && was_rvalue_reference_spelling;
                let was_const = annotated_type.is_const;
                let boxed_type = annotated_type.ty;
                let ty: &Type = boxed_type.as_ref();
                // The wrapper `was_volatile` asks for has `return f();` for a
                // body, copy-initializing an unqualified `T` from a `volatile
                // T`. For a scalar - a built-in, an enumeration or a pointer -
                // that is a read. For a class it needs a constructor taking
                // `volatile T&` or `const volatile T&`, which C++ does not
                // implicitly declare, so the wrapper does not compile at the
                // C++14 autocxx generates for. C++17 initializes the result
                // directly and would accept it; the floor is what is built
                // against, so the class case is turned down rather than made
                // to depend on the standard in use.
                if was_volatile && !self.copyable_out_of_volatile(ty) {
                    return Err(ConvertErrorFromCpp::VolatileReturn(
                        diagnostic_name.to_cpp_name(),
                    ));
                }
                // As for a parameter.
                self.check_signature_array(ty)?;
                // No return position works: Rust has no type which is a view,
                // so there is nothing for one to arrive as. Asked of the
                // converted type as well as of the names met on the way to it,
                // because the two miss different things: a typedef to a
                // reference resolves to a type mentioning the view while
                // recording only the alias among its dependencies, and a
                // container records the payload without the converted type
                // naming it.
                //
                // Ahead of the `volatile` pointee branch below, which answers
                // with a conversion rather than a refusal: a view is turned
                // down whatever else is true of the return.
                if mentions_string_view(ty)
                    || annotated_type
                        .types_encountered
                        .iter()
                        .any(|tn| known_types().is_string_view(tn))
                {
                    return Err(ConvertErrorFromCpp::StringViewOutOfCpp);
                }
                // C++ handed back the address of storage it qualified
                // `volatile`, so Rust is the side which will perform the
                // accesses. Answered before the branches below, which would
                // make this an ordinary pointer or reference.
                let volatile_pointee_conversion = if pointee_was_volatile {
                    self.volatile_pointee_policy(ty, true)
                } else {
                    None
                };
                if volatile_pointee_conversion.is_some() && pointee_was_volatile_rvalue_reference {
                    return Err(ConvertErrorFromCpp::VolatileRValueReference);
                }
                if let Some(conversion) = volatile_pointee_conversion {
                    return Ok(ReturnTypeAnalysis {
                        rt: ReturnType::Type(*rarrow, boxed_type.clone()),
                        conversion: Some(conversion),
                        was_reference: false,
                        was_mutable_reference: false,
                        was_rvalue_reference: false,
                        was_const,
                        was_volatile,
                        deps: annotated_type.types_encountered,
                        placement_param_needed: None,
                    });
                }
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
                                was_volatile,
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
                                was_volatile,
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
                            was_volatile,
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
        let all_items_found = find_constructors_present(&apis, &self.instantiable_concrete_types);
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
            // Asked before `exclude_impls` bows out, so that a
            // `block_constructors!` naming this class counts as matched
            // whichever of the two reasons stops the synthesis below. It is
            // redundant beside `exclude_impls!`, not wrong, and the class it
            // names is right here.
            let constructors_blocked = self
                .config
                .is_on_constructor_blocklist(&self_ty.to_cpp_name());
            if self.config.exclude_impls {
                // Only the synthesis below is skipped. The analysis above runs
                // either way, because withdrawing the special members C++
                // deletes is right whether or not we go on to add any.
                continue;
            }
            if constructors_blocked {
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
                let undestroyable_member = self.type_converter.undestroyable_member_of(&name.name);
                Ok(Box::new(std::iter::once(Api::Struct {
                    name,
                    details,
                    analysis: PodAndConstructorAnalysis {
                        pod: analysis,
                        constructors: if let Some(items_found) = items_found {
                            PublicConstructors::from_items_found(
                                items_found,
                                destructor_omitted_as_trivial,
                                undestroyable_member,
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
                        deprecation: None,
                        add_to_trait: None,
                        synthetic_cpp: None,
                        provenance: Provenance::SynthesizedOther,
                        variadic: false,
                        ref_qualifier: CppRefQualifier::None,
                        exception_specification: ExceptionSpecification::None,
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

/// Whether exactly one path runs from `derived` up to `ancestor` through the
/// bases `inheritance` allows.
///
/// A base a member is reached through need not be a direct one, so a direct
/// base list does not answer whether `ancestor` is a base at all; and one path
/// is what says the member can be called through `derived`, because two paths
/// are two base subobjects and C++ rejects the conversion between them.
///
/// `false` where a class on the way has a base bindgen could not name - a
/// template instantiation, which it announces through no callback. Such a base
/// may lead to `ancestor` too, so a path count taken without it would be an
/// undercount rather than an answer.
fn reached_exactly_once(
    ancestry: &HashMap<QualifiedName, Ancestry>,
    derived: &QualifiedName,
    ancestor: &QualifiedName,
    inheritance: Inheritance,
) -> bool {
    /// More than one path, or a path which cannot be counted, are the same
    /// answer here, so both stop the walk.
    enum Paths {
        None,
        One,
        Unusable,
    }

    // C++ forbids an inheritance cycle, but this graph is what bindgen
    // reported rather than the compiler's own, so bound the walk by the number
    // of classes there are: no path can be longer than that.
    fn walk(
        ancestry: &HashMap<QualifiedName, Ancestry>,
        derived: &QualifiedName,
        ancestor: &QualifiedName,
        inheritance: Inheritance,
        depth: usize,
    ) -> Paths {
        let Some(here) = ancestry.get(derived).filter(|_| depth > 0) else {
            return Paths::None;
        };
        if here.has_unnamed_base {
            return Paths::Unusable;
        }
        let mut found = Paths::None;
        for base in here.bases(inheritance) {
            let through_here = if base == ancestor {
                Paths::One
            } else {
                walk(ancestry, base, ancestor, inheritance, depth - 1)
            };
            found = match (found, through_here) {
                (Paths::Unusable, _) | (_, Paths::Unusable) => return Paths::Unusable,
                (Paths::None, other) | (other, Paths::None) => other,
                (Paths::One, Paths::One) => return Paths::Unusable,
            };
        }
        found
    }
    matches!(
        walk(ancestry, derived, ancestor, inheritance, ancestry.len()),
        Paths::One
    )
}

/// What C++ member name lookup finds for one name in one class.
enum MemberLookup {
    /// Neither the class nor any base declares the name.
    Nothing,
    /// Exactly one class does, however many paths reach it.
    Found(QualifiedName),
    /// More than one class does, so C++ would call the use ambiguous - or the
    /// ancestry is not known well enough to say which.
    Ambiguous,
}

/// Look a member name up in `class` the way C++ does: the class's own
/// declarations hide anything a base declares, and two bases declaring it is
/// ambiguous rather than a choice.
///
/// A declaration is anything of that name, not merely a function of it: a data
/// member, a nested type, a nested enum and a static data member all hide an
/// inherited function, and calling one is an error rather than a call. So
/// `declared` is consulted for what the class reported directly, and
/// `flat_scope` for the rest, a member of class `X` being an item bindgen put
/// in the enclosing mod as `X_member`.
///
/// Access is not consulted, because C++ does not consult it either until after
/// the name has been found - a private member of a base still hides a public
/// member of that base's own base.
///
/// The one rule left out is domination through a virtual base, which would
/// turn some of these `Ambiguous` answers into a class. It says nothing wrong,
/// only less: bindgen reports which bases are virtual for the class declaring
/// them alone, so the case cannot be recognised, and an inherited member left
/// unbound is what happened before any of this existed.
fn look_up_member(
    ancestry: &HashMap<QualifiedName, Ancestry>,
    declared: &HashMap<QualifiedName, HashSet<String>>,
    flat_scope: &HashSet<String>,
    class: &QualifiedName,
    name: &str,
) -> MemberLookup {
    // C++ forbids an inheritance cycle, but this graph is what bindgen
    // reported rather than the compiler's own, so bound the walk by the number
    // of classes there are: no path can be longer than that.
    fn walk(
        ancestry: &HashMap<QualifiedName, Ancestry>,
        declared: &HashMap<QualifiedName, HashSet<String>>,
        flat_scope: &HashSet<String>,
        class: &QualifiedName,
        name: &str,
        depth: usize,
    ) -> MemberLookup {
        if depth == 0 {
            return MemberLookup::Ambiguous;
        }
        if declared
            .get(class)
            .is_some_and(|declared| declared.contains(name))
            || flat_scope.contains(&format!("{class}_{name}"))
        {
            return MemberLookup::Found(class.clone());
        }
        let Some(here) = ancestry.get(class) else {
            return MemberLookup::Nothing;
        };
        if here.has_unnamed_base {
            return MemberLookup::Ambiguous;
        }
        let mut found = MemberLookup::Nothing;
        for base in here.bases.iter() {
            let through_here = walk(ancestry, declared, flat_scope, base, name, depth - 1);
            found = match (found, through_here) {
                (MemberLookup::Ambiguous, _) | (_, MemberLookup::Ambiguous) => {
                    return MemberLookup::Ambiguous
                }
                (MemberLookup::Nothing, other) | (other, MemberLookup::Nothing) => other,
                (MemberLookup::Found(one), MemberLookup::Found(other)) if one == other => {
                    MemberLookup::Found(one)
                }
                (MemberLookup::Found(_), MemberLookup::Found(_)) => return MemberLookup::Ambiguous,
            };
        }
        found
    }
    walk(
        ancestry,
        declared,
        flat_scope,
        class,
        name,
        ancestry.len() + 1,
    )
}

/// The class template a concrete type instantiates, where it instantiates one
/// outright.
///
/// Two routes arrive at a concrete type and they carry different things. One
/// autocxx met in a signature or a typedef has bindgen's own rendering of it,
/// `root::A<u32>`, whose leading path is the template. One the user named in a
/// `concrete!` directive has only the C++ expression they wrote, and that
/// expression need not be an instantiation at all: `Outer<int>::Inner` and
/// `Outer<int>::Inner<float>` both name a type *inside* one, whose members are
/// not `Outer`'s and would not compile called on it. So such an expression
/// counts only where the `>` closing the first `<` is the end of it -
/// `A<uint32_t>` - and anything else answers `None`.
pub(crate) fn instantiated_template(
    rs_definition: Option<&crate::minisyn::Type>,
    cpp_definition: &str,
) -> Option<QualifiedName> {
    if let Some(Type::Path(typ)) = rs_definition.map(|ty| &ty.0) {
        return Some(QualifiedName::from_type_path(typ));
    }
    let (template, arguments) = cpp_definition.split_once('<')?;
    // Counted rather than matched on the last character, because an expression
    // may have several argument lists and only the first one's belongs to the
    // name in front of it. An expression this cannot make sense of - a `>`
    // inside a non-type argument, say - runs off the end and answers `None`,
    // which costs the members rather than attaching them to the wrong type.
    let mut depth = 1usize;
    for (offset, character) in arguments.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return arguments[offset + 1..]
                        .trim()
                        .is_empty()
                        .then(|| QualifiedName::new_from_cpp_name(template.trim()));
                }
            }
            _ => {}
        }
    }
    None
}

/// One member function of a class template, as a method of a concrete
/// instantiation of that template.
///
/// The signature is bindgen's own rendering of the member's, in the shape
/// bindgen writes a method it can generate: a leading `this` pointer for a
/// non-static member, whose constness is the member's, and then the parameters.
/// Only the receiver is autocxx's to supply - bindgen reports the rest - because
/// the receiver is the one part of the signature which names the class template
/// and so names its parameters.
fn template_member_function(
    self_ty: &QualifiedName,
    member: &TemplateMemberFunction,
    ident: crate::minisyn::Ident,
    cpp_name: CppOriginalName,
) -> Result<Box<FuncToConvert>, ConvertErrorFromCpp> {
    let signature = member
        .signature
        .as_ref()
        .ok_or(ConvertErrorFromCpp::TemplateMemberWithDependentSignature)?;
    let is_static = matches!(member.kind, CppMethodKind::Static);
    let mut inputs: Punctuated<syn::FnArg, Comma> = Punctuated::new();
    if !is_static {
        let path = self_ty.to_type_path();
        inputs.push(if member.is_const {
            parse_quote! { this: *const #path }
        } else {
            parse_quote! { this: *mut #path }
        });
    }
    for argument in &signature.arguments {
        inputs.push(
            syn::parse_str::<syn::FnArg>(argument).map_err(|_| {
                ConvertErrorFromCpp::TemplateMemberSignatureNotRust(argument.clone())
            })?,
        );
    }
    let output = match &signature.return_type {
        None => ReturnType::Default,
        Some(text) => {
            let ty = syn::parse_str::<Type>(text)
                .map_err(|_| ConvertErrorFromCpp::TemplateMemberSignatureNotRust(text.clone()))?;
            parse_quote! { -> #ty }
        }
    };
    Ok(Box::new(FuncToConvert {
        provenance: Provenance::SynthesizedOther,
        ident,
        doc_attrs: Vec::new(),
        inputs: minisynize_punctuated(inputs),
        variadic: signature.is_variadic,
        output: output.into(),
        vis: parse_quote! { pub },
        virtualness: match member.kind {
            CppMethodKind::Virtual { pure_virtual: true } => Some(Virtualness::PureVirtual),
            CppMethodKind::Virtual {
                pure_virtual: false,
            } => Some(Virtualness::Virtual),
            _ => None,
        },
        cpp_vis: CppVisibility::Public,
        special_member: member.special_member,
        method_kind: Some(member.kind),
        original_name: Some(cpp_name.clone()),
        // Set for every member, not only the static ones: for the rest the
        // `this` parameter above answers the same question, and
        // `analyze_foreign_fn` prefers it.
        self_ty: Some(self_ty.clone()),
        synthesized_this_type: None,
        add_to_trait: None,
        // The receiver is the class which declares the member, so the shim
        // calls it by name on the receiver. A static member is named through
        // the class instead, which is what the `StaticMethodCall` body writes.
        synthetic_cpp: Some(if is_static {
            (
                CppFunctionBody::StaticMethodCall(
                    self_ty.get_namespace().clone(),
                    self_ty.get_final_ident(),
                    cpp_name.to_effective_name(),
                ),
                CppFunctionKind::Function,
            )
        } else {
            (
                CppFunctionBody::FunctionCall(Namespace::new(), cpp_name.to_effective_name()),
                CppFunctionKind::Method,
            )
        }),
        is_deleted: member.explicitness,
        deprecation: member.deprecation.clone(),
        // Reported rather than recovered from the `#[link_name]` mangling, as
        // it is for a member bindgen generates: there is no function for one of
        // these to carry a mangled name at all. It has to come from somewhere,
        // because a class may declare both qualifications of one name and the
        // shim would call whichever the lvalue it holds selects.
        // bindgen reports a class template's members through a callback of its
        // own, which carries no exception specification. Nothing reads this
        // today: a member bound from that callback reaches `analyze_and_add`,
        // which generates no subclass override.
        exception_specification: ExceptionSpecification::None,
        ref_qualifier: match member.ref_qualifier {
            RefQualifier::None => CppRefQualifier::None,
            RefQualifier::LValue => CppRefQualifier::LValue,
            RefQualifier::RValue => CppRefQualifier::RValue,
        },
    }))
}

/// The base class member `fun`, as a function reached through a derived class
/// which inherits it, or which named it in a `using Base::foo;`.
///
/// The receiver becomes the derived class and the call is forced through a C++
/// shim of our own. It has to be: the member belongs to the base, so cxx would
/// declare it by taking the address of `&Derived::foo`, whose type in C++ is
/// pointer-to-member-of-*Base* and does not match - and the base may be
/// private, which makes even naming it from outside an error. Letting C++
/// make the call is also what gets the `this` adjustment right for a base
/// which does not sit at offset zero within the derived class.
///
/// `through_base` names the base to make the call through, for a caller which
/// wants the member settled rather than looked up on the receiver: see
/// [`CppFunctionBody::BaseClassMethodCall`]. A `using Base::foo;` passes
/// `None`, since the point of one may be a base, or a member, which the shim
/// is not allowed to name.
fn import_member_into(
    importer: &QualifiedName,
    visibility: CppVisibility,
    base_method: &ApiName,
    fun: &FuncToConvert,
    through_base: Option<(&str, &ReceiverMutability)>,
) -> (ApiName, Box<FuncToConvert>) {
    // The importer's name in front of the base method's whole bindgen name,
    // which already names the base: this only has to be unique and to be an
    // identifier, because the Rust name comes from the C++ name below.
    let name = ApiName::new_with_cpp_name(
        importer.get_namespace(),
        make_ident(format!(
            "{}_{}",
            importer.get_final_item(),
            base_method.name.get_final_item()
        )),
        base_method.cpp_name_if_present().cloned(),
    );
    let mut fun = fun.clone();
    fun.provenance = Provenance::SynthesizedOther;
    // The access the declaration gives it, not the access it has on the base:
    // widening a `protected` member is one of the things a using-declaration
    // is for, and the member is only reachable at all because of this one.
    fun.cpp_vis = visibility;
    fun.self_ty = Some(importer.clone());
    fun.synthesized_this_type = Some(importer.clone());
    // The importer declares no method of its own, so this overrides nothing
    // and is no special member of the importer either.
    fun.virtualness = None;
    fun.special_member = None;
    fun.synthetic_cpp = Some((
        match through_base {
            Some((base, receiver_mutability)) => CppFunctionBody::BaseClassMethodCall(
                base.to_string(),
                base_method.cpp_name(),
                *receiver_mutability,
            ),
            None => CppFunctionBody::FunctionCall(Namespace::new(), base_method.cpp_name()),
        },
        CppFunctionKind::Method,
    ));
    (name, Box::new(fun))
}

/// The name a function would like in Rust, before the overload tracker numbers
/// it: the C++ name, unless the identifier bindgen chose ends in an underscore.
///
/// bindgen may have mangled the name either because it is not valid Rust syntax
/// (a keyword like `async`, which it makes `async_`) or because it is an
/// overload (which it numbers). The former is respected and the latter is not,
/// since overloads are numbered here instead - and the two are told apart by
/// the trailing underscore, which therefore also keeps the mangled name for a
/// C++ function called `foo_` in the first place. Cases:
/// ```text
///   function, IRN=foo,    CN=<none>                    output: foo    case 1
///   function, IRN=move_,  CN=move   (keyword problem)  output: move_  case 2
///   function, IRN=foo1,   CN=foo    (overload)         output: foo    case 3
///   method,   IRN=A_foo,  CN=foo                       output: foo    case 4
///   method,   IRN=A_move, CN=move   (keyword problem)  output: move_  case 5
///   method,   IRN=A_foo1, CN=foo    (overload)         output: foo    case 6
/// ```
///
/// Shared by the three places which have to agree about it: the pass which
/// reserves the names real functions will want, the analysis which assigns
/// them, and the synthesis of a class template's members, whose refused members
/// get a name from here without being analyzed at all.
fn ideal_rust_name(
    initial_rust_name: String,
    cpp_original_name: Option<&CppOriginalName>,
) -> String {
    match cpp_original_name {
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
    }
}

/// The Rust name a note left in place of a C++ member would take, or `None`
/// where there is none to take.
///
/// [`ideal_rust_name`] cannot be asked directly, because it decides by building
/// the identifier and `Ident::new` panics rather than refusing on a name C++
/// spells with something which is not an identifier - `operator()`. So the
/// spelling is checked before it and its answer after: `_` lexes as an
/// identifier and is not one Rust lets anything be called, and an item under
/// that name would be dropped later without a word.
fn note_rust_name(cpp_name: &CppOriginalName) -> Option<String> {
    let spelling = cpp_name.for_validation();
    if syn::parse_str::<syn::Ident>(spelling).is_err()
        && syn::parse_str::<syn::Ident>(&format!("{spelling}_")).is_err()
    {
        return None;
    }
    let name = ideal_rust_name(spelling.to_string(), Some(cpp_name));
    syn::parse_str::<syn::Ident>(&name).is_ok().then_some(name)
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
/// is that guess being wrong. A synthesized function reaches it only where its
/// name is one autocxx chose: a synthesized constructor is named for its type
/// on purpose (see [`CppOriginalName::from_type_name_for_constructor`]), and
/// every other such function is a wrapper autocxx also named. A field accessor
/// takes its name from C++, where a data member may share its class's name, so
/// `analysis::field_accessors` states the kind rather than leaving it to be
/// guessed at.
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

/// What a `std::string_view` parameter turned out to be.
enum StringViewParameter<'a> {
    /// A view the C++ wrapper can build: one taken by value, or by `const&`,
    /// which binds to the wrapper's temporary. Carries the `string_view` type
    /// itself, which is what the wrapper's call has to be spelt in terms of.
    Buildable(&'a Type),
    /// A mutable `std::string_view&`, which is an out-parameter.
    Mutable,
    /// A `std::string_view*`, or a `std::string_view&&`, which reaches here as
    /// the same pointer. Either way it is a handle to a view which must
    /// already exist, and Rust has none to point at.
    Indirect,
}

/// Whether `ty` - a type the converter has finished with, so aliases are
/// already resolved - names `std::string_view` anywhere a generated signature
/// would have to spell it.
fn mentions_string_view(ty: &Type) -> bool {
    match ty {
        Type::Path(p) => {
            known_types().is_string_view(&QualifiedName::from_type_path(p))
                || matches!(p.path.segments.last().map(|seg| &seg.arguments),
                    Some(syn::PathArguments::AngleBracketed(args))
                        if args.args.iter().any(|arg| matches!(arg,
                            syn::GenericArgument::Type(inner) if mentions_string_view(inner))))
        }
        Type::Ptr(p) => mentions_string_view(&p.elem),
        Type::Reference(r) => mentions_string_view(&r.elem),
        _ => false,
    }
}

/// Whether `ty` - a parameter as the type converter left it - is a
/// `std::string_view`, and in which of the shapes autocxx does something
/// about. `None` for everything else.
fn string_view_parameter(ty: &Type) -> Option<StringViewParameter<'_>> {
    let is_string_view = |ty: &Type| {
        matches!(ty, Type::Path(p)
            if known_types().is_string_view(&QualifiedName::from_type_path(p)))
    };
    match ty {
        Type::Path(p) => {
            // `Pin<&mut std::string_view>` is what a mutable reference has
            // become by now; anything else which is a path is the view itself.
            match extract_pinned_mutable_reference_type(p) {
                Some(inner) => is_string_view(inner).then_some(StringViewParameter::Mutable),
                None => is_string_view(ty).then_some(StringViewParameter::Buildable(ty)),
            }
        }
        Type::Reference(r) if is_string_view(&r.elem) => Some(match r.mutability {
            Some(_) => StringViewParameter::Mutable,
            None => StringViewParameter::Buildable(&r.elem),
        }),
        // A `std::string_view*`, and a `std::string_view&&`, which the type
        // converter has made the same pointer of by now.
        Type::Ptr(p) if is_string_view(&p.elem) => Some(StringViewParameter::Indirect),
        _ => None,
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

impl FnAnalyzer<'_> {
    /// Turn down the arrays a signature cannot carry, and let a `std::array`
    /// through.
    ///
    /// Two different refusals, neither of which is about the array standing on
    /// its own: [`denotes_cpp_array_behind_pointer`] is the array autocxx will
    /// not put a pointer to, and [`Self::permissible_array_element`] is the
    /// element which cannot cross by value. A reference is decided before
    /// this, while the marker saying which of the two C++ array types it is
    /// can still be read; see `TypeConverter::check_array_referent`.
    fn check_signature_array(&self, ty: &Type) -> Result<(), ConvertErrorFromCpp> {
        if denotes_cpp_array_behind_pointer(ty) {
            return Err(ConvertErrorFromCpp::CppArrayInSignature(
                ty.to_token_stream().to_string(),
            ));
        }
        if let Some(element) = cpp_array_element(ty) {
            let permissible = matches!(element, Type::Path(path)
                if self.permissible_array_element(&QualifiedName::from_type_path(path)));
            if !permissible {
                return Err(ConvertErrorFromCpp::CppArrayElementNotSupported(
                    ty.to_token_stream().to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Whether `name` may be the element of a `std::array` which crosses the
    /// bridge.
    ///
    /// One of cxx's atoms, which needs nothing further, or a type the bridge
    /// declares under a name of its own which autocxx has already proved
    /// trivially relocatable: the `c_*` newtypes, and any class or enum the
    /// POD analysis passed. Those need the certificate cxx will not write for
    /// an array element, which `array_element_witnesses` supplies.
    ///
    /// The proof is the one `generate_pod!` already rests on, and it is
    /// checked in C++ rather than taken on trust. A POD struct is asserted
    /// `IsRelocatable` by autocxx itself - `generate_pod_assertion` - and an
    /// enum or an `extern_cpp_type!` marked POD is not, so for those the
    /// certificate is what introduces the check: cxx asserts `IsRelocatable`
    /// for every type one names. A class whose destructor does something fails
    /// that assertion and the build stops, unless the C++ has opted into
    /// relocatability by hand with `using IsRelocatable = std::true_type`,
    /// which cxx documents and which is a claim its author has made rather
    /// than one autocxx invented.
    fn permissible_array_element(&self, name: &QualifiedName) -> bool {
        known_types().permissible_within_array(name)
            || known_types().relocatable_newtype(name)
            || (!known_types().is_known_type(name) && self.pod_safe_types.contains(name))
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
