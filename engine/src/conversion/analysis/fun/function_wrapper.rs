// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::conversion::analysis::fun::ReceiverMutability;
use crate::conversion::parse::CppRefQualifier;
use crate::conversion::{ConvertErrorFromCpp, CppEffectiveName};
use crate::minisyn::Ident;
use crate::{
    conversion::{api::SubclassName, type_helpers::extract_pinned_mutable_reference_type},
    types::{Namespace, QualifiedName},
};
use quote::ToTokens;
use syn::{parse_quote, Type, TypePtr, TypeReference};

/// The raw pointer a `cxx::bridge` declaration carries where the C++ it stands
/// for has a reference, split into the only two things anything asks of it:
/// what it points at, and whether it is mutable.
///
/// Both halves are needed all over the conversion code - to name the C++
/// reference the pointer stands for, to pick between `CppRef` and `CppMutRef`,
/// to build the `Pin<&mut MaybeUninit<T>>` a constructor takes. Deriving them
/// by matching a `syn::Type` at each of those places is what used to make
/// every one of them a potential panic. Derived once, here, they are ordinary
/// fields and every reader of them is a total function.
#[derive(Clone, Debug)]
pub(crate) struct BridgePointer {
    pointee: crate::minisyn::Type,
    mutable: bool,
}

impl BridgePointer {
    /// The pointer to `pointee`, mutable or not as `mutable` says.
    pub(crate) fn to(pointee: Type, mutable: bool) -> Self {
        Self {
            pointee: pointee.into(),
            mutable,
        }
    }

    /// The pointer `ty` is, if it is one.
    pub(crate) fn from_type(ty: &Type) -> Option<Self> {
        match ty {
            Type::Ptr(TypePtr {
                elem, mutability, ..
            }) => Some(Self::to((**elem).clone(), mutability.is_some())),
            _ => None,
        }
    }

    /// What the pointer points at.
    pub(crate) fn pointee(&self) -> &Type {
        &self.pointee
    }

    /// Whether it is a `*mut`, as opposed to a `*const`.
    pub(crate) fn is_mut(&self) -> bool {
        self.mutable
    }

