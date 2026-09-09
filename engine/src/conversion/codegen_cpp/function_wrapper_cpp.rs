// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use syn::{Type, TypeReference};

use crate::conversion::{
    analysis::fun::function_wrapper::{
        BridgePointer, PointerCppConversion, TypeConversionPolicy, WholeCppConversion,
    },
    ConvertErrorFromCpp,
};

use super::type_to_cpp::CppNameMap;

/// The C++ cxx spells a Rust `&[u8]` as - cxx-gen's `write.rs` writes
/// `::rust::Slice<`, the element type, then `const`. The wrapper's parameter
/// has to be this exact type: cxx checks its own shim against ours by
/// assigning it to a function pointer.
const RUST_BYTE_SLICE: &str = "::rust::Slice<::std::uint8_t const>";

impl TypeConversionPolicy {
    pub(super) fn unconverted_type(
        &self,
        cpp_name_map: &CppNameMap,
    ) -> Result<String, ConvertErrorFromCpp> {
        match self {
            Self::Whole {
                cpp: WholeCppConversion::FromUniquePtrToValue,
                ..
            } => self.unique_ptr_wrapped_type(cpp_name_map),
            Self::Whole {
                cpp: WholeCppConversion::FromPtrToValue,
                ..
            } => Ok(format!("{}*", self.unwrapped_type_as_string(cpp_name_map)?)),
            // What cxx hands the wrapper for the bridge's `&[u8]`. Spelt here
            // rather than derived from the bridge type, because the type this
            // conversion is *about* is the `std::string_view` it produces.
            Self::Whole {
                cpp: WholeCppConversion::FromRustBytesToStringView,
                ..
            } => Ok(RUST_BYTE_SLICE.to_string()),
            // `&var`. What this conversion is handed is the C++ reference;
            // the pointer in `cxxbridge_type` is what it produces.
            Self::Pointer {
                pointer,
                cpp: PointerCppConversion::FromReferenceToPointer,
                ..
            } => reference_type(pointer, cpp_name_map),
            // Likewise, but the reference is an rvalue one. Nothing reaches
            // this today: the only reader of an rvalue return's unconverted
            // type would be a `_super` helper, and a method returning `T&&`
            // never gets one. It is here because it is the answer if one ever
            // does, and `T*` - what the fall-through would say - is not.
            Self::Pointer {
                pointer,
                cpp: PointerCppConversion::FromRValueReferenceToPointer,
                ..
            } => rvalue_reference_type(pointer, cpp_name_map),
            _ => self.unwrapped_type_as_string(cpp_name_map),
        }
    }

    pub(super) fn converted_type(
        &self,
        cpp_name_map: &CppNameMap,
    ) -> Result<String, ConvertErrorFromCpp> {
        match self {
            Self::Whole {
                cpp: WholeCppConversion::FromValueToUniquePtr,
                ..
            } => self.unique_ptr_wrapped_type(cpp_name_map),
            Self::Pointer {
                pointer,
                cpp:
                    PointerCppConversion::FromReferenceToPointer
                    | PointerCppConversion::FromRValueReferenceToPointer,
                ..
            } => pointee_type(pointer, cpp_name_map, "*"),
            // `(*var)`. What this conversion produces is the C++ reference the
            // underlying function asked for, not the pointer it was handed.
            Self::Pointer {
                pointer,
                cpp: PointerCppConversion::FromPointerToReference,
                ..
            } => reference_type(pointer, cpp_name_map),
            // Likewise, but the reference is an rvalue one.
            Self::Pointer {
                pointer,
                cpp: PointerCppConversion::FromPointerToRValueReference,
                ..
            } => rvalue_reference_type(pointer, cpp_name_map),
            _ => self.unwrapped_type_as_string(cpp_name_map),
        }
    }

    fn unwrapped_type_as_string(
        &self,
        cpp_name_map: &CppNameMap,
    ) -> Result<String, ConvertErrorFromCpp> {
        cpp_name_map.type_to_cpp(&self.cxxbridge_type())
    }

