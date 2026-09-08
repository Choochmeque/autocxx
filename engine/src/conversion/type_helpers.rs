// Copyright 2023 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use syn::{
    AngleBracketedGenericArguments, GenericArgument, Path, PathArguments, PathSegment, Type,
    TypePath, TypePtr, TypeReference,
};

/// Looks in a `core::pin::Pin<&mut Something>` and returns the `Something`
/// if it's found.
/// This code could _almost_ be used from various other places around autocxx
/// but they each have slightly different requirements. Over time we should
/// try to migrate other instances to use this, though.
pub(crate) fn extract_pinned_mutable_reference_type(tp: &TypePath) -> Option<&Type> {
    if !is_pin(tp) {
        return None;
    }
    if let Some(PathSegment {
        arguments: PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }),
        ..
    }) = tp.path.segments.last()
    {
        if args.len() == 1 {
            if let Some(GenericArgument::Type(Type::Reference(TypeReference {
                mutability: Some(_),
                elem,
                ..
            }))) = args.first()
            {
                return Some(elem);
            }
        }
    }
    None
}

/// The innermost element type of `ty` if it is an array, and `ty` itself if it
/// is not.
///
/// C++ can nest arrays - `T arr[2][3]` reaches us as `[[T; 3]; 2]` - and this
/// peels off every dimension, so it answers `T` rather than `[T; 3]`. Holding
/// an array by value holds its elements by value however deeply they nest, so
/// wherever an analysis asks what a field is made of, the answer for an array
/// is whatever this returns.
pub(crate) fn array_element_type(mut ty: &Type) -> &Type {
    while let Type::Array(arr) = ty {
        ty = &arr.elem;
    }
    ty
}

/// Whether this type path is a `Pin`
fn is_pin(tp: &TypePath) -> bool {
    if tp.path.segments.len() != 3 {
        return false;
    }
    static EXPECTED_SEGMENTS: &[&[&str]] = &[&["std", "core"], &["pin"], &["Pin"]];

    for (seg, expected_name) in tp.path.segments.iter().zip(EXPECTED_SEGMENTS.iter()) {
        if !expected_name
            .iter()
            .any(|expected_name| seg.ident == expected_name)
        {
            return false;
        }
    }
    true
}

fn marker_for_reference(search_for_rvalue: bool) -> &'static str {
    if search_for_rvalue {
        "__bindgen_marker_RValueReference"
    } else {
        "__bindgen_marker_Reference"
    }
}

pub(crate) fn type_is_reference(ty: &syn::Type, search_for_rvalue: bool) -> bool {
    matches_bindgen_marker(ty, marker_for_reference(search_for_rvalue))
}

fn matches_bindgen_marker(ty: &syn::Type, marker_name: &str) -> bool {
    matches!(&ty, Type::Path(TypePath {
                  path: Path { segments, .. },..
               }) if segments.first().map(|seg| seg.ident == marker_name).unwrap_or_default())
}

/// If `seg` is `Wrapper<Inner>` for the given wrapper name, return `Inner`.
fn unwrap_newtype<'a>(seg: &'a PathSegment, wrapper_name: &str) -> Option<&'a syn::Type> {
    if seg.ident != wrapper_name {
        return None;
    }
    let PathArguments::AngleBracketed(ref angle_bracketed_args) = seg.arguments else {
        return None;
    };
    match angle_bracketed_args.args.first()? {
        GenericArgument::Type(ty) => Some(ty),
        _ => None,
    }
}

fn unwrap_bindgen_marker<'a>(ty: &'a TypePath, marker_name: &str) -> Option<&'a syn::Type> {
    unwrap_newtype(ty.path.segments.first()?, marker_name)
}

pub(crate) fn unwrap_reference(ty: &TypePath, search_for_rvalue: bool) -> Option<&syn::TypePtr> {
    match unwrap_bindgen_marker(ty, marker_for_reference(search_for_rvalue)) {
        // Our behavior here if we see __bindgen_marker_Reference <something that isn't a pointer>
        // is to ignore the type. This should never happen.
        Some(Type::Ptr(typ)) => Some(typ),
        _ => None,
    }
}