    /// The pointer type itself, as the `cxx::bridge` declares it.
    pub(crate) fn ty(&self) -> Type {
        let pointee = &self.pointee;
        if self.mutable {
            parse_quote! { *mut #pointee }
        } else {
            parse_quote! { *const #pointee }
        }
    }
}

/// What C++ does with a bridge type which is a raw pointer.
///
/// The four reference variants read the pointee, to spell the C++ reference
/// the pointer stands for. The other three do not, and are here because they
/// occur alongside a pointer anyway - two of them beside a Rust-side
/// conversion which does read it, and one beside no conversion at all. Either
/// way the [`BridgePointer`] is a field of the same
/// [`TypeConversionPolicy::Pointer`] variant, so reading it is a field access
/// rather than a `syn::Type` match with nothing to say when it does not
/// match.
#[derive(Clone, Debug)]
pub(crate) enum PointerCppConversion {
    /// C++ takes the pointer exactly as the bridge declares it. Only the Rust
    /// side has anything to do.
    None,
    /// `std::move(*p)`. A move constructor has a move constructor to call, so
    /// `std::move` says exactly what is meant here.
    FromPtrToMove,
    /// Ignored in the sense that it isn't passed into the C++ function: the
    /// pointer a placement-new return is constructed into.
    IgnoredPlacementPtrParameter,
    /// `(*p)`: the underlying C++ function asked for a reference, and the
    /// bridge carries the pointer which stands for it.
    FromPointerToReference,
    /// `&r`: C++ produced a reference, and the bridge carries the pointer
    /// which stands for it.
    FromReferenceToPointer,
    /// The rvalue-reference counterparts of the two above, for a C++ function
    /// declared `T&&`. The type converter turns such a return into the same
    /// pointer it makes of a `T&`, and nothing in Rust spells the difference,
    /// so these two carry it instead.
    FromPointerToRValueReference,
    FromRValueReferenceToPointer,
    /// `const_cast<T*>(p)`: C++ returned a `volatile T*` and the bridge carries
    /// the unqualified pointer. The qualifier cannot be dropped implicitly, and
    /// the bridge has no way to spell it, so the wrapper casts it away and the
    /// Rust side restores the discipline by handing over an
    /// `autocxx::VolatilePtr<T>` rather than a raw pointer.
    FromVolatilePointerToPointer,
    /// The same for a `volatile T&` return, which reaches the bridge as the
    /// pointer standing for it: `const_cast<T*>(::std::addressof(r))`.
    FromVolatileReferenceToPointer,
    /// `static_cast<volatile T*>(p)`, for a parameter. The qualifier would be
    /// added back implicitly by the call, but only *after* overload resolution
    /// has chosen which function to call - so where a name has both a
    /// `volatile`-pointee overload and a plain one, the unqualified argument
    /// picks the plain one and the binding for the other silently calls it. The
    /// cast makes the choice before resolution sees it.
    FromPointerToVolatilePointer,
    /// The same for a `volatile T&` parameter:
    /// `static_cast<volatile T&>(*p)`.
    FromPointerToVolatileReference,
}

impl PointerCppConversion {
    /// If we've found a function which does X to its parameter, what is the
    /// opposite of X? This is used for subclasses where calls from Rust to C++
    /// might also involve calls from C++ to Rust.
    ///
    /// `None` where there is no such thing - see
    /// [`TypeConversionPolicy::inverse`].
    fn inverse(&self) -> Option<Self> {
        Some(match self {
            // Changes no type, so has no opposite to perform on the way back.
            Self::None => Self::None,
            Self::FromPointerToReference => Self::FromReferenceToPointer,
            Self::FromReferenceToPointer => Self::FromPointerToReference,
            Self::FromPointerToRValueReference => Self::FromRValueReferenceToPointer,
            Self::FromRValueReferenceToPointer => Self::FromPointerToRValueReference,
            // Moving out of the pointer, and leaving a parameter out of the
            // call altogether, are things the wrapper does on the way into
            // C++. Neither describes a change of type which the way back could
            // undo, and neither has ever been asked to.
            Self::FromPtrToMove | Self::IgnoredPlacementPtrParameter => return Option::None,
            // Casting a qualifier away has no opposite a wrapper could
            // perform: putting `volatile` back would be inventing a promise
            // about storage rather than restoring one. Asking yields
            // `NonInvertibleConversion`, which is the honest answer for a
            // subclass peer overriding such a method.
            Self::FromVolatilePointerToPointer
            | Self::FromVolatileReferenceToPointer
            | Self::FromPointerToVolatilePointer
            | Self::FromPointerToVolatileReference => return Option::None,
        })
    }
}

/// What Rust does with a bridge type which is a raw pointer.
///
/// All but the identity read the pointee, the mutability, or both: to build
/// the `Pin<&mut MaybeUninit<T>>` a constructor takes, or to pick between
/// `CppRef` and `CppMutRef`. See [`PointerCppConversion`] for where they get
/// it.
#[derive(Clone, Debug)]
pub(crate) enum PointerRustConversion {
    /// The pointer crosses as it is.
    None,
    FromPinMaybeUninitToPtr,
    FromPinMoveRefToPtr,
    FromTypeToPtr,
    FromPlacementParamToNewReturn,
    FromReferenceWrapperToPointer,
    FromPointerToReferenceWrapper,
    /// `autocxx::VolatilePtr<T>` in the wrapper's signature, the bare pointer
    /// on the bridge. C++ wrote `volatile` on what this points at, and Rust
    /// states volatility in the access rather than in a type, so an ordinary
    /// `*mut T` here would be read with an ordinary load. These two carry the
    /// handle which only reads and writes volatilely.
    FromVolatilePtrToPointer,
    FromPointerToVolatilePtr,
}

/// A Rust-side conversion which the caller of `convert_fn_arg` insists on for
/// a parameter, whatever the parameter's type would otherwise have earned it.
///
/// All but one of them need the parameter to reach the `cxx::bridge` as a
/// pointer, which is what [`PointerRustConversion`] already says.
/// [`Self::Identity`] is the odd one out, forced not because it converts
/// anything but because naming a conversion at all is how a caller says "leave
/// this parameter alone" - see the copy constructor's source parameter, which
/// has to stay a `&T` even under `ReferencesWrappedAllFunctionsSafe`.
#[derive(Clone, Debug)]
pub(crate) enum ForcedRustConversion {
    Pointer(PointerRustConversion),
    Identity,
}

impl ForcedRustConversion {
    /// This conversion, for a parameter the `cxx::bridge` carries as a
    /// pointer.
    pub(crate) fn on_pointer(self) -> PointerRustConversion {
        match self {
            Self::Pointer(conversion) => conversion,
            Self::Identity => PointerRustConversion::None,
        }
    }

    /// This conversion, for a parameter the `cxx::bridge` carries whole.
    ///
    /// Only the identity conversion can be that. The rest read a pointee, so a
    /// caller asking for one where the type converter produced no pointer is
    /// an autocxx bug, and used to be a panic several files later in codegen.
    pub(crate) fn on_whole(self, ty: &Type) -> Result<WholeRustConversion, ConvertErrorFromCpp> {
        match self {
            Self::Identity => Ok(WholeRustConversion::None),
            Self::Pointer(_) => Err(ConvertErrorFromCpp::ParameterWasNotAPointer(
                ty.to_token_stream().to_string(),
            )),
        }
    }
}

/// What C++ does with a bridge type which nothing takes apart.
#[derive(Clone, Debug)]
pub(crate) enum WholeCppConversion {
    None,
    Move,
    /// Hand a by-value parameter on to the real function with
    /// `autocxx_move_or_copy`, which prefers a move and settles for a copy.
    ///
    /// Unlike every other variant this changes no type - the wrapper's
    /// parameter and the function's are the same - so it is not by itself a
    /// reason to generate a wrapper at all (see
    /// [`TypeConversionPolicy::cpp_work_needed`]). It only says how a wrapper
    /// which exists anyway should spell the hand-over. See google/autocxx#1252.
    MoveOrCopy,
    FromUniquePtrToValue,
    FromPtrToValue,
    FromValueToUniquePtr,
    FromReturnValueToPlacementPtr,
}

impl WholeCppConversion {
    /// As [`PointerCppConversion::inverse`].
    fn inverse(&self) -> Option<Self> {
        Some(match self {
            // `None` changes no type and has nothing to undo.
            //
            // `MoveOrCopy` changes no type either, and inverts to `None`,
            // which is what it has always done and not obviously right: a
            // subclass peer's override of a method taking one of these hands
            // its own by-value parameter on bare, which asks for a copy
            // constructor, and a POD which declares its own move constructor
            // has none - google/autocxx#1252 is that same missing constructor
            // on the other side of the call. Answering `MoveOrCopy` would ask
            // for `::autocxx_move_or_copy` instead and cover both, but it
            // would also rewrite the C++ generated for every override which
            // takes an ordinary copyable POD, so it is not this change's to
            // make.
            Self::None | Self::MoveOrCopy => Self::None,
            Self::FromUniquePtrToValue | Self::FromPtrToValue => Self::FromValueToUniquePtr,
            Self::FromValueToUniquePtr => Self::FromUniquePtrToValue,
            // `Move` changes no type either, but it is not the same as nothing:
            // it is what a by-value parameter of a type with no copy
            // constructor gets, and handing one of those on without the
            // `std::move` asks for the copy constructor it does not have. The
            // way back is a subclass peer's override receiving such a value and
            // passing it to Rust, which needs the move for the same reason.
            Self::Move => Self::Move,
            // Placement new is the wrapper constructing its result into a
            // pointer the caller supplied, which is a shape the way back does
            // not have: a subclass peer's override returns its value.
            Self::FromReturnValueToPlacementPtr => return Option::None,
        })
    }
}

/// What Rust does with a bridge type which nothing takes apart.
#[derive(Clone, Debug)]
pub(crate) enum WholeRustConversion {
    None,
    FromStr,
    ToBoxedUpHolder(SubclassName),
    FromValueParamToPtr,
    FromRValueParamToPtr,
}

/// The pointer standing for a C++ reference which a function returns, out of
/// whatever the type converter made of that reference.
///
/// There are three shapes to meet, and which one arrives depends on the
/// unsafety policy and on whether the reference is mutable: the raw pointer the
/// converter makes of a `T&` when nothing else claims it, the `&T` it makes of
/// a const reference, and the `Pin<&mut T>` it makes of a mutable one. All
/// three name the same C++ reference, and everything downstream wants it as the
/// pointer the `cxx::bridge` will carry, so this is the one place which knows
/// how to read each of them.
///
/// Nothing else is a C++ reference return, so anything else is an autocxx bug
/// and says so rather than crashing: the two callers are both building a
/// conversion inside a fallible analysis, so a refusal reaches the user as the
/// function being left out with a reason attached.
fn reference_return_as_pointer(ty: &Type) -> Result<BridgePointer, ConvertErrorFromCpp> {
    match ty {
        Type::Reference(TypeReference {
            elem, mutability, ..
        }) => Some(BridgePointer::to((**elem).clone(), mutability.is_some())),
        Type::Path(tp) => extract_pinned_mutable_reference_type(tp)
            .map(|unwrapped| BridgePointer::to(unwrapped.clone(), true)),
        _ => BridgePointer::from_type(ty),
    }
    .ok_or_else(|| ConvertErrorFromCpp::UnexpectedReferenceReturn(ty.to_token_stream().to_string()))
}

/// A policy for converting types. Conversion may occur on both the Rust and
/// C++ side. The most complex example is a C++ function which takes
/// std::string by value, which might do this:
/// * Client Rust code: `&str`
/// * Rust wrapper function: converts `&str` to `UniquePtr<CxxString>`
/// * cxx::bridge mod: refers to `UniquePtr<CxxString>`
/// * C++ wrapper function converts `std::unique_ptr<std::string>` to just
///   `std::string`
/// * Finally, the actual C++ API receives a `std::string` by value.
///
/// The implementation here is distributed across this file, and
/// `function_wrapper_rs` and `function_wrapper_cpp`.
///
/// The split between the two variants is the one thing the conversions
/// disagree about: whether anything has to look inside the type. Several of
/// them do - to name the C++ reference a pointer stands for, to wrap that
/// pointer in a `CppRef`, to build the `Pin<&mut MaybeUninit<T>>` a
/// constructor takes - and every one of those used to re-derive the pointee by
/// matching a `syn::Type` which it had no way of knowing was a pointer, and
/// panicked several files from the mistake when it was not. Those conversions
/// live in [`Self::Pointer`], which holds a [`BridgePointer`] and so cannot be
/// built without one; the rest live in [`Self::Whole`], which holds the type
/// and hands it on entire.
#[derive(Clone, Debug)]
pub(crate) enum TypeConversionPolicy {
    /// A raw pointer, kept as the pointee and the mutability so that the
    /// conversions which read those can. Not every pairing here does read
    /// them, since an ordinary `T*` parameter which neither side converts is a
    /// `Pointer` too, but every conversion which could is in this variant.
    Pointer {
        pointer: BridgePointer,
        cpp: PointerCppConversion,
        rust: PointerRustConversion,
    },
    /// A type which neither side takes apart, so it is kept whole. It may
    /// itself be a pointer: a function returning a `T*` converts nothing and
    /// gets one of these.
    Whole {
        ty: crate::minisyn::Type,
        cpp: WholeCppConversion,
        rust: WholeRustConversion,
    },
}

impl TypeConversionPolicy {
    /// A type which crosses unchanged: neither side has anything to do.
    pub(crate) fn new_unconverted(ty: Type) -> Self {
        Self::whole(ty, WholeCppConversion::None, WholeRustConversion::None)
    }

    /// A conversion of a type which is handed on entire.
    pub(crate) fn whole(ty: Type, cpp: WholeCppConversion, rust: WholeRustConversion) -> Self {
        Self::Whole {
            ty: ty.into(),
            cpp,
            rust,
        }
    }

    /// A conversion of a pointer which one side or the other reads.
    pub(crate) fn pointer(
        pointer: BridgePointer,
        cpp: PointerCppConversion,
        rust: PointerRustConversion,
    ) -> Self {
        Self::Pointer { pointer, cpp, rust }
    }

    /// The unwrapped type this conversion is about: the pointer for
    /// [`Self::Pointer`], and whatever [`Self::Whole`] was built with.
    ///
    /// That is often what the `cxx::bridge` declaration carries, but not
    /// always: the three conversions between a value and something holding it
    /// put something else there, `UniquePtr<T>` for a `T` handed over as a
    /// `std::unique_ptr` and `*mut T` for one handed over as a pointer.
    /// [`Self::converted_rust_type`] and [`Self::unconverted_rust_type`] are
    /// what the bridge is written from, and each of those answers this for
    /// every conversion but its own.
    pub(crate) fn cxxbridge_type(&self) -> Type {
        match self {
            Self::Pointer { pointer, .. } => pointer.ty(),
            Self::Whole { ty, .. } => ty.clone().into(),
        }
    }

    /// The value of a C++ function which returns a reference, under
    /// `ReferencesWrappedAllFunctionsSafe`, which wants Rust to receive a
    /// `CppRef`/`CppMutRef` rather than the raw pointer the bridge carries.
    pub(crate) fn return_reference_into_wrapper(ty: Type) -> Result<Self, ConvertErrorFromCpp> {
        Ok(Self::pointer(
            reference_return_as_pointer(&ty)?,
            PointerCppConversion::FromReferenceToPointer,
            PointerRustConversion::FromPointerToReferenceWrapper,
        ))
    }

    /// The return value of a C++ function declared `T&&`, which by the time
    /// it gets here is the same `*mut T`/`*const T` the type converter makes
    /// of a `T&`. Only the conversion remembers which of the two C++ wrote,
    /// and a subclass peer's override has to repeat the superclass's spelling
    /// exactly or it overrides nothing and does not compile.
    ///
    /// `wrap` asks for the pointer to be a `CppRef`/`CppMutRef` on the Rust
    /// side, which is what `ReferencesWrappedAllFunctionsSafe` wants for the
    /// same reason it wants one for a returned `T&`: whoever implements the
    /// override should not have to produce a raw pointer in a mode whose
    /// whole point is that safe Rust never handles one. The wrapper says
    /// "a C++ reference to a T" and no more; that C++ may move out of the
    /// referent, which is what the extra `&` means, is between the override
    /// and the header it is implementing, exactly as it is in C++.
    pub(crate) fn return_rvalue_reference(
        ty: Type,
        wrap: bool,
    ) -> Result<Self, ConvertErrorFromCpp> {
        Ok(Self::pointer(
            reference_return_as_pointer(&ty)?,
            PointerCppConversion::FromRValueReferenceToPointer,
            if wrap {
                PointerRustConversion::FromPointerToReferenceWrapper
            } else {
                PointerRustConversion::None
            },
        ))
    }

    pub(crate) fn new_to_unique_ptr(ty: Type) -> Self {
        Self::whole(
            ty,
            WholeCppConversion::FromValueToUniquePtr,
            WholeRustConversion::None,
        )
    }

    pub(crate) fn new_for_placement_return(ty: Type) -> Self {
        Self::whole(
            ty,
            WholeCppConversion::FromReturnValueToPlacementPtr,
            // Rust conversion is marked as none here, since this policy
            // will be applied to the return value, and the Rust-side
            // shenanigans applies to the placement new *parameter*
            WholeRustConversion::None,
        )
    }

    /// Whether this conversion is a reason to generate a C++ wrapper function.
    ///
    /// [`WholeCppConversion::MoveOrCopy`] is not: it changes no type, so cxx
    /// can hand the parameter over by itself perfectly well. It only has
    /// something to say once a wrapper exists for some other reason.
    /// Whether this parameter is an address whose pointee C++ qualified
    /// `volatile`. Such a parameter needs the C++ wrapper even though its own
    /// conversion is Rust-side only: without one, cxx takes the address of the
    /// C++ function against a bridge signature which cannot spell the
    /// qualifier.
    pub(crate) fn pointee_was_volatile(&self) -> bool {
        matches!(
            self,
            Self::Pointer {
                rust: PointerRustConversion::FromVolatilePtrToPointer,
                ..
            }
        )
    }

    pub(crate) fn cpp_work_needed(&self) -> bool {
        match self {
            Self::Pointer { cpp, .. } => !matches!(cpp, PointerCppConversion::None),
            Self::Whole { cpp, .. } => !matches!(
                cpp,
                WholeCppConversion::None | WholeCppConversion::MoveOrCopy
            ),
        }
    }

    pub(crate) fn unconverted_rust_type(&self) -> Type {
        match self {
            Self::Whole {
                ty,
                cpp: WholeCppConversion::FromValueToUniquePtr,
                ..
            } => unique_ptr_of(ty),
            _ => self.cxxbridge_type(),
        }
    }

    pub(crate) fn converted_rust_type(&self) -> Type {
        match self {
            Self::Whole {
                ty,
                cpp: WholeCppConversion::FromUniquePtrToValue,
                ..
            } => unique_ptr_of(ty),
            Self::Whole {
                ty,
                cpp: WholeCppConversion::FromPtrToValue,
                ..
            } => parse_quote! { *mut #ty },
            _ => self.cxxbridge_type(),
        }
    }

    pub(crate) fn rust_work_needed(&self) -> bool {
        match self {
            Self::Pointer { rust, .. } => !matches!(rust, PointerRustConversion::None),
            Self::Whole { rust, .. } => !matches!(rust, WholeRustConversion::None),
        }
    }

    /// Subclass support involves calls from Rust -> C++, but
    /// also from C++ -> Rust. Work out the correct argument conversion
    /// type for the latter call, when given the former.
    ///
    /// `None` for the handful of C++-side conversions which have no opposite,
    /// which is not the same as inverting to nothing: a wrapper which moves
    /// out of its parameter, leaves it out of the call, or constructs its
    /// result into a caller's pointer describes no change of type for the way
    /// back to undo. Every one of those is a shape a subclass peer has never
    /// been built from, and this used to be a panic; the callers turn it into
    /// a refusal instead, so a route which reaches one arrives as a
    /// limitation rather than a crash.
    pub(crate) fn inverse(&self) -> Option<Self> {
        Some(match self {
            Self::Pointer { pointer, cpp, rust } => Self::Pointer {
                pointer: pointer.clone(),
                cpp: cpp.inverse()?,
                rust: rust.clone(),
            },
            Self::Whole { ty, cpp, rust } => Self::Whole {
                ty: ty.clone(),
                cpp: cpp.inverse()?,
                rust: rust.clone(),
            },
        })
    }

    pub(crate) fn bridge_unsafe_needed(&self) -> bool {
        match self {
            Self::Pointer { rust, .. } => matches!(
                rust,
                PointerRustConversion::FromPlacementParamToNewReturn
                    | PointerRustConversion::FromPointerToReferenceWrapper
                    | PointerRustConversion::FromReferenceWrapperToPointer
            ),
            Self::Whole { rust, .. } => matches!(
                rust,
                WholeRustConversion::FromValueParamToPtr
                    | WholeRustConversion::FromRValueParamToPtr
            ),
        }
    }

    pub(crate) fn is_placement_parameter(&self) -> bool {
        matches!(
            self,
            Self::Pointer {
                cpp: PointerCppConversion::IgnoredPlacementPtrParameter,
                ..
            }
        )
    }

    pub(crate) fn populate_return_value(&self) -> bool {
        !matches!(
            self,
            Self::Whole {
                cpp: WholeCppConversion::FromReturnValueToPlacementPtr,
                ..
            }
        )
    }

    /// Whether the local variable the Rust wrapper declares for this
    /// parameter has to be `mut`.
    pub(crate) fn requires_mutability(&self) -> Option<syn::token::Mut> {
        match self {
            Self::Pointer {
                rust: PointerRustConversion::FromPinMoveRefToPtr,
                ..
            } => Some(parse_quote! { mut }),
            _ => None,
        }
    }

    /// Whether this parameter reaches Rust as an `impl ValueParam<T>`, which
    /// borrows the caller's value for the duration of the call and so counts
    /// as a reference when working out lifetimes.
    pub(crate) fn is_value_param(&self) -> bool {
        matches!(
            self,
            Self::Whole {
                rust: WholeRustConversion::FromValueParamToPtr,
                ..
            }
        )
    }

    /// Whether this parameter reaches Rust as one of the reference wrappers of
    /// `ReferencesWrappedAllFunctionsSafe`.
    pub(crate) fn takes_reference_wrapper(&self) -> bool {
        matches!(
            self,
            Self::Pointer {
                rust: PointerRustConversion::FromReferenceWrapperToPointer,
                ..
            }
        )
    }

    /// Whether this return value reaches Rust as one of those wrappers.
    pub(crate) fn returns_reference_wrapper(&self) -> bool {
        matches!(
            self,
            Self::Pointer {
                rust: PointerRustConversion::FromPointerToReferenceWrapper,
                ..
            }
        )
    }
}

/// The `cxx::UniquePtr<T>` which stands in Rust for a `std::unique_ptr<T>`.
fn unique_ptr_of(innerty: &crate::minisyn::Type) -> Type {
    parse_quote! {
        cxx::UniquePtr < #innerty >
    }
}

#[derive(Clone, Debug)]
pub(crate) enum CppFunctionBody {
    FunctionCall(Namespace, CppEffectiveName),
    /// A call to a member the receiver's class inherits from a base, made on
    /// the receiver cast to that base so that nothing the receiver's own class
    /// declares can decide which member is called.
    ///
    /// The base is carried as the C++ spelling of its name rather than as a
    /// [`QualifiedName`], because the cast has to write that spelling and
    /// nothing else here keeps the base alive: garbage collection drops a base
    /// class nobody asked for, and the name map codegen would otherwise ask
    /// then falls back on bindgen's flattened identifier, which names nothing
    /// in C++. The [`ReceiverMutability`] is the cast's, the receiver being a
    /// reference to `const` for a `const` member.
    BaseClassMethodCall(String, CppEffectiveName, ReceiverMutability),
    /// Read a data member off the receiver, by the name C++ gives it. This is
    /// the whole body of a synthesized field accessor; see
    /// `analysis::field_accessors`.
    FieldRead(String),
    /// Read a C++ variable with static storage duration. This is the whole
    /// body of the getter synthesized for one whose type Rust cannot be shown
    /// as bindgen declared it; see `analysis::statics`.
    VariableRead(QualifiedName),
    StaticMethodCall(Namespace, Ident, CppEffectiveName),
    PlacementNew(Namespace, Ident),
    ConstructSuperclass(String),
    Cast,
    Destructor(Namespace, Ident),
    AllocUninitialized(QualifiedName),
    FreeUninitialized(QualifiedName),
}

#[derive(Clone, Debug)]
pub(crate) enum CppFunctionKind {
    Function,
    Method,
    Constructor,
    ConstMethod,
    SynthesizedConstructor,
}

/// What the receiver is called in everything autocxx generates for a function
/// wrapper which is a method: the first parameter of the C++ function
/// `CppCodeGenerator` writes, and the first parameter of the `cxx::bridge`
/// declaration the function analysis writes for the same function.
///
/// Keeping the two the same is a convention, not a requirement: the C++ name
/// is local to the function `CppCodeGenerator` writes - its signature and its
/// body, nothing else reads it - the Rust name is local to the bridge
/// declaration, and C++ matches parameters by position, so the two can
/// diverge and everything still builds. It earns the constant anyway, because
/// a reader who meets `autocxx_gen_this` in a compiler diagnostic about the
/// generated C++ and then again in the bridge is looking at the same
/// parameter, and one constant is where to say so.
///
/// The name does have to be strange: cxx copies bridge parameter names into
/// the C++ shim it generates, so a C++ method with a parameter genuinely
/// called `autocxx_gen_this` produces a shim declaring that name twice, which
/// C++ rejects with "redefinition of parameter". Nothing detects or renames
/// around that collision today.
pub(crate) const RECEIVER_ARG_NAME: &str = "autocxx_gen_this";

#[derive(Clone, Debug)]
pub(crate) struct CppFunction {
    pub(crate) payload: CppFunctionBody,
    pub(crate) wrapper_function_name: crate::minisyn::Ident,
    /// Read only from the `CppFunction` a subclass method carries:
    /// `generate_subclass` names the superclass virtual method the peer class
    /// overrides from here, and the method its `_super` helper calls. The
    /// wrappers `analyze_foreign_fn` builds fill this in and nothing reads it.
    pub(crate) original_cpp_name: CppEffectiveName,
    pub(crate) return_conversion: Option<TypeConversionPolicy>,
    pub(crate) argument_conversion: Vec<TypeConversionPolicy>,
    pub(crate) kind: CppFunctionKind,
    pub(crate) pass_obs_field: bool,
    pub(crate) qualification: Option<QualifiedName>,
    /// If this C++ function is overriding a ref-qualified virtual method, it
    /// must repeat the ref-qualifier, or it doesn't override anything and
    /// doesn't even compile alongside the method it's meant to override. That
    /// applies to `&&` just as much as to `&`: an override of a pure virtual
    /// `&&` method is generated and dispatches to Rust as usual, even though
    /// we generate no Rust binding for calling the superclass method.
    /// [`CppRefQualifier::None`] in every other case; autocxx never introduces
    /// a ref-qualifier which wasn't in the original C++.
    pub(crate) ref_qualifier: CppRefQualifier,
    /// Whether the body names a C++ declaration marked `[[deprecated]]`, so
    /// that the generated function is bracketed by a pragma which silences
    /// `-Wdeprecated-declarations` for it alone. The signal is not lost: the
    /// Rust binding carries `#[deprecated]` instead, where the caller who
    /// asked for the function is the one who hears about it. See
    /// google/autocxx#1403.
    pub(crate) calls_deprecated: bool,
    /// Whether this is a subclass peer's override of a superclass virtual
    /// method, and so is declared `override`.
    ///
    /// Every part of the signature is worked out from conversions rather than
    /// copied from the superclass, so a mistake in any of them yields a
    /// well-formed method which simply overrides nothing - it compiles, it is
    /// never called, and the superclass's own implementation runs instead.
    /// `override` turns that silence into a compile error. It goes only on the
    /// in-class declaration; C++ forbids it on an out-of-line definition.
    ///
    /// False for the peer's `_super` helper, which is a new method of its own
    /// rather than an override, and for every function outside a peer class.
    pub(crate) is_virtual_override: bool,
}

/// Every combination of conversions which the rest of autocxx builds, and what
/// each variant of [`TypeConversionPolicy`] answers about it.
///
/// The combinations here are the ones the analysis can produce, gathered by
/// reading every construction site and confirmed by instrumenting them and
/// generating code for a battery of headers covering methods, constructors,
/// placement new, references and mutable references, rvalue parameters and
/// rvalue returns, subclasses, POD and non-POD types, strings and containers,
/// under both unsafety policies and with and without forced wrapper
/// generation.
///
/// What is *not* here is the point of the enum: there is no way to write a
/// conversion which reads a pointee alongside a type which is not a pointer.
/// [`PointerCppConversion`] and [`PointerRustConversion`] exist only inside
/// [`TypeConversionPolicy::Pointer`], which holds the [`BridgePointer`] they
/// read; [`WholeCppConversion`] and [`WholeRustConversion`] exist only inside
/// [`TypeConversionPolicy::Whole`], which holds no pointer and contains no
/// conversion which wants one. Each conversion appears in exactly one of the
/// two families, but for the identity, which means nothing at all in either.
/// A test cannot demonstrate the absence, because the code which would
/// demonstrate it does not compile.
#[cfg(test)]
mod tests {
    use super::*;
    use quote::ToTokens;

    fn ty(tokens: &str) -> Type {
        syn::parse_str(tokens).unwrap()
    }

    fn spelt(ty: &Type) -> String {
        ty.to_token_stream().to_string()
    }

    /// The pointer conversions, paired as the analysis pairs them.
    fn pointer_policies() -> Vec<TypeConversionPolicy> {
        let ptr_mut = BridgePointer::to(ty("Bob"), true);
        let ptr_const = BridgePointer::to(ty("Bob"), false);
        vec![
            // A raw pointer parameter, handed straight over.
            TypeConversionPolicy::pointer(
                ptr_mut.clone(),
                PointerCppConversion::None,
                PointerRustConversion::None,
            ),
            // A destructor's receiver.
            TypeConversionPolicy::pointer(
                ptr_mut.clone(),
                PointerCppConversion::None,
                PointerRustConversion::FromTypeToPtr,
            ),
            // A constructor's receiver.
            TypeConversionPolicy::pointer(
                ptr_mut.clone(),
                PointerCppConversion::None,
                PointerRustConversion::FromPinMaybeUninitToPtr,
            ),
            // A move constructor's source.
            TypeConversionPolicy::pointer(
                ptr_mut.clone(),
                PointerCppConversion::FromPtrToMove,
                PointerRustConversion::FromPinMoveRefToPtr,
            ),
            // The destination a placement-new return is constructed into.
            TypeConversionPolicy::pointer(
                ptr_mut.clone(),
                PointerCppConversion::IgnoredPlacementPtrParameter,
                PointerRustConversion::FromPlacementParamToNewReturn,
            ),
            // A wrapped reference parameter, and its inverse for a subclass.
            TypeConversionPolicy::pointer(
                ptr_mut.clone(),
                PointerCppConversion::FromPointerToReference,
                PointerRustConversion::FromReferenceWrapperToPointer,
            ),
            TypeConversionPolicy::pointer(
                ptr_const.clone(),
                PointerCppConversion::FromReferenceToPointer,
                PointerRustConversion::FromReferenceWrapperToPointer,
            ),
            // A wrapped reference return, and its inverse.
            TypeConversionPolicy::pointer(
                ptr_const.clone(),
                PointerCppConversion::FromReferenceToPointer,
                PointerRustConversion::FromPointerToReferenceWrapper,
            ),
            TypeConversionPolicy::pointer(
                ptr_const,
                PointerCppConversion::FromPointerToReference,
                PointerRustConversion::FromPointerToReferenceWrapper,
            ),
            // An rvalue reference return, wrapped and not, and their inverses.
            TypeConversionPolicy::pointer(
                ptr_mut.clone(),
                PointerCppConversion::FromRValueReferenceToPointer,
                PointerRustConversion::None,
            ),
            TypeConversionPolicy::pointer(
                ptr_mut.clone(),
                PointerCppConversion::FromRValueReferenceToPointer,
                PointerRustConversion::FromPointerToReferenceWrapper,
            ),
            TypeConversionPolicy::pointer(
                ptr_mut.clone(),
                PointerCppConversion::FromPointerToRValueReference,
                PointerRustConversion::None,
            ),
            TypeConversionPolicy::pointer(
                ptr_mut,
                PointerCppConversion::FromPointerToRValueReference,
                PointerRustConversion::FromPointerToReferenceWrapper,
            ),
        ]
    }

    /// The conversions of a type handed on whole, paired as the analysis pairs
    /// them.
    fn whole_policies() -> Vec<TypeConversionPolicy> {
        let subclass = SubclassName::from_holder_name(&crate::types::make_ident("BobHolder"));
        vec![
            TypeConversionPolicy::new_unconverted(ty("Bob")),
            // A subclass holder.
            TypeConversionPolicy::whole(
                ty("rust::Box<BobHolder>"),
                WholeCppConversion::Move,
                WholeRustConversion::ToBoxedUpHolder(subclass),
            ),
            // A POD-safe type with no copy constructor, by value.
            TypeConversionPolicy::whole(
                ty("Bob"),
                WholeCppConversion::Move,
                WholeRustConversion::None,
            ),
            // A POD out of the user's own headers, by value.
            TypeConversionPolicy::whole(
                ty("Bob"),
                WholeCppConversion::MoveOrCopy,
                WholeRustConversion::None,
            ),
            // A std::string parameter, which Rust may hand over as a &str.
            TypeConversionPolicy::whole(
                ty("CxxString"),
                WholeCppConversion::FromUniquePtrToValue,
                WholeRustConversion::FromStr,
            ),
            // The same for a subclass, which takes the simpler route.
            TypeConversionPolicy::whole(
                ty("CxxString"),
                WholeCppConversion::FromUniquePtrToValue,
                WholeRustConversion::None,
            ),
            // A non-POD parameter by value, and an rvalue-reference one.
            TypeConversionPolicy::whole(
                ty("Bob"),
                WholeCppConversion::FromPtrToValue,
                WholeRustConversion::FromValueParamToPtr,
            ),
            TypeConversionPolicy::whole(
                ty("Bob"),
                WholeCppConversion::FromPtrToValue,
                WholeRustConversion::FromRValueParamToPtr,
            ),
            // A non-POD return, and the inverses of the two above.
            TypeConversionPolicy::new_to_unique_ptr(ty("Bob")),
            TypeConversionPolicy::whole(
                ty("CxxString"),
                WholeCppConversion::FromValueToUniquePtr,
                WholeRustConversion::FromStr,
            ),
            // A return which is emplaced into a caller's pointer instead.
            TypeConversionPolicy::new_for_placement_return(ty("Bob")),
        ]
    }

    /// The bridge type comes back out spelt as the bridge would spell it,
    /// whichever variant holds it. For a pointer that means rebuilding it from
    /// the pointee and the mutability, which is where the old free-floating
    /// `syn::Type` used to be read back.
    #[test]
    fn every_policy_names_its_bridge_type() {
        for policy in pointer_policies() {
            let spelling = spelt(&policy.cxxbridge_type());
            assert!(
                spelling == "* mut Bob" || spelling == "* const Bob",
                "{spelling}"
            );
        }
        for policy in whole_policies() {
            assert!(!spelt(&policy.cxxbridge_type()).is_empty());
        }
    }

    /// What each conversion inverts to, in full. Three of them lose
    /// information on the way - a `MoveOrCopy` and a `FromPtrToValue` both
    /// invert to something whose own inverse is a third thing - so this is a
    /// table rather than a round trip, and the table is what a subclass peer's
    /// C++ signature is built from.
    #[test]
    fn every_conversion_inverts_to_the_expected_one() {
        let inverses: Vec<String> = pointer_policies()
            .into_iter()
            .chain(whole_policies())
            .map(|policy| match policy.inverse() {
                Some(TypeConversionPolicy::Pointer { cpp, .. }) => format!("{cpp:?}"),
                Some(TypeConversionPolicy::Whole { cpp, .. }) => format!("{cpp:?}"),
                None => "-".to_string(),
            })
            .collect();
        assert_eq!(
            inverses,
            vec![
                // The pointer conversions, in the order they are built above.
                "None",
                "None",
                "None",
                "-",
                "-",
                "FromReferenceToPointer",
                "FromPointerToReference",
                "FromPointerToReference",
                "FromReferenceToPointer",
                "FromPointerToRValueReference",
                "FromPointerToRValueReference",
                "FromRValueReferenceToPointer",
                "FromRValueReferenceToPointer",
                // Then the conversions of a type handed on whole.
                "None",
                "Move",
                "Move",
                "None",
                "FromValueToUniquePtr",
                "FromValueToUniquePtr",
                "FromValueToUniquePtr",
                "FromValueToUniquePtr",
                "FromUniquePtrToValue",
                "FromUniquePtrToValue",
                "-",
            ]
        );
    }

    /// Inverting a conversion leaves the bridge type and the Rust side of the
    /// conversion alone: only what C++ does with the value has an opposite.
    #[test]
    fn inversion_changes_only_the_cpp_side() {
        for policy in pointer_policies().into_iter().chain(whole_policies()) {
            let Some(inverted) = policy.inverse() else {
                continue;
            };
            assert_eq!(
                spelt(&policy.cxxbridge_type()),
                spelt(&inverted.cxxbridge_type())
            );
            assert_eq!(policy.rust_work_needed(), inverted.rust_work_needed());
        }
    }

    /// The conversions with no opposite, named. A subclass peer built from one
    /// of these is what the callers of `inverse` refuse.
    #[test]
    fn the_conversions_without_an_opposite_are_the_expected_three() {
        let without: Vec<String> = pointer_policies()
            .into_iter()
            .chain(whole_policies())
            .filter(|policy| policy.inverse().is_none())
            .map(|policy| match policy {
                TypeConversionPolicy::Pointer { cpp, .. } => format!("{cpp:?}"),
                TypeConversionPolicy::Whole { cpp, .. } => format!("{cpp:?}"),
            })
            .collect();
        assert_eq!(
            without,
            vec![
                "FromPtrToMove",
                "IgnoredPlacementPtrParameter",
                "FromReturnValueToPlacementPtr",
            ]
        );
    }

    /// A pointer is read back as the two halves it was built from, and
    /// anything which is not one is turned away rather than mistaken for one.
    #[test]
    fn a_bridge_pointer_is_only_ever_a_pointer() {
        let pointer = BridgePointer::from_type(&ty("*const Bob")).expect("that is a pointer");
        assert_eq!(spelt(pointer.pointee()), "Bob");
        assert!(!pointer.is_mut());
        assert_eq!(spelt(&pointer.ty()), "* const Bob");
        let pointer = BridgePointer::from_type(&ty("*mut Bob")).expect("that is a pointer");
        assert!(pointer.is_mut());
        assert_eq!(spelt(&pointer.ty()), "* mut Bob");
        assert!(BridgePointer::from_type(&ty("Bob")).is_none());
        assert!(BridgePointer::from_type(&ty("&Bob")).is_none());
    }

    /// The three shapes a returned C++ reference reaches the conversion in all
    /// name the same pointer, and a fourth shape is refused with a reason.
    #[test]
    fn a_returned_reference_is_read_in_each_shape_it_arrives_in() {
        for (arrives_as, expected) in [
            ("*mut Bob", "* mut Bob"),
            ("&Bob", "* const Bob"),
            ("&mut Bob", "* mut Bob"),
            ("std::pin::Pin<&mut Bob>", "* mut Bob"),
        ] {
            let pointer = reference_return_as_pointer(&ty(arrives_as)).expect(arrives_as);
            assert_eq!(spelt(&pointer.ty()), expected, "{arrives_as}");
        }
        assert!(matches!(
            reference_return_as_pointer(&ty("Bob")),
            Err(ConvertErrorFromCpp::UnexpectedReferenceReturn(_))
        ));
    }

    /// A forced conversion which needs a pointer is refused where the type
    /// converter produced none, rather than travelling on to crash in codegen.
    #[test]
    fn a_forced_pointer_conversion_needs_a_pointer() {
        assert!(matches!(
            ForcedRustConversion::Identity.on_whole(&ty("Bob")),
            Ok(WholeRustConversion::None)
        ));
        assert!(matches!(
            ForcedRustConversion::Pointer(PointerRustConversion::FromTypeToPtr)
                .on_whole(&ty("Bob")),
            Err(ConvertErrorFromCpp::ParameterWasNotAPointer(_))
        ));
    }
}