    fn unique_ptr_wrapped_type(
        &self,
        original_name_map: &CppNameMap,
    ) -> Result<String, ConvertErrorFromCpp> {
        Ok(format!(
            "std::unique_ptr<{}>",
            self.unwrapped_type_as_string(original_name_map)?
        ))
    }

    pub(super) fn cpp_conversion(
        &self,
        var_name: &str,
        cpp_name_map: &CppNameMap,
        is_return: bool,
    ) -> Result<Option<String>, ConvertErrorFromCpp> {
        // If is_return we want to avoid unnecessary std::moves because they
        // make RVO less effective
        Ok(match self {
            Self::Pointer {
                cpp: PointerCppConversion::None,
                ..
            }
            | Self::Whole {
                cpp: WholeCppConversion::None | WholeCppConversion::FromReturnValueToPlacementPtr,
                ..
            } => Some(var_name.to_string()),
            Self::Pointer {
                cpp: PointerCppConversion::FromPointerToReference,
                ..
            } => Some(format!("(*{var_name})")),
            Self::Whole {
                cpp: WholeCppConversion::Move,
                ..
            } => Some(format!("std::move({var_name})")),
            // A move constructor has to have a move constructor to call, so
            // `std::move` says exactly what is meant here.
            Self::Pointer {
                cpp: PointerCppConversion::FromPtrToMove,
                ..
            } => Some(format!("std::move(*{var_name})")),
            // Whereas these two are handing an ordinary parameter over by
            // value out of storage the Rust side owns and is about to destroy,
            // so a move is merely an optimization and must give way to a copy
            // for types whose move constructor is deleted. Name the helper
            // from the global namespace, or the argument's own namespaces
            // could offer a better-matching function of that name.
            Self::Whole {
                cpp: WholeCppConversion::FromUniquePtrToValue,
                ..
            } => Some(format!("::autocxx_move_or_copy(*{var_name})")),
            // And this one is the wrapper's own by-value parameter, which is
            // about to be destroyed too, so the same reasoning applies. Passing
            // it bare would ask for a copy constructor, which a POD type that
            // opts into relocatability by writing its own move constructor no
            // longer has. See google/autocxx#1252.
            Self::Whole {
                cpp: WholeCppConversion::MoveOrCopy,
                ..
            } => Some(format!("::autocxx_move_or_copy({var_name})")),
            Self::Whole {
                cpp: WholeCppConversion::FromValueToUniquePtr,
                ..
            } => Some(format!(
                "std::make_unique<{}>({})",
                self.unconverted_type(cpp_name_map)?,
                var_name
            )),
            Self::Whole {
                cpp: WholeCppConversion::FromPtrToValue,
                ..
            } => {
                let dereference = format!("*{var_name}");
                Some(if is_return {
                    dereference
                } else {
                    format!("::autocxx_move_or_copy({dereference})")
                })
            }
            // The view over the bytes Rust lent. `reinterpret_cast` because
            // cxx carries them as `uint8_t` and `string_view` wants `char`:
            // examining an object's bytes through `char` is what that cast is
            // defined for. The two-argument constructor rather than a
            // `strlen`-style one, because these bytes are not NUL-terminated
            // and may contain NUL, and the length is already known.
            //
            // The empty case is spelt separately. `string_view(p, n)` requires
            // `[p, p + n)` to be a valid range, and an empty Rust slice need
            // not carry a pointer to anything - `Vec::new()` leaves a dangling
            // well-aligned one, and cxx passes the pointer through as it found
            // it (`cxx.h`'s `Slice::data`). `p + 0` on such a pointer is not
            // something C++ promises anything about, so an empty slice becomes
            // an empty view rather than a view over that address.
            Self::Whole {
                cpp: WholeCppConversion::FromRustBytesToStringView,
                ..
            } => Some(format!(
                "({var_name}.empty() ? ::std::string_view() \
                 : ::std::string_view(reinterpret_cast<const char*>({var_name}.data()), \
                 {var_name}.size()))"
            )),
            Self::Pointer {
                cpp: PointerCppConversion::IgnoredPlacementPtrParameter,
                ..
            } => None,
            // `addressof` rather than `&`, because a class may overload
            // `operator&` and one which does hands back whatever it likes -
            // the address of the referent is then not what `&` produces. The
            // `<memory>` this needs is among the headers every function
            // wrapper already gets.
            Self::Pointer {
                cpp: PointerCppConversion::FromReferenceToPointer,
                ..
            } => Some(format!("::std::addressof({var_name})")),
            // `const_cast` is the only cast which removes `volatile`, and the
            // bridge has no way to spell the qualifier, so the wrapper drops it
            // here and Rust is handed an `autocxx::VolatilePtr<T>` which puts
            // the discipline back. The cast changes no address and reads
            // nothing; the storage stays as volatile as C++ declared it.
            //
            // Its target is the bridge's own pointer type rather than one
            // assembled from the pointee and a `*`, because assembling puts a
            // qualifier at the wrong level as soon as the pointee is itself a
            // pointer: `T* const*` is what `T* const volatile*` wants, and
            // prepending `const` to `T**` says `const T**`.
            Self::Pointer {
                cpp: PointerCppConversion::FromVolatilePointerToPointer,
                ..
            } => Some(format!(
                "const_cast<{}>({var_name})",
                self.unwrapped_type_as_string(cpp_name_map)?
            )),
            // The same, for a `volatile T&` return: the address first, by the
            // same `addressof` reasoning as above, then the cast.
            Self::Pointer {
                cpp: PointerCppConversion::FromVolatileReferenceToPointer,
                ..
            } => Some(format!(
                "const_cast<{}>(::std::addressof({var_name}))",
                self.unwrapped_type_as_string(cpp_name_map)?
            )),
            // Going the other way, the qualifier has to be put back *before*
            // the call, or overload resolution never sees it - see
            // `PointerCppConversion::FromPointerToVolatilePointer`.
            Self::Pointer {
                pointer,
                cpp: PointerCppConversion::FromPointerToVolatilePointer,
                ..
            } => Some(format!(
                "static_cast<{}>({var_name})",
                volatile_pointee_type(pointer, cpp_name_map, "*")?
            )),
            Self::Pointer {
                pointer,
                cpp: PointerCppConversion::FromPointerToVolatileReference,
                ..
            } => Some(format!(
                "static_cast<{}>(*{var_name})",
                volatile_pointee_type(pointer, cpp_name_map, "&")?
            )),
            // The rvalue counterpart of `FromPointerToReference`'s `(*var)`.
            // `static_cast` rather than `std::move` because that is precisely
            // what this is - the two are the same operation, and the cast
            // needs no header and spells the resulting type where the reader
            // can see it against the signature it has to match.
            Self::Pointer {
                pointer,
                cpp: PointerCppConversion::FromPointerToRValueReference,
                ..
            } => Some(format!(
                "static_cast<{}>(*{var_name})",
                rvalue_reference_type(pointer, cpp_name_map)?
            )),
            // Which leaves the other direction, and there is no expression of
            // this shape for it. `&` wants an lvalue; neither a
            // `T&&`-returning call nor a `static_cast<T&>` of one is that, so
            // the result would first have to be given a name - `T&& r = f();
            // return &r;` - and a wrapper body here is one expression built
            // around the call, with nowhere to put a name. Nothing should
            // ask: analysis refuses such a function twice over, once on the
            // `T&&` bindgen spelled and once on what the type converter made
            // of a typedef to one, and the direction which does get generated
            // - a subclass peer's override, calling from C++ into Rust -
            // inverts this conversion into the arm above. Answer with that
            // same refusal rather than assert, so that a route through which
            // neither of those two catches reaches whoever hit it as the
            // limitation it is rather than as a crash.
            Self::Pointer {
                cpp: PointerCppConversion::FromRValueReferenceToPointer,
                ..
            } => return Err(ConvertErrorFromCpp::RValueReturn),
        })
    }