pub(crate) fn unwrap_has_opaque(ty: &TypePath) -> Option<&syn::Type> {
    unwrap_bindgen_marker(ty, "__bindgen_marker_Opaque")
}

/// If `ty` is a type C++ qualified `const` in its own right - `const int`,
/// `T* const`, a `const`-qualified return - return the type it qualifies.
///
/// Rust has no spelling for that qualifier, so bindgen would drop it without
/// the marker: `const int` and `int` are both `c_int`, and `T* const` and `T*`
/// are both `*mut T`. Constness of a *pointee* is a different thing and needs
/// no marker, because `*const T` says it.
pub(crate) fn unwrap_const(ty: &TypePath) -> Option<&syn::Type> {
    unwrap_bindgen_marker(ty, "__bindgen_marker_Const")
}

/// If `ty` is a C++ `long double`, return the Rust type bindgen substituted
/// for it - which is the right size and nothing else. See
/// [`crate::conversion::ConvertErrorFromCpp::LongDouble`] for why that is not
/// enough to put in a signature.
pub(crate) fn unwrap_long_double(ty: &TypePath) -> Option<&syn::Type> {
    unwrap_bindgen_marker(ty, "__bindgen_marker_LongDouble")
}

/// Whether `ty` mentions a C++ `long double` anywhere inside it, including as
/// a template argument.
///
/// [`unwrap_long_double`] only sees a marker the type converter recursed into,
/// and it does not recurse into the arguments of a template it is about to
/// name in C++ verbatim. Without this the marker reaches the generated header
/// as a literal `Wrapper<__bindgen_marker_LongDouble<double>>`.
pub(crate) fn mentions_long_double(ty: &Type) -> bool {
    match ty {
        Type::Path(typ) => {
            if unwrap_long_double(typ).is_some() {
                return true;
            }
            typ.path.segments.iter().any(|seg| {
                let PathArguments::AngleBracketed(args) = &seg.arguments else {
                    return false;
                };
                args.args.iter().any(|arg| match arg {
                    GenericArgument::Type(inner) => mentions_long_double(inner),
                    _ => false,
                })
            })
        }
        Type::Array(arr) => mentions_long_double(&arr.elem),
        Type::Ptr(ptr) => mentions_long_double(&ptr.elem),
        Type::Reference(r) => mentions_long_double(&r.elem),
        // bindgen writes a C function pointer as `Option<unsafe extern "C"
        // fn(..)>`, so this is reached through the `Option`'s argument above.
        Type::BareFn(f) => {
            f.inputs.iter().any(|arg| mentions_long_double(&arg.ty))
                || match &f.output {
                    syn::ReturnType::Type(_, ty) => mentions_long_double(ty),
                    syn::ReturnType::Default => false,
                }
        }
        Type::Paren(inner) => mentions_long_double(&inner.elem),
        Type::Group(inner) => mentions_long_double(&inner.elem),
        _ => false,
    }
}

/// Peels bindgen's `const` markers off `ty`, for the walks which care what a
/// type is laid out as rather than whether C++ let anyone write to it. A
/// `const T` occupies exactly what a `T` does.
pub(crate) fn strip_const_markers(mut ty: &Type) -> &Type {
    while let Type::Path(typ) = ty {
        match unwrap_const(typ) {
            Some(inner) => ty = inner,
            None => break,
        }
    }
    ty
}

/// [`array_element_type`], for output which may have `const` markers in it.
///
/// The two have to be peeled together rather than one after the other: bindgen
/// folds a `const` element type into the array's own constness as well as
/// leaving it on the element (`ir/ty.rs`'s `from_clang_ty`), so
/// `const int a[2][3]` arrives as markers and array layers alternating all the
/// way down, and stopping at the first of either finds nothing useful.
pub(crate) fn unqualified_array_element_type(ty: &Type) -> &Type {
    let mut ty = strip_const_markers(ty);
    while let Type::Array(arr) = ty {
        ty = strip_const_markers(&arr.elem);
    }
    ty
}

