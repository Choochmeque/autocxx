// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::{
    conversion::{
        api::{
            AnalysisPhase, Api, ApiName, HolderSurface, NullPhase, OpaqueTypedefReason,
            TypedefKind, UnanalyzedApi,
        },
        apivec::ApiVec,
        codegen_cpp::type_to_cpp::CppNameMap,
        inner_type_traits::inner_types_required_of_params,
        type_helpers::{
            extract_pinned_mutable_reference_type, is_volatile_qualified, mentions_cpp_array,
            mentions_float128, mentions_long_double, mentions_volatile,
            unqualified_array_element_type, unwrap_bitfield, unwrap_const, unwrap_float128,
            unwrap_function_pointer, unwrap_has_opaque, unwrap_long_double, unwrap_reference,
            unwrap_std_array, unwrap_volatile,
        },
        ConvertErrorFromCpp,
    },
    known_types::{known_types, CxxGenericType},
    parse_callbacks::ParseCallbackResults,
    types::{make_ident, Namespace, QualifiedName},
    vendored_bindgen::callbacks::SpecialMemberKind,
};
use autocxx_parser::IncludeCppConfig;
use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;
use itertools::Itertools;
use proc_macro2::Ident;
use quote::ToTokens;
use syn::{
    parse_quote, punctuated::Punctuated, token::Comma, GenericArgument, PathArguments, PathSegment,
    Type, TypePath, TypePtr,
};

use super::tdef::TypedefAnalysis;

/// Certain kinds of type may require special handling by callers.
#[derive(Debug, Clone)]
pub(crate) enum TypeKind {
    Regular,
    Pointer,
    SubclassHolder(crate::minisyn::Ident),
    Reference,
    RValueReference,
    MutableReference,
}

/// What a typedef was analysed to point at: the type its target was converted
/// to, and what kind of thing that turned out to be.
///
/// The kind is kept because it cannot always be read back off the type. A C++
/// rvalue reference and a C++ pointer both convert to a Rust pointer, so
/// `typedef T&& R` and `typedef T* P` are indistinguishable by the time
/// anything uses the alias; only the analysis which unwrapped bindgen's
/// reference marker knows which of the two it was, and a parameter of the
/// first kind has to be passed as a value to move from rather than as a
/// pointer. See google/autocxx#1363.
#[derive(Debug, Clone)]
pub(crate) struct TypedefTargetInfo {
    ty: Type,
    kind: TypeKind,
    /// Whether C++ qualified the target `const` in its own right. Same reason
    /// it is kept on the analysis: the converted type cannot say it.
    is_const: bool,
    /// Whether the target is the array a `std::array<T, N>` was lowered to.
    /// Same reason again: the converted type is a bare `[T; N]`, which is what
    /// a C array is written as too.
    is_std_array: bool,
}

/// What a typedef's target was, beyond what its converted type can say.
///
/// Both facts are lost by converting - Rust cannot spell a top-level `const`,
/// and `[T; N]` is written for a `std::array<T, N>` and a C array alike - so
/// they are read from the typedef's own analysis and applied to each use of
/// the alias. See [`TypedefAnalysis::target_is_const`].
#[derive(Clone, Copy, Default)]
pub(crate) struct TargetFacts {
    is_const: bool,
    is_std_array: bool,
}

/// Results of some type conversion, annotated with a list of every type encountered,
/// and optionally any extra APIs we need in order to use this type.
#[derive(Debug)]
pub(crate) struct Annotated<T> {
    pub(crate) ty: T,
    pub(crate) types_encountered: HashSet<QualifiedName>,
    pub(crate) extra_apis: ApiVec<NullPhase>,
    pub(crate) kind: TypeKind,
    /// Whether C++ qualified this type `const` in its own right - `const int
    /// m`, `T* const p`, `const int f()`. Kept beside the type because Rust
    /// has no way to spell it: `ty` is what the qualifier was applied to.
    /// Constness of a *pointee* is not this; that is in `ty` already, as
    /// `*const T`.
    pub(crate) is_const: bool,
    /// Whether C++ qualified what this points or refers to `volatile` -
    /// `volatile T*`, `volatile T&`. Kept beside the type for the same reason
    /// `is_const` is, and more so: `*const T` carries a pointee's constness,
    /// and there is no Rust pointer type which carries the other qualifier, so
    /// conversion is where the fact would otherwise be lost for good.
    pub(crate) has_volatile_pointee: bool,
    /// Whether this type is the array a C++ `std::array<T, N>` was lowered to.
    /// Kept beside the type for the reason [`Self::is_const`] is: `ty` is the
    /// bare `[T; N]`, and bindgen writes the C array `T[N]` as that too, so the
    /// type no longer says which of the two C++ wrote. What reads it is
    /// [`TypeConverter::convert_lvalue_reference`], a reference being where the
    /// two are different C++ types.
    pub(crate) is_std_array: bool,
}

impl<T> Annotated<T> {
    fn new(
        ty: T,
        types_encountered: HashSet<QualifiedName>,
        extra_apis: ApiVec<NullPhase>,
        kind: TypeKind,
    ) -> Self {
        Self {
            ty,
            types_encountered,
            extra_apis,
            kind,
            is_const: false,
            has_volatile_pointee: false,
            is_std_array: false,
        }
    }

    /// Records that C++ qualified this type's pointee `volatile`. See
    /// [`Self::has_volatile_pointee`].
    fn marked_volatile_pointee_if(mut self, has_volatile_pointee: bool) -> Self {
        self.has_volatile_pointee = has_volatile_pointee;
        self
    }

    /// Records that C++ qualified this `const`. See [`Self::is_const`].
    fn marked_const(mut self) -> Self {
        self.is_const = true;
        self
    }

    /// [`Self::marked_const`], for a constness which is only sometimes there.
    fn marked_const_if(self, is_const: bool) -> Self {
        if is_const {
            self.marked_const()
        } else {
            self
        }
    }

    /// Records that this array is what a `std::array` was lowered to. See
    /// [`Self::is_std_array`].
    fn marked_std_array(mut self) -> Self {
        self.is_std_array = true;
        self
    }

    /// [`Self::marked_std_array`], for a fact which is only sometimes there.
    fn marked_std_array_if(self, is_std_array: bool) -> Self {
        if is_std_array {
            self.marked_std_array()
        } else {
            self
        }
    }

    /// Applies what a typedef's target carried for it. See [`TargetFacts`].
    fn marked_from(self, facts: TargetFacts) -> Self {
        self.marked_const_if(facts.is_const)
            .marked_std_array_if(facts.is_std_array)
    }

    fn map<T2, F: FnOnce(T) -> T2>(self, fun: F) -> Annotated<T2> {
        Annotated {
            ty: fun(self.ty),
            types_encountered: self.types_encountered,
            extra_apis: self.extra_apis,
            kind: self.kind,
            is_const: self.is_const,
            // Carried over, because the one use of this which does not merely
            // re-box - the lvalue reference below - overwrites it with the
            // answer for the type it built.
            has_volatile_pointee: self.has_volatile_pointee,
            is_std_array: self.is_std_array,
        }
    }
}

/// Options when converting a type.
/// It's possible we could add more policies here in future.
/// For example, Rust in general allows type names containing
/// __, whereas cxx doesn't. If we could identify cases where
/// a type will only ever be used in a bindgen context,
/// we could be more liberal. At the moment though, all outputs
/// from [TypeConverter] _might_ be used in the [cxx::bridge].
pub(crate) enum TypeConversionContext {
    /// Behind a reference, a pointer, or an array element of whatever we were
    /// asked to convert. `within_struct_field` remembers whether that
    /// outermost thing was a struct field, because a couple of the things
    /// bindgen emits - an opaque blob, above all - are fine as field data at
    /// any depth and are never fine in a signature.
    WithinReference {
        within_struct_field: bool,
    },
    WithinStructField {
        struct_type_params: HashSet<Ident>,
    },
    WithinContainer,
    /// The target of a typedef. Unlike every other context here this is a
    /// definition rather than a use: the alias may go on to be a struct field,
    /// a parameter, both or neither, and which of those it is decides what its
    /// target is allowed to be.
    ///
    /// Every position-dependent answer is the one `WithinReference { false }`
    /// gave when it stood in for this. Those are not a by-value parameter's
    /// answers, which differ in one place: a typedef may name a type only
    /// forward-declared, where an `OuterType` may not. The single change is
    /// that the one thing which can go wrong with a pointer target and depends
    /// on where it is - the target being a pointer to a pointer - is not
    /// decided here, but wherever the alias is converted, where it can be. The
    /// rest of what makes a pointee valid still is decided here. See
    /// `ensure_pointee_is_valid` and the `Type::Ptr` arm of
    /// `convert_type_path_which_is_not_a_reference`.
    WithinTypedef,
    OuterType,
}

impl TypeConversionContext {
    fn allow_instantiation_of_forward_declaration(&self) -> bool {
        matches!(self, Self::WithinReference { .. } | Self::WithinTypedef)
    }

    /// Whether the type being converted is a struct field, or sits inside one
    /// behind a reference, a pointer or an array.
    ///
    /// Note that a cxx container - `UniquePtr<T>`, `CxxVector<T>` - is *not*
    /// transparent to this, even when the container itself is a field: cxx
    /// spells the payload type out in the bridge, so nothing which can only be
    /// stored as bytes can go there whether or not a field holds the container.
    fn within_struct_field(&self) -> bool {
        match self {
            Self::WithinStructField { .. } => true,
            Self::WithinReference {
                within_struct_field,
            } => *within_struct_field,
            // `false` for a typedef target for the reason the doc comment on
            // `WithinTypedef` gives: this is the answer the context it replaced
            // gave. It is not an answer deferred to the eventual field - a
            // target which needs `true`, a bindgen opaque blob, is turned down
            // here and becomes an opaque type instead, so no field ever gets to
            // re-ask. Only a pointer target's own question waits for a use.
            Self::WithinContainer | Self::WithinTypedef | Self::OuterType => false,
        }
    }

    /// The context for the type behind a reference or pointer, or for the
    /// element type of an array.
    fn behind_reference(&self) -> Self {
        Self::WithinReference {
            within_struct_field: self.within_struct_field(),
        }
    }
    fn allowed_generic_type(&self, ident: &Ident) -> bool {
        !matches!(self,
            Self::WithinStructField { struct_type_params }
                if struct_type_params.contains(ident))
    }
}

/// A type which can convert from a type encountered in `bindgen`
/// output to the sort of type we should represeent to `cxx`.
/// As a simple example, `std::string` should be replaced
/// with [CxxString]. This also involves keeping track
/// of typedefs, and any instantiated concrete types.
///
/// To do this conversion correctly, this type relies on
/// inspecting the pre-existing list of APIs.
pub(crate) struct TypeConverter<'a> {
    types_found: HashSet<QualifiedName>,
    typedefs: HashMap<QualifiedName, TypedefTargetInfo>,
    concrete_templates: HashMap<String, QualifiedName>,
    /// Types we have only a stand-in for, mapped to why - which is known for
    /// a typedef whose target failed, and not for a plain forward declaration.
    forward_declarations: HashMap<QualifiedName, Option<OpaqueTypedefReason>>,
    /// Concrete template instantiations one of whose arguments is a type we
    /// have only a stand-in for, mapped to that argument. See
    /// [`ConvertErrorFromCpp::InstantiationOnIncompleteType`].
    instantiations_on_incomplete_types: HashMap<QualifiedName, QualifiedName>,
    /// Classes whose destructor C++ would have to write in this translation
    /// unit, and which hold by value something that destructor could not
    /// destroy. See [`ConvertErrorFromCpp::MemberOfInstantiationOnIncompleteType`].
    classes_we_may_not_destroy: HashMap<QualifiedName, UndestroyableMember>,
    /// Every concrete template instantiation, mapped to what it was built
    /// from. The synthesized name is flat - `au_Owner_AutocxxConcrete` says
    /// nothing about `Owner` - so this is the only way back to its arguments.
    concrete_definitions: HashMap<QualifiedName, Type>,
    /// Every alias, mapped to the target bindgen wrote for it.
    ///
    /// Not `typedefs`, which is the *analysed* target and so is empty in the
    /// phase which manufactures template instantiations: that analysis is what
    /// is being run. This is bindgen's own text, available in every phase, and
    /// is read only by [`Self::incompleteness_of_argument`], which has to see
    /// through an alias in a template argument before anything converts it.
    alias_targets: HashMap<QualifiedName, Type>,
    /// Every generic type bindgen emitted, mapped to the inner types each of
    /// its template parameters is bounded to have, by position. See
    /// [`ConvertErrorFromCpp::DependentQualifiedTypeOnSubstitute`].
    inner_types_required: HashMap<QualifiedName, Vec<Vec<String>>>,
    ignored_types: HashSet<QualifiedName>,
    /// The accessor surfaces of holders which already existed when a
    /// conversion worked one out. See [`Self::take_deferred_surfaces`].
    deferred_surfaces: HashMap<QualifiedName, HolderSurface>,
    config: &'a IncludeCppConfig,
    original_name_map: CppNameMap,
}

/// Why a class may not be destroyed here: the member which cannot be
/// destroyed, and the incomplete type its destruction would have needed.
#[derive(Clone, Debug)]
pub(crate) struct UndestroyableMember {
    /// What the class holds that cannot be destroyed, as a phrase naming it:
    /// a member or a base.
    pub(crate) held: String,
    /// The type nothing in this header defines.
    pub(crate) argument: QualifiedName,
}

/// What resolving a typedef left [`TypeConverter::resolve_typedef_target`]
/// with: the path to carry on converting, or a type it converted outright.
enum ResolvedTypedef {
    Path(TypePath, QualifiedName),
    /// Boxed because it is several times the size of the other variant, and
    /// this enum is returned at every level of a nested type - through the
    /// stack frames this split exists to shrink.
    Converted(Box<Annotated<Type>>),
}