    /// Whether [`Self::cpp_conversion`] names `std::string_view`, so that
    /// callers know to ask for `<string_view>` and for the C++17 check which
    /// goes with it.
    pub(super) fn builds_a_string_view(&self) -> bool {
        matches!(
            self,
            Self::Whole {
                cpp: WholeCppConversion::FromRustBytesToStringView,
                ..
            }
        )
    }

    /// Whether [`Self::cpp_conversion`] may emit a call to the
    /// `autocxx_move_or_copy` helper, so that callers know to emit its
    /// definition. Keep in step with that function; over-reporting only costs
    /// an unused template definition, under-reporting fails to compile.
    pub(super) fn may_use_move_or_copy_helper(&self) -> bool {
        matches!(
            self,
            Self::Whole {
                cpp: WholeCppConversion::FromUniquePtrToValue
                    | WholeCppConversion::FromPtrToValue
                    | WholeCppConversion::MoveOrCopy,
                ..
            }
        )
    }
}

/// The C++ reference which `pointer` stands for, for the two conversions which
/// turn one into the other. Spelled by asking for the reference type itself, so
/// that the referents which get a name of their own - `str`, which is
/// `rust::Str` - keep it here too.
fn reference_type(
    pointer: &BridgePointer,
    cpp_name_map: &CppNameMap,
) -> Result<String, ConvertErrorFromCpp> {
    let ty = Type::Reference(TypeReference {
        and_token: Default::default(),
        lifetime: None,
        mutability: pointer.is_mut().then(Default::default),
        elem: Box::new(pointer.pointee().clone()),
    });
    cpp_name_map.type_to_cpp(&ty)
}