/// Whether `ty` is a C++ array, or reaches one through the indirections a
/// signature can be written with.
///
/// cxx spells a Rust `[T; N]` as `std::array<T, N>`, which is a different C++
/// type from the `T[N]` bindgen wrote it for. A struct field is unaffected -
/// the struct is defined by the C++ header, and cxx only checks its layout -
/// but a function signature is not: the bridge declares a parameter of
/// `std::array` and then takes the address of a function which has no such
/// parameter, which C++ refuses.
///
/// An array *parameter* is not this. C++ decays one to a pointer before
/// bindgen sees the declaration, so it arrives already a pointer and is bound
/// as one; `test_take_array` is that case. What is left are the shapes which
/// keep the array type, and they arrive spelled four ways: the array itself,
/// `&[T; N]` for `const T (&)[N]`, `Pin<&mut [T; N]>` for `T (&)[N]`, and
/// `*mut [T; N]` for a pointer whose pointee only becomes an array after
/// `ensure_pointee_is_valid` has looked - which is what an alias does, as in
/// `using A = T[N]; void f(A*)`.
pub(crate) fn denotes_cpp_array(ty: &Type) -> bool {
    match ty {
        Type::Array(_) => true,
        Type::Reference(TypeReference { elem, .. }) => denotes_cpp_array(elem),
        Type::Ptr(TypePtr { elem, .. }) => denotes_cpp_array(elem),
        Type::Path(typ) => {
            matches!(extract_pinned_mutable_reference_type(typ), Some(inner) if denotes_cpp_array(inner))
        }
        _ => false,
    }
}

/// If `ty` is `root::__BindgenBitfieldUnit<[u8; N]>` - the allocation unit
/// bindgen puts a run of C++ bitfields into - return the `[u8; N]` storage it
/// wraps.
///
/// Unlike the `__bindgen_marker_` types above, this one is a real struct
/// bindgen emits, so it is namespaced under `root` and we insist on exactly
/// that shape rather than matching any segment.
pub(crate) fn unwrap_bitfield(ty: &TypePath) -> Option<&syn::Type> {
    let mut segments = ty.path.segments.iter();
    if segments.next()?.ident != "root" {
        return None;
    }
    let inner = unwrap_newtype(segments.next()?, "__BindgenBitfieldUnit");
    if segments.next().is_some() {
        return None;
    }
    inner
}

/// If `ty` is `Option<F>` for a bare function type `F`, return that function
/// type. This is the shape bindgen gives a C function pointer, because a null
/// pointer is one of the values one can hold.
///
/// bindgen writes the path out as `::std::option::Option`; the shorter
/// spellings of the same path are accepted too, but nothing else, because a
/// C++ class called `Option` reaches us as `root::Option`.
pub(crate) fn unwrap_function_pointer(ty: &TypePath) -> Option<&syn::TypeBareFn> {
    if ty.qself.is_some() {
        return None;
    }
    let mut segments = ty.path.segments.iter().rev();
    let last = segments.next()?;
    if !segments.all(|seg| matches!(seg.ident.to_string().as_str(), "std" | "core" | "option")) {
        return None;
    }
    match unwrap_newtype(last, "Option")? {
        Type::BareFn(fun) => Some(fun),
        _ => None,
    }
}

/// Whether `ty` is a pointer as far as copying it is concerned: one written
/// out, or a C function pointer in the `Option<unsafe extern "C" fn(..)>`
/// shape bindgen gives it. Either is trivially copyable whatever it points at.
pub(crate) fn is_pointer_like(ty: &Type) -> bool {
    match ty {
        Type::Ptr(_) => true,
        Type::Path(typ) => unwrap_function_pointer(typ).is_some(),
        _ => false,
    }
}