impl<'a> TypeConverter<'a> {
    pub(crate) fn new<A: AnalysisPhase>(
        config: &'a IncludeCppConfig,
        apis: &ApiVec<A>,
        parse_callback_results: &ParseCallbackResults,
    ) -> Self
    where
        A::TypedefAnalysis: TypedefTarget,
    {
        let mut me = Self {
            types_found: find_types(apis),
            typedefs: Self::find_typedefs(apis),
            concrete_templates: Self::find_concrete_templates(apis),
            forward_declarations: Self::find_incomplete_types(apis),
            instantiations_on_incomplete_types: Self::find_instantiations_on_incomplete_types(apis),
            concrete_definitions: Self::find_concrete_definitions(apis),
            classes_we_may_not_destroy: HashMap::new(),
            alias_targets: Self::find_alias_targets(apis),
            inner_types_required: Self::find_inner_types_required(apis),
            ignored_types: Self::find_ignored_types(apis),
            deferred_surfaces: HashMap::new(),
            config,
            original_name_map: CppNameMap::new_for_analysis(apis),
        };
        // Last, because it reads the maps above.
        me.classes_we_may_not_destroy =
            me.find_classes_we_may_not_destroy(apis, parse_callback_results);
        me
    }

    /// The classes destroying which would make a C++ compiler destroy
    /// something it may not.
    ///
    /// A class holding one of
    /// [`Self::instantiations_on_incomplete_types`] by value is fine as long
    /// as C++ never writes its destructor here: a destructor the class
    /// declares itself is either defined in this translation unit - in which
    /// case the compiler already destroyed that member and accepted it - or
    /// defined in another one, in which case destroying an object here is a
    /// call and instantiates nothing. What is not fine is a destructor C++
    /// writes on first use, which is what an implicitly declared one and an
    /// `= default`ed one both are: writing it destroys the member, and that
    /// is the ill-formed part. C++ calls the difference *user-provided*.
    ///
    /// The relation closes over itself, because a class holding one of
    /// *these* by value - or inheriting from one - is in exactly the same
    /// position.
    ///
    /// What it takes on trust is that a user-provided destructor destroys its
    /// members without needing them complete *here*. A template whose
    /// destructor's exception specification names `sizeof` breaks that, as
    /// does one whose destructor body does and which a by-value copy
    /// instantiates; both are ill-formed today, with or without this rule, and
    /// deciding them needs a fact about the template rather than about the
    /// class holding it.
    ///
    /// A *concrete instantiation* built on one of the classes found here -
    /// `au<Owner>` - keeps its own smart-pointer support, so owning one of
    /// those still reaches the C++ this refuses. Codegen reads
    /// [`Api::ConcreteType::incomplete_argument`] for that, which is settled
    /// when the instantiation is manufactured, before any class is known
    /// undestroyable. Equally ill-formed before this rule and after it.
    fn find_classes_we_may_not_destroy<A: AnalysisPhase>(
        &self,
        apis: &ApiVec<A>,
        parse_callback_results: &ParseCallbackResults,
    ) -> HashMap<QualifiedName, UndestroyableMember> {
        let user_provided_destructors: HashSet<QualifiedName> = apis
            .iter()
            .filter_map(|api| match api {
                Api::Function { fun, .. }
                    if matches!(fun.special_member, Some(SpecialMemberKind::Destructor))
                        && fun.is_deleted.is_none() =>
                {
                    fun.self_ty.clone()
                }
                _ => None,
            })
            .collect();
        let mut found: HashMap<QualifiedName, UndestroyableMember> = HashMap::new();
        loop {
            let mut added = false;
            for api in apis.iter() {
                let Api::Struct { name, details, .. } = api else {
                    continue;
                };
                let name = &name.name;
                if user_provided_destructors.contains(name) || found.contains_key(name) {
                    continue;
                }
                let culprit = details
                    .item
                    .fields
                    .iter()
                    .find_map(|field| {
                        let argument = self.destruction_blocked_by(
                            &field.ty,
                            &found,
                            &mut HashSet::new(),
                            true,
                        )?;
                        Some(UndestroyableMember {
                            held: match &field.ident {
                                Some(id) => format!("its member `{id}`"),
                                None => "an unnamed member of it".to_string(),
                            },
                            argument,
                        })
                    })
                    .or_else(|| {
                        // A base is destroyed by the derived class's
                        // destructor exactly as a member is. bindgen writes
                        // most bases as a field, which the loop above already
                        // saw, but emits no storage at all for a virtual one.
                        //
                        // Read through the same traversal as a member rather
                        // than looking the name up directly, because a base
                        // may be written under an alias. A base bindgen could
                        // not name - which is what it reports for a template
                        // instantiation - is not here to read, so a class
                        // deriving virtually from one keeps its ownership
                        // surface.
                        parse_callback_results
                            .get_bases(name)?
                            .named
                            .iter()
                            .find_map(|base| {
                                let argument = self.destruction_blocked_by(
                                    &Type::Path(base.name.to_type_path()),
                                    &found,
                                    &mut HashSet::new(),
                                    true,
                                )?;
                                Some(UndestroyableMember {
                                    held: format!("its base class `{}`", base.name.to_cpp_name()),
                                    argument,
                                })
                            })
                    });
                if let Some(culprit) = culprit {
                    found.insert(name.clone(), culprit);
                    added = true;
                }
            }
            if !added {
                return found;
            }
        }
    }

    /// The incomplete type destroying something of type `ty` would need, if
    /// there is one: an instantiation built on such a type, or a class already
    /// found undestroyable, reached through any number of arrays and aliases.
    ///
    /// One traversal rather than two, because the wrappers interleave: a
    /// `typedef au<bb> Array[2]` is an alias to an array to an instantiation,
    /// and each step has to re-ask both questions.
    ///
    /// `seen` bars an alias already being expanded, so that a chain of them
    /// cannot walk in a circle.
    fn destruction_blocked_by(
        &self,
        ty: &Type,
        found: &HashMap<QualifiedName, UndestroyableMember>,
        seen: &mut HashSet<QualifiedName>,
        outermost: bool,
    ) -> Option<QualifiedName> {
        // Destroying an array destroys each element, and a `const` or
        // `volatile` member is destroyed exactly as a plain one is. The array
        // layers and the `const` markers interleave, so those are peeled
        // together; `volatile` is peeled by the recursion below, before
        // anything reads the type, or the marker's own argument list would be
        // read as the type's.
        let ty = unqualified_array_element_type(ty);
        let Type::Path(typ) = ty else {
            // A pointer or a reference to one of these destroys nothing.
            return None;
        };
        if let Some(inner) = unwrap_volatile(typ) {
            return self.destruction_blocked_by(inner, found, seen, outermost);
        }
        let qn = QualifiedName::from_type_path(typ);
        // A type we have only a stand-in for says nothing about the member
        // itself: C++ forbids a by-value member of an incomplete type
        // outright, so a member whose own type we call incomplete is one we
        // merely failed to understand - an opaque typedef standing in for a
        // perfectly destructible instantiation - and refusing those would
        // withdraw ownership from classes which work. As a template
        // *argument* it is exactly the signal this rule is built on.
        if !outermost && self.forward_declarations.contains_key(&qn) {
            return Some(qn);
        }
        if let Some(argument) = self.instantiations_on_incomplete_types.get(&qn) {
            return Some(argument.clone());
        }
        if let Some(member) = found.get(&qn) {
            return Some(member.argument.clone());
        }
        // The arguments, which are never the outermost type - `au<Owner>`
        // holds an `Owner` by value and so cannot be destroyed either.
        let blocked_argument = typ
            .path
            .segments
            .iter()
            .filter_map(|seg| match &seg.arguments {
                PathArguments::AngleBracketed(ab) => Some(ab.args.iter()),
                _ => None,
            })
            .flatten()
            .find_map(|arg| match arg {
                GenericArgument::Type(inner) => {
                    self.destruction_blocked_by(inner, found, &mut HashSet::new(), false)
                }
                _ => None,
            });
        if blocked_argument.is_some() {
            return blocked_argument;
        }
        if !seen.insert(qn.clone()) {
            return None;
        }
        // A concrete instantiation's name carries none of its arguments, so
        // read what it was built from.
        if let Some(definition) = self.concrete_definitions.get(&qn) {
            if let Some(argument) = self.destruction_blocked_by(definition, found, seen, outermost)
            {
                return Some(argument);
            }
        }
        // An alias for the member's own type is still the member's own type.
        self.alias_targets
            .get(&qn)
            .and_then(|target| self.destruction_blocked_by(target, found, seen, outermost))
    }

    /// Why `qn` may not be destroyed here, if it may not be: it holds by value
    /// something this translation unit could not destroy.
    fn undestroyable_member_error(&self, qn: &QualifiedName) -> Option<ConvertErrorFromCpp> {
        self.classes_we_may_not_destroy.get(qn).map(|member| {
            ConvertErrorFromCpp::MemberOfInstantiationOnIncompleteType {
                class: qn.clone(),
                held: member.held.clone(),
                argument: member.argument.clone(),
            }
        })
    }

    /// Whether destroying one of these here is C++ we may not write. Read by
    /// the function analysis, which puts it on the class it belongs to.
    pub(crate) fn undestroyable_member_of(
        &self,
        qn: &QualifiedName,
    ) -> Option<UndestroyableMember> {
        self.classes_we_may_not_destroy.get(qn).cloned()
    }