/// The C++ rvalue reference which `pointer` stands for, for the two conversions
/// which turn one into the other.
///
/// Not spelled via a `syn` type as [`reference_type`] is, because Rust has
/// nothing which means `T&&`: that is the whole reason those two conversions
/// exist. That also means this does not inherit the `str` case above, and must
/// not: a subclass peer overriding a `virtual rust::Str&& f()` has to write
/// `rust::Str&&`, whereas the reference spelling of the same pointer is the
/// bare `rust::Str`, because that is how cxx passes a `&str` - by value, the
/// `&` being part of the Rust type rather than a C++ reference. Both spellings
/// are generated today and neither would do for the other's job.
fn rvalue_reference_type(
    pointer: &BridgePointer,
    cpp_name_map: &CppNameMap,
) -> Result<String, ConvertErrorFromCpp> {
    pointee_type(pointer, cpp_name_map, "&&")
}

/// What `pointer` points at with `volatile` put back on it, plus its constness,
/// and `suffix` - `*` or `&` - appended. This is the type the C++ function was
/// declared with, which the call has to name to be resolved against it.
///
/// Prepending the qualifiers is only correct because a pointee here is never
/// itself a pointer - `readable_by_rust_out_of_volatile` admits only the
/// built-in scalars - since a qualifier written on a pointer binds to the
/// declarator instead.
fn volatile_pointee_type(
    pointer: &BridgePointer,
    cpp_name_map: &CppNameMap,
    suffix: &str,
) -> Result<String, ConvertErrorFromCpp> {
    let const_string = if pointer.is_mut() { "" } else { "const " };
    Ok(format!(
        "{}volatile {}{}",
        const_string,
        cpp_name_map.type_to_cpp(pointer.pointee())?,
        suffix
    ))
}

/// What `pointer` points at, with its constness restored and `suffix` - `*` or
/// `&&` - appended.
fn pointee_type(
    pointer: &BridgePointer,
    cpp_name_map: &CppNameMap,
    suffix: &str,
) -> Result<String, ConvertErrorFromCpp> {
    let const_string = if pointer.is_mut() { "" } else { "const " };
    Ok(format!(
        "{}{}{}",
        const_string,
        cpp_name_map.type_to_cpp(pointer.pointee())?,
        suffix
    ))
}
