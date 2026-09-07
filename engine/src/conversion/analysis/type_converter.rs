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
            AnalysisPhase, Api, ApiName, NullPhase, OpaqueTypedefReason, TypedefKind, UnanalyzedApi,
        },
        apivec::ApiVec,
        codegen_cpp::type_to_cpp::CppNameMap,
        type_helpers::{
            extract_pinned_mutable_reference_type, unwrap_bitfield, unwrap_const,
            unwrap_function_pointer, unwrap_has_opaque, unwrap_reference,
        },
        ConvertErrorFromCpp,
    },
    known_types::{known_types, CxxGenericType},
    types::{make_ident, Namespace, QualifiedName},
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
        }
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

    fn map<T2, F: FnOnce(T) -> T2>(self, fun: F) -> Annotated<T2> {
        Annotated {
            ty: fun(self.ty),
            types_encountered: self.types_encountered,
            extra_apis: self.extra_apis,
            kind: self.kind,
            is_const: self.is_const,
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
    ignored_types: HashSet<QualifiedName>,
    config: &'a IncludeCppConfig,
    original_name_map: CppNameMap,
}

impl<'a> TypeConverter<'a> {
    pub(crate) fn new<A: AnalysisPhase>(config: &'a IncludeCppConfig, apis: &ApiVec<A>) -> Self
    where
        A::TypedefAnalysis: TypedefTarget,
    {
        Self {
            types_found: find_types(apis),
            typedefs: Self::find_typedefs(apis),
            concrete_templates: Self::find_concrete_templates(apis),
            forward_declarations: Self::find_incomplete_types(apis),
            ignored_types: Self::find_ignored_types(apis),
            config,
            original_name_map: CppNameMap::new_for_analysis(apis),
        }
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
            Type::Reference(mut r) => {
                let innerty = self.convert_boxed_type(r.elem, ns, &ctx.behind_reference())?;
                r.elem = innerty.ty;
                Annotated::new(
                    Type::Reference(r),
                    innerty.types_encountered,
                    innerty.extra_apis,
                    TypeKind::Reference,
                )
            }
            Type::Array(mut arr) => {
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
                Annotated::new(
                    Type::Array(arr),
                    innerty.types_encountered,
                    innerty.extra_apis,
                    TypeKind::Regular,
                )
                .marked_const_if(is_const)
            }
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
        } else if let Some(ty) = unwrap_bitfield(&typ) {
            // A bindgen bitfield unit is a `__BindgenBitfieldUnit` wrapping
            // the byte array C++ actually laid the bitfields out in. cxx has
            // no business knowing about the wrapper - and couldn't name it
            // anyway, since its name contains `__` - so pretend the field is
            // just that storage.
            self.convert_type(ty.clone(), ns, ctx)
        } else if let Some(ptr) = unwrap_reference(&typ, false) {
            // LValue reference
            let mutability = ptr.mutability;
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
            Ok(outer)
        } else if let Some(ptr) = unwrap_reference(&typ, true) {
            // RValue reference
            Self::ensure_pointee_is_valid(ptr, ctx)?;
            let innerty = self.convert_boxed_type(ptr.elem.clone(), ns, &ctx.behind_reference())?;
            let mut ptr = ptr.clone();
            ptr.elem = innerty.ty;
            Ok(Annotated::new(
                Type::Ptr(ptr),
                innerty.types_encountered,
                innerty.extra_apis,
                TypeKind::RValueReference,
            ))
        } else {
            // An actual path
            let newp = self.convert_type_path_which_is_not_a_reference(typ, ns, ctx)?;
            if let Type::Path(newpp) = &newp.ty {
                let qn = QualifiedName::from_type_path(newpp);
                if !ctx.allow_instantiation_of_forward_declaration()
                    && self.forward_declarations.contains_key(&qn)
                {
                    return Err(self.incomplete_type_error(qn));
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
        let target_is_const = self
            .resolve_typedef(&original_tn)?
            .is_some_and(|target| target.is_const);
        // First let's see if this is a typedef.
        let (mut typ, tn) = match self.resolve_typedef(&original_tn)? {
            None => (typ, original_tn),
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
                    return result.map(|mut annotated| {
                        annotated.types_encountered.extend(deps);
                        annotated.marked_const_if(target_is_const)
                    });
                }
                // `Pin<&mut T>` is not a name to go looking for: it is what
                // analysing the typedef already made of a C++ mutable
                // reference, and it is finished. Read as a name it is the
                // generic `core::pin::Pin`, which cxx knows nothing about, so
                // autocxx would invent a concrete type for it and write
                // `T&*` into the generated C++. See google/autocxx#1363.
                if extract_pinned_mutable_reference_type(resolved_tp).is_some() {
                    return Ok(Annotated::new(
                        Type::Path(resolved_tp.clone()),
                        deps,
                        ApiVec::new(),
                        TypeKind::MutableReference,
                    ));
                }
                let resolved_tn = QualifiedName::from_type_path(resolved_tp);
                deps.insert(resolved_tn.clone());
                (resolved_tp.clone(), resolved_tn)
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
                annotated.types_encountered.extend(deps);
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
                return Ok(annotated.marked_const_if(target_is_const));
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
                return Ok(Annotated::new(other.clone(), deps, ApiVec::new(), kind)
                    .marked_const_if(target_is_const));
            }
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
        // `std::shared_ptr` is the only one of the three which gets an
        // accessor surface generated for it (see `shared_ptr_payload`). A
        // `std::unique_ptr<const T>` or `std::weak_ptr<const T>` is lowered to
        // the same opaque holder, which builds and round-trips where today it
        // is a hard C++ error, but Rust can do nothing with one but hand it
        // back to C++.
        let payload_is_const = generic_args_are_const_qualified(&typ);
        if known_types().cxx_generic_behavior(&tn) == CxxGenericType::CppPtr && payload_is_const {
            let mut extra_apis = ApiVec::new();
            // Only `std::shared_ptr` gets an accessor surface, and only when
            // its payload is a type the bridge can name in one.
            //
            // The payload's dependency is recorded here, on whatever is being
            // converted, rather than on the holder: `Api::ConcreteType` has no
            // arm in `deps.rs` to carry one. Wherever the holder is reached
            // through something else - a function, an alias, another container
            // - that is enough, because the conversion which yielded the
            // holder recorded the payload on that same something, so the
            // holder outlives the payload only if nothing names the holder at
            // all. `test_shared_ptr_const_class_payload_survives_through_an_alias`
            // walks the longest of those chains.
            //
            // It is not enough under `generate_all!`, which makes every API a
            // garbage-collection root, holders included. A holder rooted that
            // way survives on its own, and if its payload class is an
            // `IgnoredItem` the accessor generated below names a type nothing
            // declares, and the generated code does not compile. Giving
            // `Api::ConcreteType` a dependency of its own is the fix, and is
            // the queued follow-up to the lowering rather than part of it; a
            // builtin payload, which is all the marker used to reach us for,
            // cannot be ignored, which is why nothing has hit this yet.
            let payload = if tn == QualifiedName::new_from_cpp_name("std::shared_ptr") {
                match self.convert_const_payload(&typ, ns) {
                    Some(Ok(mut payload)) => {
                        deps.extend(payload.types_encountered.drain(..));
                        extra_apis.append(&mut payload.extra_apis);
                        Some(Box::new(payload.ty.into()))
                    }
                    Some(Err(err)) => return Err(err),
                    None => None,
                }
            } else {
                None
            };
            let (new_tn, api) = self.get_templated_typename(&Type::Path(typ))?;
            deps.insert(new_tn.clone());
            // `None` where this instantiation already had a holder, which
            // carries the payload it was first created with.
            if let Some(Api::ConcreteType {
                name,
                rs_definition,
                cpp_definition,
                ..
            }) = api
            {
                extra_apis.push(Api::ConcreteType {
                    name,
                    rs_definition,
                    cpp_definition,
                    shared_ptr_payload: payload,
                });
            }
            return Ok(Annotated::new(
                Type::Path(new_tn.to_type_path()),
                deps,
                extra_apis,
                TypeKind::Regular,
            )
            .marked_const_if(target_is_const));
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
            let generic_behavior = known_types().cxx_generic_behavior(&tn);
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
                    let mut innerty = self.convert_punctuated(
                        ab.args.clone(),
                        ns,
                        &TypeConversionContext::WithinContainer,
                    )?;
                    ab.args = innerty.ty;
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
        Ok(
            Annotated::new(Type::Path(typ), deps, extra_apis, kind)
                .marked_const_if(target_is_const),
        )
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
        let innerty = self.convert_boxed_type(ptr.elem, ns, &ctx.behind_reference())?;
        ptr.elem = innerty.ty;
        Ok(Annotated::new(
            Type::Ptr(ptr),
            innerty.types_encountered,
            innerty.extra_apis,
            TypeKind::Pointer,
        ))
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

    /// The `T` of a `std::shared_ptr<const T>`, converted as the `cxx::bridge`
    /// would spell it, for the accessor the holder gets.
    ///
    /// `None` where the instantiation has no single `const`-qualified type
    /// argument to read - which is nothing `std::shared_ptr` can be written
    /// with today, but the pattern match rather than an `unwrap` is what keeps
    /// it that way. The holder is still generated in that case; it simply has
    /// no accessor.
    fn convert_const_payload(
        &mut self,
        typ: &TypePath,
        ns: &Namespace,
    ) -> Option<Result<Annotated<Type>, ConvertErrorFromCpp>> {
        let PathArguments::AngleBracketed(args) = &typ.path.segments.last()?.arguments else {
            return None;
        };
        let [GenericArgument::Type(Type::Path(payload))] =
            args.args.iter().collect::<Vec<_>>().as_slice()
        else {
            return None;
        };
        let payload = unwrap_const(payload)?.clone();
        Some(self.convert_type(payload, ns, &TypeConversionContext::WithinContainer))
    }

    fn get_templated_typename(
        &mut self,
        rs_definition: &Type,
    ) -> Result<(QualifiedName, Option<UnanalyzedApi>), ConvertErrorFromCpp> {
        let count = self.concrete_templates.len();
        // We just use this as a hash key, essentially.
        let cpp_definition = self.original_name_map.type_to_cpp(rs_definition)?;
        let e = self.concrete_templates.get(&cpp_definition);
        match e {
            Some(tn) => Ok((tn.clone(), None)),
            None => {
                let synthetic_ident = format!(
                    "{}_AutocxxConcrete",
                    cpp_definition.replace(|c: char| !(c.is_ascii_alphanumeric() || c == '_'), "_")
                );
                // Remove runs of multiple _s. Trying to avoid a dependency on
                // regex.
                let synthetic_ident = synthetic_ident
                    .split('_')
                    .filter(|s| !s.is_empty())
                    .join("_");
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
                let api = UnanalyzedApi::ConcreteType {
                    name: ApiName::new_in_root_namespace(make_ident(synthetic_ident)),
                    cpp_definition: cpp_definition.clone(),
                    rs_definition: Some(Box::new(rs_definition.clone().into())),
                    shared_ptr_payload: None,
                };
                self.concrete_templates
                    .insert(cpp_definition, api.name().clone());
                Ok((api.name().clone(), Some(api)))
            }
        }
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
                    if !forward_declarations_ok && self.forward_declarations.contains_key(&inner_qn)
                    {
                        return Err(self.incomplete_type_error(inner_qn));
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
                        CxxGenericType::CppPtr => {
                            if !known_types().permissible_within_unique_ptr(&inner_qn) {
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

/// Whether any of `typ`'s template arguments carries bindgen's `const` marker,
/// as `std::shared_ptr<const T>` does.
///
/// Only the argument's own qualifier counts, which is the one cxx has no room
/// for; a `const` on something the argument points at is already in the type,
/// as `*const T`.
///
/// A record argument used to be invisible here: bindgen kept the marker only
/// on an argument it rendered directly, and resolved a class argument's
/// `TypeKind::ResolvedTypeRef` past the node the qualifier was on, so
/// `std::shared_ptr<const Foo>` arrived indistinguishable from
/// `std::shared_ptr<Foo>`. `third_party/patches/18-const-template-argument.patch`
/// carries the qualifier across that resolution, so every argument C++ wrote
/// `const` is one this can see.
///
/// One kind still is not, and this time the gap is here rather than in
/// bindgen: a `const` reached through an alias. bindgen keeps the marker on
/// the alias - `typedef const int CI` comes out
/// `pub type CI = __bindgen_marker_Const<c_int>` - but the argument of
/// `std::shared_ptr<CI>` is the path `CI` with nothing on it, and this looks
/// no further. Resolving the alias would mean asking `resolve_typedef` here,
/// where every other reader of that constness asks after conversion instead;
/// until it does, that one spelling is not lowered and the C++ cxx generates
/// fails to bind, which is the pre-lowering symptom. (A `const` alias whose
/// target is a *record* or an enum is a different matter: bindgen erases the
/// qualifier from those, which is the remaining gap recorded in
/// `11-const-newtype-marker.patch`.)
fn generic_args_are_const_qualified(typ: &TypePath) -> bool {
    let Some(PathArguments::AngleBracketed(args)) =
        typ.path.segments.last().map(|seg| &seg.arguments)
    else {
        return false;
    };
    args.args.iter().any(|arg| {
        matches!(arg, GenericArgument::Type(Type::Path(inner)) if unwrap_const(inner).is_some())
    })
}

/// Processing functions sometimes results in new types being materialized.
/// These types haven't been through the analysis phases (chicken and egg
/// problem) but fortunately, don't need to. We need to keep the type
/// system happy by adding an [ApiAnalysis] but in practice, for the sorts
/// of things that get created, it's always blank.
pub(crate) fn add_analysis<A: AnalysisPhase>(api: UnanalyzedApi) -> Api<A> {
    match api {
        Api::ConcreteType {
            name,
            rs_definition,
            cpp_definition,
            shared_ptr_payload,
        } => Api::ConcreteType {
            name,
            rs_definition,
            cpp_definition,
            shared_ptr_payload,
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