    pub(crate) fn convert_boxed_type(
        &mut self,
        ty: Box<Type>,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Box<Type>>, ConvertErrorFromCpp> {
        Ok(self.convert_type(*ty, ns, ctx)?.map(Box::new))
    }

    pub(crate) fn convert_type(
        &mut self,
        ty: Type,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Type>, ConvertErrorFromCpp> {
        let result = match ty {
            Type::Path(p) => self.convert_type_path(p, ns, ctx)?,
            Type::Reference(r) => self.convert_reference(r, ns, ctx)?,
            Type::Array(arr) => self.convert_array(arr, ns, ctx)?,
            Type::Ptr(ptr) => self.convert_ptr(ptr, ns, ctx)?,
            _ => {
                return Err(ConvertErrorFromCpp::UnknownType(
                    ty.to_token_stream().to_string(),
                ))
            }
        };
        Ok(result)
    }

    fn convert_type_path(
        &mut self,
        typ: TypePath,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Type>, ConvertErrorFromCpp> {
        if let Some(result) = Self::function_pointer(&typ, ctx) {
            return result;
        }
        // First we try to spot if these are the special marker paths that
        // bindgen uses to denote references or other things. Note that
        // there is deliberately no `__bindgen_marker_UnusedTemplateParam`
        // case here: bindgen reports that condition per-item through the
        // `denote_discards_template_param` callback (see
        // `ParseCallbackResults::discards_template_param`), not by wrapping
        // a type.
        if let Some(ty) = unwrap_const(&typ) {
            // C++ qualified this type `const` in its own right. Rust cannot
            // spell that, so the type is whatever the qualifier was applied
            // to and the fact travels alongside on the `Annotated`. Two things
            // read it: `find_constructors_present`, for which a `const` member
            // with no initializer deletes the class's implicitly declared
            // default constructor, and the return-type analysis in `fun`,
            // which routes a `const`-returning function through a wrapper.
            Ok(self.convert_type(ty.clone(), ns, ctx)?.marked_const())
        } else if let Some(ty) = unwrap_volatile(&typ) {
            // The qualifier is peeled and the type carries on as what it
            // qualifies. Nothing is refused here, because this arm is reached
            // for a by-value parameter or return as well, and there the
            // qualifier is genuinely spent: C++ has copied the value by the
            // time Rust sees it, and `volatile` on a by-value parameter governs
            // only the callee's re-reads of its own local copy.
            //
            // The positions where it is not spent are refused where the type
            // still says so, before conversion peels it: a variable
            // (`analyze_static`), a data member's accessor (`member_shape`), a
            // POD struct's field (`byvalue_checker`) and a template argument
            // (below). A `volatile` *pointee* is the position still without an
            // answer. It is not refused here, and it does not work either: cxx
            // declares the function by taking its address, so a `volatile T*`
            // parameter leaves the generated C++ initializing a `void (*)(T*)`
            // from a `void (volatile T*)`, which the C++ compiler rejects.
            // Turning that into a refusal would be an improvement on the
            // diagnostic and not on the outcome; what the position actually
            // needs is a synthesized accessor built on
            // `read_volatile`/`write_volatile`, since Rust has no volatile
            // pointer type for the marker to become. That work slots in at
            // `convert_ptr`.
            self.convert_type(ty.clone(), ns, ctx)
        } else if let Some(ty) = unwrap_has_opaque(&typ) {
            // bindgen could not name the C++ type here, so it substituted a
            // blob of bytes of the right size and alignment. As field data
            // that is exactly what autocxx wants - the layout is all it needs -
            // and it stays field data however many pointers and arrays nest it.
            // In a signature the blob is a different type from the one C++
            // wrote: `void f(Thing)` would become `fn f(u32)` for a four-byte
            // `Thing`, and the shim would compile. So refuse it there, in each
            // of the positions a signature can put it: the parameter or return
            // type itself, behind a reference or a pointer, as an array
            // element, and as the payload of a cxx container.
            if !ctx.within_struct_field() {
                return Err(ConvertErrorFromCpp::BindgenOpaqueBlob(
                    ty.to_token_stream().to_string(),
                ));
            }
            self.convert_type(ty.clone(), ns, ctx)
        } else if let Some(ty) = unwrap_long_double(&typ) {
            // C++ `long double`. bindgen substituted a Rust type of the right
            // size - `f64` where the type is 8 bytes, an integer of the same
            // width where it is 16 - and no Rust type has both that size and
            // `long double`'s calling convention, so a signature mentioning
            // one would compile and pass the wrong bytes. Turn it down; the
            // error says what to do instead.
            //
            // As field data the substitute is fine, because Rust only carries
            // those bytes around. Such a struct still cannot be passed by
            // value, for the same ABI reason - `ByValueChecker` refuses it,
            // because it reads the field types before this unwrapping and so
            // sees the marker rather than what it wraps.
            if !ctx.within_struct_field() {
                return Err(ConvertErrorFromCpp::LongDouble);
            }
            self.convert_type(ty.clone(), ns, ctx)
        } else if let Some(ty) = unwrap_float128(&typ) {
            // C++ `__float128`. bindgen substituted `u128`, which is the right
            // size and an integer; Rust has no 128-bit float to offer instead.
            // A signature naming the substitute would compile and would be
            // read as a different kind of number on both sides, so turn it
            // down. `unsigned __int128`, which arrives as the same bare `u128`
            // when this marker is absent, really is that integer and is
            // supported.
            //
            // As field data the substitute is fine, for the reason it is fine
            // for a `long double`, and `ByValueChecker` refuses such a struct
            // by value for the same ABI reason.
            if !ctx.within_struct_field() {
                return Err(ConvertErrorFromCpp::Float128);
            }
            self.convert_type(ty.clone(), ns, ctx)
        } else if let Some(ty) = unwrap_bitfield(&typ) {
            // A bindgen bitfield unit is a `__BindgenBitfieldUnit` wrapping
            // the byte array C++ actually laid the bitfields out in. cxx has
            // no business knowing about the wrapper - and couldn't name it
            // anyway, since its name contains `__` - so pretend the field is
            // just that storage.
            self.convert_type(ty.clone(), ns, ctx)
        } else if let Some(ty) = unwrap_std_array(&typ) {
            // The array a `std::array<T, N>` was lowered to. The type is that
            // array - cxx spells a Rust `[T; N]` as `std::array<T, N>`, so the
            // bridge names the class the header was written with - and the
            // marker only says which of the two C++ array types produced it.
            // That fact travels alongside on the `Annotated`, for
            // `convert_lvalue_reference`, which is where the two differ.
            Ok(self.convert_type(ty.clone(), ns, ctx)?.marked_std_array())
        } else if let Some(ptr) = unwrap_reference(&typ, false) {
            self.convert_lvalue_reference(ptr, ns, ctx)
        } else if let Some(ptr) = unwrap_reference(&typ, true) {
            self.convert_rvalue_reference(ptr, ns, ctx)
        } else {
            self.convert_path_which_is_not_a_reference(typ, ns, ctx)
        }
    }

    /// What [`Self::convert_type_path_which_is_not_a_reference`] does before
    /// it has a path to work on: resolve a typedef, and answer either with the
    /// path it resolved to or with the converted type outright, for the
    /// targets which are not paths at all.
    ///
    /// Its own function, and never inlined, so that the locals of all these
    /// cases are gone before the caller recurses through a template argument.
    /// A debug build gives every one of them a stack slot which lives as long
    /// as the call it is written in, and that call is a recursive one.
    #[inline(never)]
    fn resolve_typedef_target(
        &mut self,
        typ: TypePath,
        original_tn: QualifiedName,
        ns: &Namespace,
        ctx: &TypeConversionContext,
        deps: &mut HashSet<QualifiedName>,
        target: TargetFacts,
    ) -> Result<ResolvedTypedef, ConvertErrorFromCpp> {
        let resolved = match self.resolve_typedef(&original_tn)? {
            None => ResolvedTypedef::Path(typ, original_tn),
            Some(TypedefTargetInfo {
                ty: Type::Path(resolved_tp),
                ..
            }) => {
                // The typedef may resolve to a C function pointer - see
                // `function_pointer`, which decides what to do with one and is
                // the only thing that should: nothing within it needs
                // converting, and the `Option` wrapping it must not be
                // mistaken for a type we should go looking for.
                if let Some(result) = Self::function_pointer(resolved_tp, ctx) {
                    return result
                        .map(|mut annotated| {
                            annotated.types_encountered.extend(std::mem::take(deps));
                            annotated.marked_const_if(target.is_const)
                        })
                        .map(Box::new)
                        .map(ResolvedTypedef::Converted);
                }
                // `Pin<&mut T>` is not a name to go looking for: it is what
                // analysing the typedef already made of a C++ mutable
                // reference, and it is finished. Read as a name it is the
                // generic `core::pin::Pin`, which cxx knows nothing about, so
                // autocxx would invent a concrete type for it and write
                // `T&*` into the generated C++. See google/autocxx#1363.
                if extract_pinned_mutable_reference_type(resolved_tp).is_some() {
                    return Ok(ResolvedTypedef::Converted(Box::new(Annotated::new(
                        Type::Path(resolved_tp.clone()),
                        std::mem::take(deps),
                        ApiVec::new(),
                        TypeKind::MutableReference,
                    ))));
                }
                let resolved_tn = QualifiedName::from_type_path(resolved_tp);
                deps.insert(resolved_tn.clone());
                ResolvedTypedef::Path(resolved_tp.clone(), resolved_tn)
            }
            Some(TypedefTargetInfo {
                ty: Type::Ptr(resolved_tp),
                kind,
                ..
            }) => {
                // The typedef resolves to a pointer. Its pointee may
                // itself involve typedefs (e.g. typedef char C;
                // typedef C* S;), so convert it like any directly
                // written pointer instead of passing it through
                // verbatim — otherwise the unresolved pointee name
                // reaches cxx and generation fails with
                // "unsupported type". See google/autocxx#1368.
                let is_rvalue_reference = matches!(kind, TypeKind::RValueReference);
                let mut annotated = self.convert_ptr(resolved_tp.clone(), ns, ctx)?;
                annotated.types_encountered.extend(std::mem::take(deps));
                // A C++ rvalue reference converts to a pointer as well, so
                // which of the two this alias names cannot be read back off
                // the type; that is why the typedef's analysis recorded it.
                // Calling `typedef T&& R` a pointer costs the caller the one
                // fact it needs - the parameter is something to move from -
                // and the C++ shim it then writes takes `T*` and hands it
                // straight to a function wanting `T&&`, which no compiler
                // accepts. See google/autocxx#1363.
                if is_rvalue_reference {
                    annotated.kind = TypeKind::RValueReference;
                }
                return Ok(ResolvedTypedef::Converted(Box::new(
                    annotated.marked_const_if(target.is_const),
                )));
            }
            Some(TypedefTargetInfo { ty: other, .. }) => {
                // Anything else the typedef resolved to was converted when the
                // typedef itself was analysed, so take it as it stands - but
                // say what kind it is. A typedef to a C++ reference lands here
                // as `&T`, and calling that `Regular` costs the caller the one
                // fact it needs: whether the value borrows, which decides
                // lifetimes on a returned reference and how a parameter
                // crosses the bridge. Here the type says which it is, so read
                // it; what the typedef recorded is only needed where two
                // different C++ constructs converge on one Rust type, which is
                // the pointer arm above. See google/autocxx#1363.
                let kind = match other {
                    Type::Reference(reference) if reference.mutability.is_some() => {
                        TypeKind::MutableReference
                    }
                    Type::Reference(_) => TypeKind::Reference,
                    _ => TypeKind::Regular,
                };
                return Ok(ResolvedTypedef::Converted(Box::new(
                    // `typedef std::array<T, N> A` lands here, the target
                    // having been converted to the bare `[T; N]` when the
                    // typedef was analysed. Nothing in that says which of the
                    // two C++ array types it was, so the alias carries the
                    // answer for it.
                    Annotated::new(other.clone(), std::mem::take(deps), ApiVec::new(), kind)
                        .marked_from(target),
                )));
            }
        };
        Ok(resolved)
    }

    /// The `T&` arm of [`Self::convert_type`].
    ///
    /// Its own function, and never inlined, because a debug build gives every
    /// arm of a `match` a stack slot of its own and holds them all for the
    /// length of the call. `convert_type` recurses through here, so every
    /// arm's locals would otherwise be paid for at each level of a nested
    /// type.
    #[inline(never)]
    fn convert_reference(
        &mut self,
        mut r: syn::TypeReference,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Type>, ConvertErrorFromCpp> {
        let innerty = self.convert_boxed_type(r.elem, ns, &ctx.behind_reference())?;
        // As for the reference bindgen writes with a marker; see
        // `check_array_referent`.
        Self::check_array_referent(&innerty, ctx)?;
        r.elem = innerty.ty;
        Ok(Annotated::new(
            Type::Reference(r),
            innerty.types_encountered,
            innerty.extra_apis,
            TypeKind::Reference,
        ))
    }

    /// The array arm of [`Self::convert_type`]. Not inlined, for the reason
    /// [`Self::convert_reference`] gives.
    #[inline(never)]
    fn convert_array(
        &mut self,
        mut arr: syn::TypeArray,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Type>, ConvertErrorFromCpp> {
        let innerty = self.convert_type(*arr.elem, ns, &ctx.behind_reference())?;
        // An array of `const` elements is as unassignable as a `const`
        // scalar, and C++ says so outright: an array type whose element
        // type is cv-qualified is itself cv-qualified. bindgen agrees -
        // it folds a const element into the array's own constness - but
        // it also leaves the marker on the element, and for
        // `const T a[2][3]` the outermost node we are handed is the
        // array rather than a marker. So the fact has to come up from
        // the element here, or a multidimensional const array looks
        // assignable.
        let is_const = innerty.is_const;
        arr.elem = Box::new(innerty.ty);
        Ok(Annotated::new(
            Type::Array(arr),
            innerty.types_encountered,
            innerty.extra_apis,
            TypeKind::Regular,
        )
        .marked_const_if(is_const))
    }

    /// The `T&` case of [`Self::convert_type_path`].
    ///
    /// Its own function, and never inlined, because a debug build gives every
    /// branch of a function a stack slot of its own and holds them all for the
    /// length of the call. `convert_type` recurses through here, so each
    /// branch's locals would otherwise be paid for at every level of a nested
    /// type.
    #[inline(never)]
    fn convert_lvalue_reference(
        &mut self,
        ptr: &syn::TypePtr,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Type>, ConvertErrorFromCpp> {
        // LValue reference
        let mutability = ptr.mutability;
        // Read before conversion, which peels the marker off the referent.
        let referent_is_volatile = is_volatile_qualified(&ptr.elem);
        let elem = self.convert_boxed_type(ptr.elem.clone(), ns, &ctx.behind_reference())?;
        // A `rust::Str` referent has already been turned into `&str` by the
        // `should_dereference_in_cpp` branch below, so a C++ `rust::Str&`
        // gets wrapped again here into `&&str`. That is deliberate and
        // correct: cxx spells `&str` as a `rust::Str` value and `&T` as
        // `const T&`, so `&&str` *is* `const rust::Str&`, and `rust::Str`
        // has the same (pointer, length) layout as Rust's `&str`.
        // `test_pass_rust_str_by_ref` runs that shape end to end, and
        // `test_pass_rust_str` the plain value it wraps.
        //
        // A *mutable* `rust::Str&` is refused, which is what the check
        // below does. It would become `Pin<&mut &str>`: the slot belongs
        // to C++, which is free to write a fat pointer of its own into it,
        // after which Rust holds a `&str` whose lifetime nothing checked.
        // Under `ReferencesWrappedAllFunctionsSafe` the same parameter
        // becomes a `CppMutRef` instead, which Rust never dereferences
        // except through an unsafe call the caller vouches for, so there
        // it is kept - `test_pass_rust_str_by_mut_ref_cpprefs` covers it.
        // The const case is untouched either way, because `&&str` hands
        // Rust no way to write to the slot.
        //
        // `rust::Str` is the only type autocxx represents as a borrowed
        // fat pointer, so it is the only shape this catches.
        // `rust::Slice<T>` is not a known type at all: bindgen discards
        // its template parameter, so any signature mentioning one is
        // already turned down with `UnusedTemplateParam` before reaching
        // here, by value and by reference alike
        // (`test_rust_slice_never_reaches_this`). `rust::String&` is a
        // different problem, not this one - it owns its contents, so there
        // is no unchecked borrow, and what goes wrong there is that cxx
        // wants `&mut String` where autocxx writes `Pin<&mut String>`.
        //
        // A struct *field* is exempt, because no `Pin<&mut &str>` reaches
        // Rust from one. A struct with a reference field is never POD -
        // `generate_pod!` on one already fails, bindgen's reference marker
        // not being a type the POD analysis knows - so such a struct is
        // always opaque, and its fields are bytes Rust cannot name, let
        // alone write through. Refusing the field instead loses autocxx
        // the knowledge that the struct has a reference member, and it
        // then offers a default constructor C++ has deleted; see
        // `test_rust_str_reference_field_is_left_alone`.
        //
        // `using StrRef = rust::Str&` is refused at the alias itself,
        // where the context is `WithinTypedef` and no use is in sight yet.
        // A signature mentioning the alias then loses the alias it depends
        // on, which is the right answer; a struct field of that type stays
        // fine, because the struct was going to be opaque either way.
        // `test_rust_str_reference_field_is_left_alone` covers the field
        // spelt both ways.
        if mutability.is_some()
            && !ctx.within_struct_field()
            && Self::is_rust_str(&elem.ty)
            && !self.config.unsafe_policy.requires_cpprefs()
        {
            return Err(ConvertErrorFromCpp::MutableReferenceToRustStr);
        }
        Self::check_array_referent(&elem, ctx)?;
        let mut outer = elem.map(|elem| match mutability {
            Some(_) => Type::Path(parse_quote! {
                ::core::pin::Pin < & #mutability #elem >
            }),
            None => Type::Reference(parse_quote! {
                & #elem
            }),
        });
        outer.kind = if mutability.is_some() {
            TypeKind::MutableReference
        } else {
            TypeKind::Reference
        };
        outer.has_volatile_pointee = referent_is_volatile;
        // A reference to a `std::array` is not itself one.
        outer.is_std_array = false;
        Ok(outer)
    }

    /// Turn down a reference to an array C++ wrote as `T[N]`.
    ///
    /// `const T (&)[N]` and `const std::array<T, N>&` are different C++ types
    /// which reach autocxx as the same `&[T; N]`, bindgen writing `[T; N]` for
    /// the class and for the C array alike. cxx spells that back as
    /// `std::array<T, N>`, so it is the class the bridge would name, and only
    /// one of the two is therefore bindable. Which one this is, the type no
    /// longer says; [`Annotated::is_std_array`] does, carrying the marker
    /// `36-std-array-marker.patch` puts on the class.
    ///
    /// A struct field is exempt. No reference reaches Rust from one - a struct
    /// with a reference member is never POD, so it is opaque and its fields are
    /// bytes nobody names - and refusing the field would instead lose autocxx
    /// the knowledge that the member is there, which is what tells it C++ has
    /// deleted the default constructor.
    fn check_array_referent(
        elem: &Annotated<Box<Type>>,
        ctx: &TypeConversionContext,
    ) -> Result<(), ConvertErrorFromCpp> {
        if matches!(*elem.ty, Type::Array(_)) && !elem.is_std_array && !ctx.within_struct_field() {
            return Err(ConvertErrorFromCpp::CppArrayReferenceInSignature(
                elem.ty.to_token_stream().to_string(),
            ));
        }
        Ok(())
    }

    /// The `T&&` case of [`Self::convert_type_path`].
    ///
    /// Its own function, and never inlined, because a debug build gives every
    /// branch of a function a stack slot of its own and holds them all for the
    /// length of the call. `convert_type` recurses through here, so each
    /// branch's locals would otherwise be paid for at every level of a nested
    /// type.
    #[inline(never)]
    fn convert_rvalue_reference(
        &mut self,
        ptr: &syn::TypePtr,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Type>, ConvertErrorFromCpp> {
        // RValue reference
        Self::ensure_pointee_is_valid(ptr, ctx)?;
        let pointee_is_volatile = is_volatile_qualified(&ptr.elem);
        let innerty = self.convert_boxed_type(ptr.elem.clone(), ns, &ctx.behind_reference())?;
        let mut ptr = ptr.clone();
        ptr.elem = innerty.ty;
        Ok(Annotated::new(
            Type::Ptr(ptr),
            innerty.types_encountered,
            innerty.extra_apis,
            TypeKind::RValueReference,
        )
        .marked_volatile_pointee_if(pointee_is_volatile))
    }

    /// The plain-path case of [`Self::convert_type_path`].
    ///
    /// Its own function, and never inlined, because a debug build gives every
    /// branch of a function a stack slot of its own and holds them all for the
    /// length of the call. `convert_type` recurses through here, so each
    /// branch's locals would otherwise be paid for at every level of a nested
    /// type.
    #[inline(never)]
    fn convert_path_which_is_not_a_reference(
        &mut self,
        typ: TypePath,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Type>, ConvertErrorFromCpp> {
        // An actual path
        let newp = self.convert_type_path_which_is_not_a_reference(typ, ns, ctx)?;
        if let Type::Path(newpp) = &newp.ty {
            let qn = QualifiedName::from_type_path(newpp);
            if !ctx.allow_instantiation_of_forward_declaration() {
                if self.forward_declarations.contains_key(&qn) {
                    return Err(self.incomplete_type_error(qn));
                }
                // Not in a struct field, unlike the forward declaration
                // above: an instantiation built on one is a complete type,
                // and a class may have a member of it. Whether *that*
                // class can then be destroyed is C++'s business and its
                // author's, and refusing the member would only hide the
                // member's type from the analysis which decides what
                // constructors the class has.
                if !ctx.within_struct_field() {
                    if let Some(err) = self.instantiation_on_incomplete_type_error(&qn) {
                        return Err(err);
                    }
                    // The class holding such a member, where destroying one
                    // is what makes C++ write the destructor which destroys
                    // it. Refused in the same positions and let through in
                    // the same ones: naming it, and holding a reference or a
                    // pointer to it, destroy nothing.
                    if let Some(err) = self.undestroyable_member_error(&qn) {
                        return Err(err);
                    }
                }
            }
            // Special handling because rust_Str (as emitted by bindgen)
            // doesn't simply get renamed to a different type _identifier_.
            // This plain type-by-value (as far as bindgen is concerned)
            // is actually a &str.
            if known_types().should_dereference_in_cpp(&qn) {
                Ok(Annotated::new(
                    Type::Reference(parse_quote! {
                        &str
                    }),
                    newp.types_encountered,
                    newp.extra_apis,
                    TypeKind::Reference,
                ))
            } else {
                Ok(newp)
            }
        } else {
            Ok(newp)
        }
    }

    /// Whether a converted referent is the `&str` which autocxx uses to
    /// represent a C++ `rust::Str`.
    ///
    /// The question is asked of the *converted* type rather than of the name
    /// C++ wrote, so that a typedef to `rust::Str` answers it too: resolving
    /// the alias is exactly what the conversion has just done. Nothing else
    /// converts to a shared Rust reference at this point - C++ has no
    /// reference to a reference - so `&str` here means `rust::Str` and
    /// nothing else.
    fn is_rust_str(ty: &Type) -> bool {
        matches!(ty, Type::Reference(r)
            if r.mutability.is_none()
                && matches!(&*r.elem, Type::Path(p)
                    if p.qself.is_none() && p.path.is_ident("str")))
    }

    fn convert_type_path_which_is_not_a_reference(
        &mut self,
        mut typ: TypePath,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Type>, ConvertErrorFromCpp> {
        // First, qualify any unqualified paths.
        let first_seg = &typ.path.segments.iter().next().unwrap().ident;
        if first_seg != "root" && first_seg != "output" {
            let ty = QualifiedName::from_type_path(&typ);
            // If the type looks like it is unqualified, check we know it
            // already, and if not, qualify it according to the current
            // namespace. This is a bit of a shortcut compared to having a full
            // resolution pass which can search all known namespaces.
            if !known_types().is_known_type(&ty) {
                let num_segments = typ.path.segments.len();
                if num_segments > 1 {
                    return Err(ConvertErrorFromCpp::UnsupportedBuiltInType(ty));
                }
                if !self.types_found.contains(&ty) {
                    typ.path.segments = std::iter::once("output")
                        .chain(ns.iter())
                        .map(|s| {
                            let i = make_ident(s);
                            parse_quote! { #i }
                        })
                        .chain(typ.path.segments)
                        .collect();
                }
            }
        }

        let original_tn = QualifiedName::from_type_path(&typ);
        original_tn
            .validate_ok_for_cxx()
            .map_err(ConvertErrorFromCpp::InvalidIdent)?;
        if self.config.is_on_blocklist(&original_tn.to_cpp_name()) {
            return Err(ConvertErrorFromCpp::Blocked(original_tn));
        }
        let mut deps = HashSet::new();

        // Now convert this type itself.
        deps.insert(original_tn.clone());
        // A `typedef const int ci` is `const` at the alias, not at each use of
        // it, so the constness of what we are about to resolve has to come
        // from the typedef's own analysis. Read before the match below, which
        // has several exits and would have to carry it through all of them.
        let target = self
            .resolve_typedef(&original_tn)?
            .map(|target| TargetFacts {
                is_const: target.is_const,
                is_std_array: target.is_std_array,
            })
            .unwrap_or_default();
        // First let's see if this is a typedef.
        let (mut typ, tn) =
            match self.resolve_typedef_target(typ, original_tn, ns, ctx, &mut deps, target)? {
                ResolvedTypedef::Path(typ, tn) => (typ, tn),
                ResolvedTypedef::Converted(annotated) => return Ok(*annotated),
            };

        // A cxx smart pointer whose payload C++ qualified `const` -
        // `std::shared_ptr<const T>` - has no cxx spelling: `SharedPtr<T>`
        // drops the qualifier, and the C++ typecheck shim cxx then writes
        // fails to bind against the real signature. Lower the whole
        // instantiation to an opaque C++ type instead, whose definition is
        // that exact specialization, so the `const` stays where C++ can see
        // it. Done before substitution, because it is the C++ spelling of the
        // container - not cxx's - that the typedef has to name. See
        // google/autocxx#799.
        //
        // All three of the smart pointers are lowered, and each gets the
        // accessors its C++ type has (see `HolderSurface`).
        //
        // Asked only of the containers whose answer it is - the two places
        // below which read it are both within a cxx container. The question is
        // fallible, because reading a qualifier off an alias means resolving
        // the alias, and a template argument of a container cxx knows nothing
        // about is one nobody resolves: it goes into an opaque holder whole,
        // and asking would turn an alias autocxx cannot follow into a refusal
        // of a type which never needed it followed.
        //
        // A `volatile` argument is turned down before any of that. cxx names a
        // container's payload as a plain type, with nowhere to put the
        // qualifier, so `UniquePtr<T>` is what would be declared for a
        // `std::unique_ptr<volatile T>` - a different C++ type, which the shim
        // cxx writes then fails to bind against. Lowering to a holder is what
        // rescues the `const` case and cannot rescue this one: the holder's
        // accessors would still have to hand the payload to Rust, and Rust has
        // no volatile type to hand it as. Asked here, before the branches
        // below convert the arguments and peel the marker off.
        let generic_behavior = known_types().cxx_generic_behavior(&tn);
        if generic_behavior != CxxGenericType::Not && mentions_volatile(&Type::Path(typ.clone())) {
            return Err(ConvertErrorFromCpp::VolatileTemplateArgument);
        }
        // A container's whole purpose is to give Rust something to reach the
        // payload through, and a `std::string_view` payload has nothing to hand
        // back - Rust has no type which is a view. Asked for every container,
        // because the holder routes just below would otherwise build accessors
        // naming it, and the plain-payload predicates further on would report it
        // as a container problem rather than as the view it is about.
        //
        // The payload is asked about directly - one level, through one pointer,
        // with aliases resolved - rather than searched for at any depth. A view
        // nested inside some *other* template is that template's business: such
        // an instantiation becomes an opaque holder of its own, whose accessors
        // hand back the holder and never the view, and which is a shape that
        // works.
        if generic_behavior != CxxGenericType::Not {
            for arg in direct_generic_args(&typ) {
                // Through a pointer, and out of the `const` marker bindgen
                // wraps a qualified argument in - `shared_ptr<const T>` arrives
                // as `shared_ptr<__bindgen_marker_Const<T>>`.
                let arg = match &arg {
                    Type::Ptr(p) => (*p.elem).clone(),
                    Type::Path(p) => unwrap_const(p).cloned().unwrap_or(arg),
                    _ => arg,
                };
                if let Type::Path(p) = &arg {
                    let mut tn = QualifiedName::from_type_path(p);
                    if let Some(TypedefTargetInfo {
                        ty: Type::Path(target),
                        ..
                    }) = self.resolve_typedef(&tn)?
                    {
                        tn = QualifiedName::from_type_path(target);
                    }
                    if known_types().is_string_view(&tn) {
                        return Err(ConvertErrorFromCpp::StringViewOutOfCpp);
                    }
                }
            }
        }
        let payload_is_const = generic_behavior != CxxGenericType::Not
            && self.generic_args_are_const_qualified(&typ)?;
        if matches!(
            generic_behavior,
            CxxGenericType::CppUniquePtr | CxxGenericType::CppSharedPtr
        ) && payload_is_const
        {
            let mut extra_apis = ApiVec::new();
            let surface = self.const_smart_pointer_surface(&tn, &typ, ns, &mut extra_apis)?;
            return self.lower_to_holder(typ, surface, deps, extra_apis, target.is_const);
        }

        // A `std::vector` of pointers has no cxx spelling either.
        // `CxxVector<T>` needs a `T: VectorElement`, which cxx implements for
        // its own types and for opaque `ExternType`s and could not implement
        // for a raw pointer even if Rust had a way to write `vector<T*>`'s
        // element as one. autocxx never got that far: a pointer is not a path,
        // so `confirm_inner_type_is_acceptable_generic_payload` refused the
        // whole signature as `TemplatedTypeContainingNonPathArg`. Lower the
        // instantiation to the same kind of opaque holder the branch above
        // uses - definition `std::vector<T*>`, so the element type stays where
        // C++ can see it - and give it a read-only accessor surface. See
        // google/autocxx#330.
        if generic_behavior == CxxGenericType::CppVector {
            if let Some(element) = sole_pointer_generic_arg(&typ) {
                // Conversion is what turns bindgen's spelling of the pointee
                // into the bridge's, and what turns down a pointee the bridge
                // could not name - a pointer to a pointer, above all.
                //
                // The names it met go onto the holder, for the reason the
                // smart-pointer branch above gives: the accessors are what
                // name them, and `deps.rs` reads them back from there.
                let mut element =
                    self.convert_type(element, ns, &TypeConversionContext::WithinContainer)?;
                let extra_apis = std::mem::take(&mut element.extra_apis);
                let surface = Some(HolderSurface::VectorOfPointers {
                    element: Box::new(element.ty.into()),
                    deps: element.types_encountered,
                });
                return self.lower_to_holder(typ, surface, deps, extra_apis, target.is_const);
            }
        }

        // Now let's see if it's a known type.
        // (We may entirely reject some types at this point too.)
        let mut typ = match known_types().consider_substitution(&tn) {
            Some(mut substitute_type) => {
                if let Some(last_seg_args) = typ
                    .path
                    .segments
                    .into_iter()
                    .next_back()
                    .map(|ps| ps.arguments)
                {
                    let last_seg = substitute_type.path.segments.last_mut().unwrap();
                    last_seg.arguments = last_seg_args;
                }
                substitute_type
            }
            None => {
                // Pass through as-is, but replacing `root` with `output`
                // in the first path element. We don't just create a whole
                // new `TypePath` because that would discard any generics.
                if let Some(first_seg) = typ.path.segments.get_mut(0) {
                    if first_seg.ident == "root" {
                        first_seg.ident = make_ident("output").0;
                    }
                }
                typ
            }
        };

        let mut extra_apis = ApiVec::new();
        let mut kind = TypeKind::Regular;

        // Finally let's see if it's generic.
        if let Some(last_seg) = Self::get_generic_args(&mut typ) {
            let forward_declarations_ok = generic_behavior == CxxGenericType::Rust;
            if generic_behavior != CxxGenericType::Not {
                // this is a type of generic understood by cxx (e.g. CxxVector)
                // so let's convert any generic type arguments. This recurses.
                if let PathArguments::AngleBracketed(ref mut ab) = last_seg.arguments {
                    // The smart pointers were lowered above. The rest of what
                    // cxx spells generically - `CxxVector<T>`, `rust::Box<T>` -
                    // names its payload as a plain type with no room for a
                    // qualifier, and is not lowered. For `std::vector` it
                    // cannot be: an opaque holder has to be a complete type
                    // for cxx, and completing a `std::vector<const T>` is
                    // ill-formed. `rust::Box<const T>` would complete, but
                    // nothing else about it is worked out - cxx's Rust-side
                    // `Box<T>` has no room for the qualifier either - so it is
                    // turned down alongside. Either way, say what is wrong
                    // rather than declare a container of a mutable payload and
                    // leave the C++ compiler to complain about a
                    // specialization nobody wrote.
                    if payload_is_const {
                        return Err(ConvertErrorFromCpp::ConstCxxContainerPayload(tn.clone()));
                    }
                    // The payload names as bindgen wrote them, read before
                    // conversion because that is the only moment an alias can
                    // be told from what it resolves to.
                    let payloads_as_written = payload_names_as_written(&ab.args);
                    let mut innerty = self.convert_punctuated(
                        ab.args.clone(),
                        ns,
                        &TypeConversionContext::WithinContainer,
                    )?;
                    ab.args = innerty.ty;
                    match generic_behavior {
                        CxxGenericType::CppUniquePtr => {
                            rename_unique_ptr_payloads(
                                &mut ab.args,
                                &payloads_as_written,
                                &mut deps,
                            );
                        }
                        CxxGenericType::CppSharedPtr => {
                            refuse_aliased_atom_payload(&ab.args, &payloads_as_written)?;
                        }
                        _ => {}
                    }
                    // Converting the payload may have manufactured a type -
                    // the opaque holder of a `std::shared_ptr<const T>`, or
                    // any other concrete instantiation - and the bridge names
                    // it in this container's own declaration, so its `Api` has
                    // to travel out with the type. Dropping it left the
                    // enclosing function refused for depending on a type
                    // nothing declared. See google/autocxx#799.
                    extra_apis.append(&mut innerty.extra_apis);
                    kind = self.confirm_inner_type_is_acceptable_generic_payload(
                        &ab.args,
                        &tn,
                        generic_behavior,
                        forward_declarations_ok,
                    )?;
                    deps.extend(innerty.types_encountered.drain(..));
                } else {
                    return Err(ConvertErrorFromCpp::TemplatedTypeContainingNonPathArg(
                        tn.clone(),
                    ));
                }
            } else {
                // Oh poop. It's a generic type which cxx won't be able to handle.
                // We'll have to come up with a concrete type in both the cxx::bridge (in Rust)
                // and a corresponding typedef in C++.
                // First let's see if this actually depends on a generic type
                // param of the surrounding struct.
                for seg in &typ.path.segments {
                    if let PathArguments::AngleBracketed(args) = &seg.arguments {
                        for arg in args.args.iter() {
                            if let GenericArgument::Type(Type::Path(typ)) = arg {
                                if let Some(seg) = typ.path.segments.last() {
                                    if typ.path.segments.len() == 1
                                        && !ctx.allowed_generic_type(&seg.ident)
                                    {
                                        return Err(
                                            ConvertErrorFromCpp::ReferringToGenericTypeParam,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                // Let's second see if this is a concrete version of a templated type
                // which we already rejected. Some, but possibly not all, of the reasons
                // for its rejection would also apply to any concrete types we
                // make. Err on the side of caution. In future we may be able to relax
                // this a bit.
                let qn = QualifiedName::from_type_path(&typ); // ignores generic params
                if self.ignored_types.contains(&qn) {
                    return Err(ConvertErrorFromCpp::ConcreteVersionOfIgnoredTemplate);
                }
                // The concrete instantiation is named in C++ by writing its
                // arguments out again, and this branch does not convert them
                // first - so a `long double` argument would reach the header
                // as a literal `__bindgen_marker_LongDouble<double>` rather
                // than being turned down like one anywhere else.
                if mentions_long_double(&Type::Path(typ.clone())) {
                    return Err(ConvertErrorFromCpp::LongDouble);
                }
                if mentions_float128(&Type::Path(typ.clone())) {
                    return Err(ConvertErrorFromCpp::Float128);
                }
                // Same reasoning, and one step worse: writing the arguments out
                // again drops the qualifier rather than leaking a marker name,
                // so `Holder<volatile T>` would be named as `Holder<T>` - a
                // specialization C++ declared separately, or did not declare at
                // all. That compiles and is the wrong type. `std::array` is
                // turned down for this in bindgen, before autocxx sees it; this
                // covers every other template.
                if mentions_volatile(&Type::Path(typ.clone())) {
                    return Err(ConvertErrorFromCpp::VolatileTemplateArgument);
                }
                // A class template a `smart_pointer!` directive named becomes
                // the same opaque holder as the containers above, with the one
                // accessor that directive claims for it. The instantiation is
                // otherwise the concrete type autocxx already made here, so
                // everything which could be done with one still can. See
                // google/autocxx#670.
                if self.config.is_smart_pointer_template(&tn.to_cpp_name()) {
                    let surface = self.custom_ptr_surface(&tn, &typ, ns, &mut extra_apis)?;
                    return self.lower_to_holder(
                        typ,
                        Some(surface),
                        deps,
                        extra_apis,
                        target.is_const,
                    );
                }
                let (new_tn, api) = self.get_templated_typename(&Type::Path(typ))?;
                extra_apis.extend(api.into_iter());
                // Although it's tempting to remove the dep on the original type,
                // this means we wouldn't spot cases where the original type can't
                // be represented in C++, e.g. because it has an unused template parameter.
                // So we keep the original dep too.
                typ = new_tn.to_type_path();
                deps.insert(new_tn);
            }
        }
        Ok(Annotated::new(Type::Path(typ), deps, extra_apis, kind).marked_from(target))
    }

    fn get_generic_args(typ: &mut TypePath) -> Option<&mut PathSegment> {
        match typ.path.segments.last_mut() {
            Some(s) if !s.arguments.is_empty() => Some(s),
            _ => None,
        }
    }

    fn convert_punctuated<P>(
        &mut self,
        pun: Punctuated<GenericArgument, P>,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Punctuated<GenericArgument, P>>, ConvertErrorFromCpp>
    where
        P: Default,
    {
        let mut new_pun = Punctuated::new();
        let mut types_encountered = HashSet::new();
        let mut extra_apis = ApiVec::new();
        for arg in pun.into_iter() {
            new_pun.push(match arg {
                GenericArgument::Type(t) => {
                    let mut innerty = self.convert_type(t, ns, ctx)?;
                    types_encountered.extend(innerty.types_encountered.drain(..));
                    extra_apis.append(&mut innerty.extra_apis);
                    GenericArgument::Type(innerty.ty)
                }
                _ => arg,
            });
        }
        Ok(Annotated::new(
            new_pun,
            types_encountered,
            extra_apis,
            TypeKind::Regular,
        ))
    }

    /// Follow a chain of typedefs to what it eventually points at, along with
    /// what the analysis of the last typedef in the chain made of that target.
    ///
    /// The last is enough for the constness it also reports, even though C++
    /// applies `const` where it is written and no later typedef takes it off
    /// again. bindgen erases the qualifier from an alias whose target is
    /// itself an alias - `typedef int I; typedef const I CI;` comes out as
    /// `pub type CI = root::I;` with no marker on it - so the only alias which
    /// can arrive `const` is one naming a builtin, and that is where a chain
    /// ends. See `test_const_field_through_typedef_chain_deletes_default_constructor`.
    fn resolve_typedef<'b>(
        &'b self,
        tn: &QualifiedName,
    ) -> Result<Option<&'b TypedefTargetInfo>, ConvertErrorFromCpp> {
        let mut encountered = HashSet::new();
        let mut tn = tn.clone();
        let mut previous_typ = None;
        loop {
            let r = self.typedefs.get(&tn);
            match r.map(|target| &target.ty) {
                Some(Type::Path(typ)) => {
                    previous_typ = r;
                    let new_tn = QualifiedName::from_type_path(typ);
                    if encountered.contains(&new_tn) {
                        return Err(ConvertErrorFromCpp::InfinitelyRecursiveTypedef(tn.clone()));
                    }
                    if typ
                        .path
                        .segments
                        .iter()
                        .any(|seg| seg.ident.to_string().starts_with("_bindgen_mod"))
                    {
                        return Err(ConvertErrorFromCpp::TypedefToTypeInAnonymousNamespace);
                    }
                    encountered.insert(new_tn.clone());
                    tn = new_tn;
                }
                None => return Ok(previous_typ),
                _ => return Ok(r),
            }
        }
    }

    /// What to make of a C function pointer, which bindgen writes exactly as
    /// `Option<unsafe extern "C" fn(..)>` - the `Option` being there because a
    /// null pointer is one of the values one can hold. `None` if this is not
    /// one; otherwise the answer, which depends entirely on where it was
    /// found. See google/autocxx#1494.
    ///
    /// As struct field data it is taken as bindgen wrote it. Nothing in that
    /// names a C++ type, so there is nothing to convert, and copying the field
    /// copies a pointer - which is what lets a struct holding one be POD. A
    /// POD struct is re-exported from the bindgen module rather than declared
    /// to cxx, so its fields never have to be types cxx can express.
    ///
    /// The types in a signature do, and cxx has no function pointer type, so
    /// there it is refused. Saying so is the whole reason this arm exists:
    /// left to the ordinary path, the `Option` bindgen wrote gets read as a
    /// C++ type nobody has heard of and the user is told autocxx cannot
    /// support the built-in type `std::option::Option`, which is a Rust
    /// spelling of something their C++ never said.
    fn function_pointer(
        typ: &TypePath,
        ctx: &TypeConversionContext,
    ) -> Option<Result<Annotated<Type>, ConvertErrorFromCpp>> {
        unwrap_function_pointer(typ)?;
        Some(if ctx.within_struct_field() {
            Ok(Annotated::new(
                Type::Path(typ.clone()),
                HashSet::new(),
                ApiVec::new(),
                // It behaves as a pointer in every way the later analyses ask
                // about: a field holding one is left uninitialized by an
                // implicit default constructor, copied by an implicit copy
                // constructor, and destroyed by doing nothing at all.
                TypeKind::Pointer,
            ))
        } else {
            Err(ConvertErrorFromCpp::FunctionPointerInSignature)
        })
    }

    fn convert_ptr(
        &mut self,
        mut ptr: TypePtr,
        ns: &Namespace,
        ctx: &TypeConversionContext,
    ) -> Result<Annotated<Type>, ConvertErrorFromCpp> {
        Self::ensure_pointee_is_valid(&ptr, ctx)?;
        // Read before conversion, which peels the marker off the pointee.
        let pointee_is_volatile = is_volatile_qualified(&ptr.elem);
        let innerty = self.convert_boxed_type(ptr.elem, ns, &ctx.behind_reference())?;
        ptr.elem = innerty.ty;
        Ok(Annotated::new(
            Type::Ptr(ptr),
            innerty.types_encountered,
            innerty.extra_apis,
            TypeKind::Pointer,
        )
        .marked_volatile_pointee_if(pointee_is_volatile))
    }

    fn ensure_pointee_is_valid(
        ptr: &TypePtr,
        ctx: &TypeConversionContext,
    ) -> Result<(), ConvertErrorFromCpp> {
        match *ptr.elem {
            Type::Path(..) => Ok(()),
            Type::Array(..) => Err(ConvertErrorFromCpp::InvalidArrayPointee),
            // A pointer to a pointer is refused nearly everywhere because
            // there is no good way to hand one across the bridge: autocxx
            // would have to decide what the outer pointer's referent means
            // in Rust, and it has no way to know. Inside a struct field it
            // never has to decide - the field is data whose layout we copy
            // and whose contents Rust only ever sees as a raw pointer - so
            // the rejection would only stop the whole struct being generated
            // over a field nobody was going to dereference. See
            // https://github.com/google/autocxx/issues/1278. This permits one
            // level of nesting: the pointee is converted as
            // `WithinReference`, so `float***` is still refused.
            //
            // A typedef target is let through for a different reason: the
            // alias has no place of use yet, so there is nothing here to
            // decide. Whether `typedef float** M` is usable depends on where
            // `M` turns up, and every position which converts a type asks
            // again when it does, because the typedef arm of
            // `convert_type_path_which_is_not_a_reference` re-converts a
            // pointer target against that position's own context. The readers
            // which take the stored target without converting it want less
            // than that: the by-value checker only asks whether the outermost
            // thing is a pointer, dependency collection and
            // `replace_hopeless_typedef_targets` read the names it mentions,
            // and codegen emits a `pub use` of the alias. None of them puts the
            // pointer type into the [cxx::bridge]: a typedef is not declared
            // there, and the one route by which an alias reaches it - being
            // replaced by an opaque type, when a name it depends on was ignored
            // - carries the name and nothing of the target. Refusing it here
            // would instead throw the alias away at its definition and take
            // every use down with it, field or not.
            //
            // The other caller is the rvalue-reference arm of
            // `convert_type_path`, so `typedef float*&& R` is now stored rather
            // than refused. That is the same layout - one pointer to a `float*`
            // - which a field written `float*&& x` already gets through the
            // struct-field arm, and a use of the alias in a signature still
            // goes through `convert_ptr` and is still turned down.
            Type::Ptr(..)
                if matches!(
                    ctx,
                    TypeConversionContext::WithinStructField { .. }
                        | TypeConversionContext::WithinTypedef
                ) =>
            {
                Ok(())
            }
            Type::Ptr(..) => Err(ConvertErrorFromCpp::InvalidPointerPointee),
            _ => Err(ConvertErrorFromCpp::InvalidPointee(
                ptr.elem.to_token_stream().to_string(),
            )),
        }
    }

    /// Whether any of `typ`'s template arguments is `const`-qualified in its
    /// own right, as `std::shared_ptr<const T>`'s is.
    ///
    /// Only the argument's own qualifier counts, which is the one cxx has no
    /// room for; a `const` on something the argument points at is already in
    /// the type, as `*const T`.
    ///
    /// A record argument used to be invisible here: bindgen kept the marker
    /// only on an argument it rendered directly, and resolved a class
    /// argument's `TypeKind::ResolvedTypeRef` past the node the qualifier was
    /// on, so `std::shared_ptr<const Foo>` arrived indistinguishable from
    /// `std::shared_ptr<Foo>`.
    /// `third_party/patches/18-const-template-argument.patch` carries the
    /// qualifier across that resolution, so every argument C++ wrote `const`
    /// is one [`Self::arg_is_const_qualified`] can see.
    fn generic_args_are_const_qualified(
        &self,
        typ: &TypePath,
    ) -> Result<bool, ConvertErrorFromCpp> {
        let Some(PathArguments::AngleBracketed(args)) =
            typ.path.segments.last().map(|seg| &seg.arguments)
        else {
            return Ok(false);
        };
        for arg in args.args.iter() {
            if let GenericArgument::Type(Type::Path(inner)) = arg {
                if self.arg_is_const_qualified(inner)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Whether one template argument is `const`-qualified in its own right,
    /// however C++ spelt it.
    ///
    /// Written at the instantiation, the qualifier is bindgen's marker on the
    /// argument. Written on an alias it is not there at all: `typedef const
    /// int CI` comes out `pub type CI = __bindgen_marker_Const<c_int>`, and
    /// the argument of `std::shared_ptr<CI>` is the bare path `CI`. C++ makes
    /// no distinction between the two spellings - both name
    /// `std::shared_ptr<const int>` - so neither does this, and the alias is
    /// resolved by the same walk which reads the constness of an alias used
    /// anywhere else.
    ///
    /// A `const` alias whose target is a *record* or an enum is still
    /// invisible, and that gap is bindgen's: it erases the qualifier when it
    /// resolves the alias, which is the remaining gap recorded in
    /// `11-const-newtype-marker.patch`.
    fn arg_is_const_qualified(&self, arg: &TypePath) -> Result<bool, ConvertErrorFromCpp> {
        if unwrap_const(arg).is_some() {
            return Ok(true);
        }
        Ok(self
            .resolve_typedef(&QualifiedName::from_type_path(arg))?
            .is_some_and(|target| target.is_const))
    }

    /// The `T` of a `std::shared_ptr<const T>`, converted as the `cxx::bridge`
    /// would spell it, for the accessor the holder gets.
    ///
    /// `None` where the instantiation has no single `const`-qualified type
    /// argument to read - which is nothing the smart pointers can be written
    /// with today, but the pattern match rather than an `unwrap` is what keeps
    /// it that way. The holder is still generated in that case; it simply has
    /// no accessor.
    ///
    /// Where the qualifier is on an alias, the argument is handed to the
    /// conversion as it stands: naming the alias is what carries the `const`,
    /// and converting the alias is what resolves it. Where it is written at
    /// the instantiation, bindgen's marker is peeled off first, because
    /// nothing downstream knows what to do with one.
    fn convert_const_payload(
        &mut self,
        typ: &TypePath,
        ns: &Namespace,
    ) -> Result<Option<Annotated<Type>>, ConvertErrorFromCpp> {
        let Some(PathArguments::AngleBracketed(args)) =
            typ.path.segments.last().map(|seg| &seg.arguments)
        else {
            return Ok(None);
        };
        let [GenericArgument::Type(Type::Path(payload))] =
            args.args.iter().collect::<Vec<_>>().as_slice()
        else {
            return Ok(None);
        };
        let payload = match unwrap_const(payload) {
            Some(inner) => inner.clone(),
            None if self.arg_is_const_qualified(payload)? => Type::Path((*payload).clone()),
            None => return Ok(None),
        };
        self.convert_type(payload, ns, &TypeConversionContext::WithinContainer)
            .map(Some)
    }

    /// Which accessors the holder for a `const`-payload smart pointer gets.
    ///
    /// `None` where the payload is not a single type the bridge can name in
    /// one - which is nothing these three can be written with today, but the
    /// pattern match rather than an `unwrap` is what keeps it that way. The
    /// holder is generated either way, and can be handed back to C++ either
    /// way; only the accessors depend on this.
    ///
    /// The names the payload's conversion met are recorded on the holder
    /// rather than on whatever is being converted, because it is the holder's
    /// accessors which name them: a function handling one of these names the
    /// holder and nothing else. `deps.rs` reads them back off `HolderSurface`,
    /// so the garbage collector reaches the payload through the holder and an
    /// ignored payload takes the holder with it. Both matter under
    /// `generate_all!`, where a holder is a garbage-collection root and
    /// survives whether or not anything names it.
    fn const_smart_pointer_surface(
        &mut self,
        tn: &QualifiedName,
        typ: &TypePath,
        ns: &Namespace,
        extra_apis: &mut ApiVec<NullPhase>,
    ) -> Result<Option<HolderSurface>, ConvertErrorFromCpp> {
        let Some(mut payload) = self.convert_const_payload(typ, ns)? else {
            return Ok(None);
        };
        extra_apis.append(&mut payload.extra_apis);
        let ty = Box::new(payload.ty.into());
        let deps = payload.types_encountered;
        Ok(match tn.to_cpp_name().as_str() {
            "std::shared_ptr" => Some(HolderSurface::SharedPtr { payload: ty, deps }),
            "std::unique_ptr" => Some(HolderSurface::UniquePtr { payload: ty, deps }),
            "std::weak_ptr" => {
                // `lock` answers with a `std::shared_ptr<const T>`, so that
                // holder has to exist for this one to have a surface at all -
                // and the header need never have mentioned the specialization.
                // Manufacture it here, by the same route a header which did
                // mention it would take, so that the two share one holder
                // whichever came first.
                let shared_holder = self.manufacture_holder(
                    sibling_shared_ptr(typ),
                    Some(HolderSurface::SharedPtr { payload: ty, deps }),
                    extra_apis,
                )?;
                Some(HolderSurface::WeakPtr {
                    deps: std::iter::once(shared_holder.clone()).collect(),
                    shared_holder,
                })
            }
            // Nothing else has `CxxGenericType::CppPtr` behavior, so nothing
            // else reaches this - but a fourth smart pointer would arrive here
            // with no accessors rather than with the wrong ones.
            _ => None,
        })
    }

    /// The accessor surface of an instantiation of a class template a
    /// `smart_pointer!` directive named: `get`, over the first template
    /// argument.
    ///
    /// The argument is read here in two spellings, because the two halves of
    /// the shim need different ones. The `cxx::bridge` needs the Rust type, so
    /// the argument is converted like any other payload - which is also what
    /// records the names the holder then depends on, and what may manufacture a
    /// type of its own for a nested instantiation. The C++ shim needs the C++
    /// type, because what it hands back is a pointer to the argument and a
    /// user's template promises no `element_type` typedef to name one with.
    ///
    /// A `const` argument is kept as such in both: `MyPtr<const T>::get`
    /// returns a `const T*`, so the shim has to be declared returning one and
    /// Rust has to be handed a `*const T`. The qualifier travels in the C++
    /// spelling by itself; the Rust side has it peeled off first, since nothing
    /// downstream knows what to do with bindgen's marker, and
    /// [`Self::arg_is_const_qualified`] is what sees it where an alias carried
    /// it instead.
    ///
    /// It is the *first* type argument, not the first one which happens to be a
    /// named type: `MyPtr<int*, Tag>` points at an `int*`, and picking the
    /// argument which can be handled would give it a `Tag*` accessor for a
    /// `Tag` it has nothing to do with. An argument which is not a named type
    /// is refused instead.
    fn custom_ptr_surface(
        &mut self,
        tn: &QualifiedName,
        typ: &TypePath,
        ns: &Namespace,
        extra_apis: &mut ApiVec<NullPhase>,
    ) -> Result<HolderSurface, ConvertErrorFromCpp> {
        let argument = typ
            .path
            .segments
            .last()
            .and_then(|seg| match &seg.arguments {
                PathArguments::AngleBracketed(ab) => ab.args.iter().find_map(|arg| match arg {
                    GenericArgument::Type(ty) => Some(ty),
                    _ => None,
                }),
                _ => None,
            })
            // Nothing gets this far with no type argument at all: a template
            // whose parameters are values rather than types is one bindgen
            // renders as a blob of bytes, and a signature mentioning that is
            // refused before any of this. Said rather than unwrapped, so that a
            // shape which does reach here is named.
            .ok_or_else(|| ConvertErrorFromCpp::SmartPointerWithoutTypeArgument(tn.clone()))?;
        let Type::Path(argument) = argument else {
            return Err(ConvertErrorFromCpp::SmartPointerPayloadNotANamedType(
                tn.clone(),
                argument.to_token_stream().to_string(),
            ));
        };
        let payload_is_const = self.arg_is_const_qualified(argument)?;
        let stripped = match unwrap_const(argument) {
            Some(inner) => inner.clone(),
            None => Type::Path(argument.clone()),
        };
        // Asked again of what the marker wrapped: `MyPtr<int* const>` arrives
        // as a path, bindgen's `const` marker being one, and what it qualifies
        // is the pointer the check above turns down.
        if !matches!(stripped, Type::Path(_)) {
            return Err(ConvertErrorFromCpp::SmartPointerPayloadNotANamedType(
                tn.clone(),
                stripped.to_token_stream().to_string(),
            ));
        }
        let mut payload =
            self.convert_type(stripped, ns, &TypeConversionContext::WithinContainer)?;
        extra_apis.append(&mut payload.extra_apis);
        Ok(HolderSurface::CustomPtr {
            payload: Box::new(payload.ty.into()),
            payload_cpp: Box::new(Type::Path(argument.clone()).into()),
            payload_is_const,
            deps: payload.types_encountered,
        })
    }

    /// Divert a template instantiation cxx cannot spell to the opaque C++
    /// holder autocxx already manufactures for such things, with `surface`
    /// saying which accessors the holder gets.
    ///
    /// `typ` is the instantiation before this conversion substitutes cxx's own
    /// container names, because it is the C++ spelling of the template - not
    /// cxx's - that the generated typedef has to name. Usually that means
    /// bindgen's spelling; where the instantiation was reached through an
    /// alias it is what the alias stored, which may be a substitution some
    /// earlier conversion made. `type_to_cpp` maps both back to C++, and
    /// [`sibling_shared_ptr`] is where the difference matters.
    fn lower_to_holder(
        &mut self,
        typ: TypePath,
        surface: Option<HolderSurface>,
        mut deps: HashSet<QualifiedName>,
        mut extra_apis: ApiVec<NullPhase>,
        target_is_const: bool,
    ) -> Result<Annotated<Type>, ConvertErrorFromCpp> {
        let new_tn = self.manufacture_holder(typ, surface, &mut extra_apis)?;
        deps.insert(new_tn.clone());
        Ok(Annotated::new(
            Type::Path(new_tn.to_type_path()),
            deps,
            extra_apis,
            TypeKind::Regular,
        )
        .marked_const_if(target_is_const))
    }

    /// The opaque holder for `typ`, made if this is the first time this
    /// instantiation has been seen and found if it is not, and its name.
    ///
    /// A holder carries the accessors it was created with, and everything
    /// which arrives at the same C++ specialization afterwards shares that one
    /// holder. Where the holder was made here, `surface` goes straight onto it.
    /// Where it already existed it is remembered instead, for
    /// [`Self::take_deferred_surfaces`]: an instantiation which a `concrete!`
    /// directive named is registered before any conversion runs, so the
    /// conversion which knows what accessors it should have finds it already
    /// made and has nothing of its own to put them on.
    fn manufacture_holder(
        &mut self,
        typ: TypePath,
        surface: Option<HolderSurface>,
        extra_apis: &mut ApiVec<NullPhase>,
    ) -> Result<QualifiedName, ConvertErrorFromCpp> {
        let (new_tn, api) = self.get_templated_typename(&Type::Path(typ))?;
        match api {
            Some(Api::ConcreteType {
                name,
                rs_definition,
                cpp_definition,
                incomplete_argument,
                ..
            }) => extra_apis.push(Api::ConcreteType {
                name,
                rs_definition,
                cpp_definition,
                holder_surface: surface,
                constructor_and_allocator_deps: Vec::new(),
                incomplete_argument,
            }),
            _ => {
                if let Some(surface) = surface {
                    self.deferred_surfaces
                        .entry(new_tn.clone())
                        .or_insert(surface);
                }
            }
        }
        Ok(new_tn)
    }

    /// The accessor surfaces worked out for holders which already existed, to
    /// be put onto them by [`crate::conversion::analysis::fun::FnAnalyzer`]
    /// once conversion is over. Each is the surface of the first conversion
    /// which asked for one, which is the rule a holder made here follows too.
    pub(crate) fn take_deferred_surfaces(&mut self) -> HashMap<QualifiedName, HolderSurface> {
        std::mem::take(&mut self.deferred_surfaces)
    }

    fn get_templated_typename(
        &mut self,
        rs_definition: &Type,
    ) -> Result<(QualifiedName, Option<UnanalyzedApi>), ConvertErrorFromCpp> {
        // An instantiation is named in C++ by writing its arguments out again,
        // and `type_to_cpp` writes an array as `std::array<T, N>` - which is
        // what one is everywhere a signature can hold it, since
        // `check_signature_array` lets no other array through. A template
        // argument is the one position C++ can put a real array in, and
        // `Foo<uint8_t[2]>` would be named as `Foo<std::array<uint8_t, 2>>`, a
        // specialization nobody instantiated. Neither spelling is worth
        // telling apart: both were refused outright until now, because
        // `type_to_cpp` had no array at all.
        //
        // Asked here rather than at either caller, because both reach C++
        // through this: the branch below which invents a concrete type for a
        // template cxx cannot spell, and `manufacture_holder`, whose
        // `std::shared_ptr<const T>` payload may be an array too.
        if mentions_cpp_array(rs_definition) {
            return Err(ConvertErrorFromCpp::CppArrayInTemplateArgument(
                rs_definition.to_token_stream().to_string(),
            ));
        }
        // Refused here rather than left to rustc, which would report an
        // unsatisfied bound on a generated trait naming neither the type the
        // user wrote nor the member which needs it.
        if let Some((argument, inner_type)) = self.unsatisfiable_inner_type(rs_definition) {
            let instantiation = match rs_definition {
                Type::Path(typ) => QualifiedName::from_type_path(typ),
                _ => unreachable!("only a path has template arguments to be refused over"),
            };
            return Err(ConvertErrorFromCpp::DependentQualifiedTypeOnSubstitute {
                instantiation,
                argument,
                inner_type,
            });
        }
        let count = self.concrete_templates.len();
        // We just use this as a hash key, essentially.
        let cpp_definition = self.original_name_map.type_to_cpp(rs_definition)?;
        let e = self.concrete_templates.get(&cpp_definition);
        match e {
            Some(tn) => Ok((tn.clone(), None)),
            None => {
                let synthetic_ident = concrete_type_ident(&cpp_definition);
                // Ensure we're not duplicating some existing concrete template name.
                // If so, we'll invent a name which is guaranteed to be unique.
                let synthetic_ident = match self
                    .concrete_templates
                    .values()
                    .map(|n| n.get_final_item())
                    .find(|s| s == &synthetic_ident)
                {
                    None => synthetic_ident,
                    Some(_) => format!("AutocxxConcrete{count}"),
                };
                // The arguments are read here, before anything converts them,
                // because this is the only place they are looked at at all: a
                // later phase finds this instantiation by name in
                // `concrete_templates` and never sees what it was built from.
                let incomplete_argument = self.incomplete_argument_of(rs_definition);
                let api = UnanalyzedApi::ConcreteType {
                    name: ApiName::new_in_root_namespace(make_ident(synthetic_ident)),
                    cpp_definition: cpp_definition.clone(),
                    rs_definition: Some(Box::new(rs_definition.clone().into())),
                    holder_surface: None,
                    constructor_and_allocator_deps: Vec::new(),
                    incomplete_argument: incomplete_argument.clone(),
                };
                if let Some(argument) = incomplete_argument {
                    self.instantiations_on_incomplete_types
                        .insert(api.name().clone(), argument);
                }
                self.concrete_templates
                    .insert(cpp_definition, api.name().clone());
                Ok((api.name().clone(), Some(api)))
            }
        }
    }

    /// The first template argument of `rs_definition` which names a type we
    /// have only a stand-in for, if any.
    ///
    /// Only arguments named by value count. A pointer or a reference to an
    /// incomplete type is a complete type itself, and a template which keeps
    /// one - `holder<at> { at* p; }` - destroys perfectly well; it is the
    /// `std::unique_ptr<at>` kind of member which does not.
    ///
    /// This is a conservative rule rather than a theorem: a template which
    /// holds its argument by value may still be destructible where the
    /// argument is not, and one which does not may still be undestructible for
    /// reasons of its own. It is the shape which matters in practice and the
    /// most that can be decided from a template's arguments alone.
    ///
    /// A class which holds one of these by value keeps the member either way:
    /// turning it down would only hide it from the analysis which works out
    /// what constructors that class has. Whether the class itself may be
    /// destroyed is decided separately, by
    /// [`Self::find_classes_we_may_not_destroy`].
    fn incomplete_argument_of(&self, rs_definition: &Type) -> Option<QualifiedName> {
        self.incomplete_argument_within(rs_definition, &mut HashSet::new())
    }

    /// [`Self::incomplete_argument_of`], carrying the names already looked at
    /// so that a chain of aliases cannot walk in a circle.
    fn incomplete_argument_within(
        &self,
        ty: &Type,
        seen: &mut HashSet<QualifiedName>,
    ) -> Option<QualifiedName> {
        let Type::Path(typ) = ty else {
            return None;
        };
        typ.path
            .segments
            .iter()
            .filter_map(|seg| match &seg.arguments {
                PathArguments::AngleBracketed(ab) => Some(ab.args.iter()),
                _ => None,
            })
            .flatten()
            .find_map(|arg| match arg {
                GenericArgument::Type(inner) => self.incompleteness_of_argument(inner, seen),
                _ => None,
            })
    }

    /// Whether one template argument reaches a type nothing defines - as
    /// itself, through the aliases it may be written as, or inside its own
    /// arguments.
    ///
    /// The aliases matter because this runs before anything converts these
    /// arguments, so they are here under whatever name the header wrote:
    /// `au<Alias>`, where `using Alias = bb`, is the same instantiation as
    /// `au<bb>` and used to be classified as if it were not. An alias for an
    /// instantiation which was itself classified this way - `using Inner =
    /// au<bb>` - is caught through the same lookup, because a typedef's
    /// recorded target is what its own conversion made of it.
    fn incompleteness_of_argument(
        &self,
        arg: &Type,
        seen: &mut HashSet<QualifiedName>,
    ) -> Option<QualifiedName> {
        let Type::Path(typ) = arg else {
            return None;
        };
        let qn = QualifiedName::from_type_path(typ);
        if self.forward_declarations.contains_key(&qn) {
            return Some(qn);
        }
        if let Some(argument) = self.instantiations_on_incomplete_types.get(&qn) {
            return Some(argument.clone());
        }
        // `seen` gates only this, the one step which leaves the type in hand
        // for another. Walking the arguments below is walking a finite piece
        // of syntax, and a name repeating in it - `Pair<au<int>, au<bb>>`, or
        // `au<au<bb>>` - is not a circle. `QualifiedName` drops the arguments,
        // so barring a name here would bar the second `au` in each of those
        // and lose the `bb` inside it. Barring the second expansion of one
        // alias loses nothing: the map is fixed and nothing substitutes into
        // what it holds, so the expansion already under way is walking the
        // same target, and whatever it finds comes back through the caller
        // which started it.
        if let Some(target) = self.alias_targets.get(&qn) {
            if seen.insert(qn.clone()) {
                if let Some(found) = self.incompleteness_of_argument(target, seen) {
                    return Some(found);
                }
            }
        }
        self.incomplete_argument_within(arg, seen)
    }

    fn confirm_inner_type_is_acceptable_generic_payload(
        &self,
        path_args: &Punctuated<GenericArgument, Comma>,
        desc: &QualifiedName,
        generic_behavior: CxxGenericType,
        forward_declarations_ok: bool,
    ) -> Result<TypeKind, ConvertErrorFromCpp> {
        for inner in path_args {
            match inner {
                GenericArgument::Type(Type::Path(typ)) => {
                    let inner_qn = QualifiedName::from_type_path(typ);
                    if !forward_declarations_ok {
                        if self.forward_declarations.contains_key(&inner_qn) {
                            return Err(self.incomplete_type_error(inner_qn));
                        }
                        if let Some(err) = self.instantiation_on_incomplete_type_error(&inner_qn) {
                            return Err(err);
                        }
                        if let Some(err) = self.undestroyable_member_error(&inner_qn) {
                            return Err(err);
                        }
                    }
                    match generic_behavior {
                        CxxGenericType::Rust => {
                            if !inner_qn.get_namespace().is_empty() {
                                return Err(ConvertErrorFromCpp::RustTypeWithAPath(inner_qn));
                            }
                            if !self.config.is_rust_type(&inner_qn.get_final_ident()) {
                                return Err(ConvertErrorFromCpp::BoxContainingNonRustType(
                                    inner_qn,
                                ));
                            }
                            if self
                                .config
                                .is_subclass_holder(&inner_qn.get_final_ident().to_string())
                            {
                                return Ok(TypeKind::SubclassHolder(inner_qn.get_final_ident()));
                            } else {
                                return Ok(TypeKind::Regular);
                            }
                        }
                        CxxGenericType::CppUniquePtr => {
                            if !known_types().permissible_within_unique_ptr(&inner_qn) {
                                return Err(ConvertErrorFromCpp::InvalidTypeForCppPtr(inner_qn));
                            }
                        }
                        CxxGenericType::CppSharedPtr => {
                            if !known_types().permissible_within_shared_or_weak_ptr(&inner_qn) {
                                return Err(ConvertErrorFromCpp::InvalidTypeForCppPtr(inner_qn));
                            }
                        }
                        CxxGenericType::CppVector => {
                            if !known_types().permissible_within_vector(&inner_qn) {
                                return Err(ConvertErrorFromCpp::InvalidTypeForCppVector(inner_qn));
                            }
                            if matches!(
                                typ.path.segments.last().map(|ps| &ps.arguments),
                                Some(
                                    PathArguments::Parenthesized(_)
                                        | PathArguments::AngleBracketed(_)
                                )
                            ) {
                                return Err(ConvertErrorFromCpp::GenericsWithinVector);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {
                    return Err(ConvertErrorFromCpp::TemplatedTypeContainingNonPathArg(
                        desc.clone(),
                    ))
                }
            }
        }
        Ok(TypeKind::Regular)
    }

    fn find_typedefs<A: AnalysisPhase>(
        apis: &ApiVec<A>,
    ) -> HashMap<QualifiedName, TypedefTargetInfo>
    where
        A::TypedefAnalysis: TypedefTarget,
    {
        apis.iter()
            .filter_map(|api| match &api {
                Api::Typedef { analysis, .. } => analysis
                    .get_target()
                    .map(|target| (api.name().clone(), target)),
                _ => None,
            })
            .collect()
    }

    fn find_concrete_templates<A: AnalysisPhase>(
        apis: &ApiVec<A>,
    ) -> HashMap<String, QualifiedName> {
        apis.iter()
            .filter_map(|api| match &api {
                Api::ConcreteType { cpp_definition, .. } => {
                    Some((cpp_definition.clone(), api.name().clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn find_incomplete_types<A: AnalysisPhase>(
        apis: &ApiVec<A>,
    ) -> HashMap<QualifiedName, Option<OpaqueTypedefReason>> {
        apis.iter()
            .filter_map(|api| match api {
                Api::ForwardDeclaration { .. } => Some((api.name().clone(), None)),
                Api::OpaqueTypedef {
                    forward_declaration: true,
                    reason,
                    ..
                } => Some((api.name().clone(), reason.clone())),
                _ => None,
            })
            .collect()
    }

    /// What every alias in `apis` was written as pointing at, as bindgen wrote
    /// it. See the field of the same name.
    fn find_alias_targets<A: AnalysisPhase>(apis: &ApiVec<A>) -> HashMap<QualifiedName, Type> {
        apis.iter()
            .filter_map(|api| match api {
                Api::Typedef {
                    item: TypedefKind::Type(ity),
                    ..
                } => Some((api.name().clone(), (*ity.ty).clone())),
                Api::Typedef {
                    item: TypedefKind::Use(ty),
                    ..
                } => Some((api.name().clone(), (**ty).clone().into())),
                _ => None,
            })
            .collect()
    }

    /// What each generic type bindgen emitted requires of its template
    /// parameters, read off the bound bindgen put on each. See the field of the
    /// same name.
    fn find_inner_types_required<A: AnalysisPhase>(
        apis: &ApiVec<A>,
    ) -> HashMap<QualifiedName, Vec<Vec<String>>> {
        apis.iter()
            .filter_map(|api| match api {
                Api::Struct { details, .. } => {
                    let required = inner_types_required_of_params(&details.item.generics);
                    required
                        .iter()
                        .any(|inner_types| !inner_types.is_empty())
                        .then(|| (api.name().clone(), required))
                }
                _ => None,
            })
            .collect()
    }

    /// The inner type an instantiation's argument is required to have and does
    /// not, where autocxx is the reason it does not.
    ///
    /// bindgen implements the trait for each type which declares such an inner
    /// type, so a type it describes in full carries whatever C++ gave it. A
    /// type autocxx replaces carries what the prelude class declares instead,
    /// which is a shorter list, and the difference is the bound nothing can
    /// satisfy - a rustc error deep in the generated bindings, in place of
    /// anything naming the type the user wrote.
    fn unsatisfiable_inner_type(&self, rs_definition: &Type) -> Option<(QualifiedName, String)> {
        let Type::Path(typ) = rs_definition else {
            return None;
        };
        let required = self
            .inner_types_required
            .get(&QualifiedName::from_type_path(typ))?;
        let arguments = typ
            .path
            .segments
            .last()
            .into_iter()
            .filter_map(|seg| match &seg.arguments {
                PathArguments::AngleBracketed(ab) => Some(ab.args.iter()),
                _ => None,
            })
            .flatten()
            // Every type argument, in the order written. `required` is indexed
            // by the position of a type parameter, so dropping the arguments
            // which are not paths here would pair each of the rest with some
            // other parameter's bounds.
            .filter_map(|arg| match arg {
                GenericArgument::Type(inner) => Some(inner),
                _ => None,
            });
        for (argument, inner_types) in arguments.zip(required) {
            let Type::Path(argument) = argument else {
                continue;
            };
            let argument = QualifiedName::from_type_path(argument);
            for inner_type in inner_types {
                if known_types().substitute_declares_inner_type(&argument, inner_type)
                    == Some(false)
                {
                    return Some((argument, inner_type.clone()));
                }
            }
        }
        None
    }

    /// The concrete template instantiations which were built on a type nothing
    /// defines, recovered from the `Api`s a previous phase left behind.
    ///
    /// [`Self::get_templated_typename`] works this out once, when it
    /// manufactures the instantiation; every later phase reads it back from
    /// here, because the instantiation is found in `concrete_templates` by
    /// then and its arguments are never looked at again.
    fn find_instantiations_on_incomplete_types<A: AnalysisPhase>(
        apis: &ApiVec<A>,
    ) -> HashMap<QualifiedName, QualifiedName> {
        apis.iter()
            .filter_map(|api| match api {
                Api::ConcreteType {
                    incomplete_argument: Some(argument),
                    ..
                } => Some((api.name().clone(), argument.clone())),
                _ => None,
            })
            .collect()
    }

    /// What each concrete template instantiation was built from, so that its
    /// arguments can be read back from its flat synthesized name.
    fn find_concrete_definitions<A: AnalysisPhase>(
        apis: &ApiVec<A>,
    ) -> HashMap<QualifiedName, Type> {
        apis.iter()
            .filter_map(|api| match api {
                Api::ConcreteType {
                    rs_definition: Some(rs_definition),
                    ..
                } => Some((api.name().clone(), (**rs_definition).clone().into())),
                _ => None,
            })
            .collect()
    }

    /// Why `qn` may not be destroyed here, if it may not be: it is a template
    /// instantiation built on a type nothing defines.
    fn instantiation_on_incomplete_type_error(
        &self,
        qn: &QualifiedName,
    ) -> Option<ConvertErrorFromCpp> {
        self.instantiations_on_incomplete_types.get(qn).map(|arg| {
            ConvertErrorFromCpp::InstantiationOnIncompleteType {
                instantiation: qn.clone(),
                argument: arg.clone(),
            }
        })
    }

    /// What to tell whoever tried to use `qn`, which we have only a stand-in
    /// for. Where we know what was wrong with the thing it stands in for, that
    /// is the useful answer; otherwise all we can say is that it's incomplete.
    fn incomplete_type_error(&self, qn: QualifiedName) -> ConvertErrorFromCpp {
        match self.forward_declarations.get(&qn) {
            Some(Some(OpaqueTypedefReason { culprit, reason })) => {
                ConvertErrorFromCpp::TypeContainingUngeneratableTypedef {
                    name: qn,
                    culprit: culprit.clone(),
                    reason: reason.clone(),
                }
            }
            _ => ConvertErrorFromCpp::TypeContainingForwardDeclaration(qn),
        }
    }

    fn find_ignored_types<A: AnalysisPhase>(apis: &ApiVec<A>) -> HashSet<QualifiedName> {
        apis.iter()
            .filter_map(|api| match api {
                Api::IgnoredItem { .. } => Some(api.name()),
                _ => None,
            })
            .cloned()
            .collect()
    }
}

/// `typ`'s one template argument, where it is a raw pointer, as
/// `std::vector<T*>`'s is.
///
/// A pointer is one of the arguments bindgen renders directly rather than
/// through a type reference, so `std::vector<Foo*>` arrives as
/// `vector<*mut Foo>` with the pointer intact - see
/// [`TypeConverter::arg_is_const_qualified`] for the same mechanism and where
/// it stops. `const Foo*` needs nothing extra from the marker to survive: it
/// arrives as `*const Foo`, because pointee constness is part of a Rust type
/// already.
///
/// One argument, because that is all a `std::vector` ever reaches us with:
/// bindgen erases the allocator whether or not the header let it default, so
/// `std::vector<Foo*, MyAlloc<Foo*>>` arrives as `vector<*mut Foo>` too and is
/// lowered to a holder whose typedef names the default-allocator
/// specialization. C++ then refuses to build the wrapper - which is exactly
/// what `std::vector<Foo, MyAlloc<Foo>>` already does without any of this, the
/// erasure being bindgen's and older than the lowering. So the match here is
/// what keeps a single pointer argument the only shape that arrives, and not
/// what decides which allocators are in reach.
/// The template arguments `typ` was written with, as types, one level deep.
fn direct_generic_args(typ: &TypePath) -> Vec<Type> {
    let Some(PathArguments::AngleBracketed(args)) =
        typ.path.segments.last().map(|seg| &seg.arguments)
    else {
        return Vec::new();
    };
    args.args
        .iter()
        .filter_map(|arg| match arg {
            GenericArgument::Type(ty) => Some(ty.clone()),
            _ => None,
        })
        .collect()
}

fn sole_pointer_generic_arg(typ: &TypePath) -> Option<Type> {
    let PathArguments::AngleBracketed(args) = &typ.path.segments.last()?.arguments else {
        return None;
    };
    match args.args.iter().collect::<Vec<_>>().as_slice() {
        [GenericArgument::Type(elem @ Type::Ptr(_))] => Some((*elem).clone()),
        _ => None,
    }
}

/// The `std::shared_ptr<const T>` beside a `std::weak_ptr<const T>`: what
/// `std::weak_ptr::lock` answers with, and so what the weak holder's shim has
/// to return a holder of.
///
/// Only the template arguments are taken from `typ`; the container is named
/// afresh from what autocxx knows `std::shared_ptr` by, rather than by
/// renaming a segment. The path reaching the lowering is not always bindgen's:
/// where the header wrote `using W = std::weak_ptr<CI>`, the typedef was
/// analysed before any typedef target could be resolved, so the `const` on
/// `CI` was invisible then, nothing was lowered, and what the alias stored was
/// cxx's own substituted spelling - `cxx::WeakPtr<CI>`. Renaming the last
/// segment of that produces `cxx::shared_ptr`, which autocxx knows nothing
/// about and which reached the generated C++ verbatim, as
/// `typedef cxx::shared_ptr<CI> ...`.
fn sibling_shared_ptr(typ: &TypePath) -> TypePath {
    let mut sibling = QualifiedName::new_from_cpp_name("std::shared_ptr").to_type_path();
    if let (Some(last), Some(original)) =
        (sibling.path.segments.last_mut(), typ.path.segments.last())
    {
        last.arguments = original.arguments.clone();
    }
    sibling
}

/// Processing functions sometimes results in new types being materialized.
/// These types haven't been through the analysis phases (chicken and egg
/// problem) but fortunately, don't need to. We need to keep the type
/// system happy by adding an [ApiAnalysis] but in practice, for the sorts
/// of things that get created, it's always blank.
/// Put onto each holder the accessor surface a conversion worked out for it
/// after it already existed.
///
/// A holder made during conversion carries its surface from the moment it is
/// made. One which was already there does not, and there is exactly one way to
/// be already there: a `concrete!` directive registers an instantiation before
/// any conversion runs. The conversion which then meets that instantiation in a
/// signature is the one which knows what accessors it should have - it has the
/// template arguments in front of it - but it finds the type made and has
/// nothing of its own to put them on. This is where the two meet.
///
/// A holder which already has a surface keeps it, which is the rule
/// [`TypeConverter::manufacture_holder`] follows for the holders it makes.
pub(crate) fn attach_deferred_holder_surfaces<P: AnalysisPhase>(
    type_converter: &mut TypeConverter,
    apis: ApiVec<P>,
) -> ApiVec<P> {
    let mut surfaces = type_converter.take_deferred_surfaces();
    if surfaces.is_empty() {
        return apis;
    }
    apis.into_iter()
        .map(|api| match api {
            Api::ConcreteType {
                name,
                rs_definition,
                cpp_definition,
                holder_surface: None,
                constructor_and_allocator_deps,
                incomplete_argument,
            } => {
                let holder_surface = surfaces.shift_remove(&name.name);
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

pub(crate) fn add_analysis<A: AnalysisPhase>(api: UnanalyzedApi) -> Api<A> {
    match api {
        Api::ConcreteType {
            name,
            rs_definition,
            cpp_definition,
            holder_surface,
            constructor_and_allocator_deps,
            incomplete_argument,
        } => Api::ConcreteType {
            name,
            rs_definition,
            cpp_definition,
            holder_surface,
            constructor_and_allocator_deps,
            incomplete_argument,
        },
        Api::IgnoredItem { name, err, ctx } => Api::IgnoredItem { name, err, ctx },
        _ => panic!("Function analysis created an unexpected type of extra API"),
    }
}
pub(crate) trait TypedefTarget {
    fn get_target(&self) -> Option<TypedefTargetInfo>;
}

impl TypedefTarget for () {
    fn get_target(&self) -> Option<TypedefTargetInfo> {
        None
    }
}

impl TypedefTarget for TypedefAnalysis {
    fn get_target(&self) -> Option<TypedefTargetInfo> {
        Some(TypedefTargetInfo {
            ty: match self.kind {
                TypedefKind::Type(ref ty) => (*ty.ty).clone(),
                TypedefKind::Use(ref ty) => (***ty).clone(),
            },
            kind: self.target_kind.clone(),
            is_const: self.target_is_const,
            is_std_array: self.target_is_std_array,
        })
    }
}

pub(crate) fn find_types<A: AnalysisPhase>(apis: &ApiVec<A>) -> HashSet<QualifiedName> {
    apis.iter()
        .filter_map(|api| match api {
            Api::ForwardDeclaration { .. }
            | Api::OpaqueTypedef { .. }
            | Api::ConcreteType { .. }
            | Api::Typedef { .. }
            | Api::Enum { .. }
            | Api::Struct { .. }
            | Api::Subclass { .. }
            | Api::ExternCppType { .. }
            | Api::RustType { .. } => Some(api.name()),
            Api::StringConstructor { .. }
            | Api::Function { .. }
            | Api::Const { .. }
            | Api::Static { .. }
            | Api::CType { .. }
            | Api::RustSubclassFn { .. }
            | Api::IgnoredItem { .. }
            | Api::SubclassTraitItem { .. }
            | Api::RustFn { .. } => None,
        })
        .cloned()
        .collect()
}

/// The name bindgen gave each of a container's payloads, before conversion
/// resolved any alias among them.
///
/// Read at that moment because it is the only one at which an alias can be
/// told from what it resolves to, and that matters because bindgen drops a
/// `const` off an alias's target: `using CU32 = const uint32_t` reaches us as
/// `pub type CU32 = u32`, with nothing anywhere saying the payload is really a
/// `const uint32_t`. Both callers below need to know.
fn payload_names_as_written(
    args: &Punctuated<GenericArgument, Comma>,
) -> Vec<Option<QualifiedName>> {
    args.iter()
        .map(|arg| match arg {
            GenericArgument::Type(Type::Path(payload)) => {
                Some(QualifiedName::from_type_path(payload))
            }
            _ => None,
        })
        .collect()
}

/// Whether this payload is one bindgen wrote itself, rather than the target an
/// alias of its own resolved to. See [`payload_names_as_written`].
fn payload_was_written_verbatim(
    arg: &GenericArgument,
    as_written: Option<&Option<QualifiedName>>,
) -> bool {
    match (arg, as_written) {
        (GenericArgument::Type(Type::Path(payload)), Some(Some(written))) => {
            *written == QualifiedName::from_type_path(payload)
        }
        _ => false,
    }
}

/// Name a `std::unique_ptr`'s payloads with the `autocxx::c_*` wrapper of the
/// same C++ type, where cxx will not take the payload's own name.
///
/// cxx turns down a `unique_ptr` of any of its own atoms, so
/// `std::unique_ptr<uint32_t>` has no cxx spelling; `autocxx::c_u32` is that
/// same `uint32_t` under a name cxx treats as any other, and the explicit shim
/// trait impls in `autocxx::c_type_vectors` give it the `UniquePtrTarget` cxx
/// would otherwise be missing. The wrapper's own name is recorded as a
/// dependency so that the generated C++ declares its typedef. See
/// google/autocxx#422.
///
/// Only a payload bindgen wrote as the atom itself is renamed: one reached
/// through an alias may be a `const uint32_t` whose qualifier bindgen dropped,
/// and naming that with the wrapper would declare a `unique_ptr` of a mutable
/// one, which the shim would fail to bind - costing the whole bridge instead
/// of the one function.
///
/// The character types are not caught up in this and need not be: bindgen
/// gives `char32_t`, `wchar_t` and their siblings markers of their own, so
/// none of them arrives as a bare atom, and each is already a named type to
/// cxx with container glue of its own. `char8_t` is the exception, refused by
/// the payload predicate for want of that glue - see
/// `test_char8_t_containers_are_refused`.
fn rename_unique_ptr_payloads(
    args: &mut Punctuated<GenericArgument, Comma>,
    as_written: &[Option<QualifiedName>],
    deps: &mut HashSet<QualifiedName>,
) {
    for (index, arg) in args.iter_mut().enumerate() {
        if !payload_was_written_verbatim(arg, as_written.get(index)) {
            continue;
        }
        if let GenericArgument::Type(Type::Path(payload)) = arg {
            let name = QualifiedName::from_type_path(payload);
            if let Some(wrapper) = known_types().unique_ptr_payload_wrapper(&name) {
                deps.insert(wrapper.clone());
                *payload = wrapper.to_type_path();
            }
        }
    }
}

/// Turn down a `std::shared_ptr` or `std::weak_ptr` whose payload is an atom
/// reached through an alias.
///
/// Those two take atoms a `unique_ptr` will not, and the bridge names such a
/// payload by its atom - so an alias which was really a `const uint32_t`, with
/// the qualifier dropped by bindgen, would declare a container of a mutable
/// one and the shim would fail to bind. Refusing it here keeps the clean
/// per-function rejection those signatures had before the atoms were let in at
/// all; an alias to a named type is unaffected, and so is a payload bindgen
/// wrote itself.
fn refuse_aliased_atom_payload(
    args: &Punctuated<GenericArgument, Comma>,
    as_written: &[Option<QualifiedName>],
) -> Result<(), ConvertErrorFromCpp> {
    for (index, arg) in args.iter().enumerate() {
        if payload_was_written_verbatim(arg, as_written.get(index)) {
            continue;
        }
        if let GenericArgument::Type(Type::Path(payload)) = arg {
            let name = QualifiedName::from_type_path(payload);
            if !known_types().permissible_within_unique_ptr(&name) {
                return Err(ConvertErrorFromCpp::InvalidTypeForCppPtr(name));
            }
        }
    }
    Ok(())
}

/// The identifier autocxx gives a concrete type it manufactures for a C++ type
/// cxx cannot spell, derived from that type's C++ definition so that the same
/// definition always earns the same name.
pub(crate) fn concrete_type_ident(cpp_definition: &str) -> String {
    let ident = format!(
        "{}_AutocxxConcrete",
        cpp_definition.replace(|c: char| !(c.is_ascii_alphanumeric() || c == '_'), "_")
    );
    // Remove runs of multiple _s. Trying to avoid a dependency on regex.
    ident.split('_').filter(|s| !s.is_empty()).join("_")
}
