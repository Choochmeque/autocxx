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

/// If `ty` is a type C++ qualified `volatile` in its own right, return the
/// type it qualifies.
///
/// Rust has no spelling for the qualifier at any depth. `const` has one escape,
/// since a `const` pointee survives as `*const T`; `volatile` has none, there
/// being no volatile pointer type, so without the marker bindgen drops the fact
/// everywhere.
pub(crate) fn unwrap_volatile(ty: &TypePath) -> Option<&syn::Type> {
    unwrap_bindgen_marker(ty, "__bindgen_marker_Volatile")
}

/// If `ty` is a C++ `long double`, return the Rust type bindgen substituted
/// for it - which is the right size and nothing else. See
/// [`crate::conversion::ConvertErrorFromCpp::LongDouble`] for why that is not
/// enough to put in a signature.
pub(crate) fn unwrap_long_double(ty: &TypePath) -> Option<&syn::Type> {
    unwrap_bindgen_marker(ty, "__bindgen_marker_LongDouble")
}

/// If `ty` is a C++ `__float128`, return the Rust type bindgen substituted for
/// it - `u128`, which is the right size and the wrong kind of number. See
/// [`crate::conversion::ConvertErrorFromCpp::Float128`].
pub(crate) fn unwrap_float128(ty: &TypePath) -> Option<&syn::Type> {
    unwrap_bindgen_marker(ty, "__bindgen_marker_Float128")
}

/// Whether `ty` mentions a C++ `long double` anywhere inside it, including as
/// a template argument.
///
/// [`unwrap_long_double`] only sees a marker the type converter recursed into,
/// and it does not recurse into the arguments of a template it is about to
/// name in C++ verbatim. Without this the marker reaches the generated header
/// as a literal `Wrapper<__bindgen_marker_LongDouble<double>>`.
pub(crate) fn mentions_long_double(ty: &Type) -> bool {
    mentions_marker(ty, "__bindgen_marker_LongDouble")
}

/// [`mentions_long_double`], for the other float autocxx turns down by name.
pub(crate) fn mentions_float128(ty: &Type) -> bool {
    mentions_marker(ty, "__bindgen_marker_Float128")
}

/// Whether `ty` is, or contains, a type C++ qualified `volatile` - a pointee
/// or a template argument included.
///
/// For a template argument that breadth is the point: the argument is written
/// back out verbatim to name the instantiation in C++, so a qualifier anywhere
/// inside it makes the name a different specialization. Where the question is
/// instead whether *this* object is volatile, ask
/// [`is_volatile_qualified`] - a pointer to something volatile is not itself
/// volatile, and reading it is an ordinary read.
pub(crate) fn mentions_volatile(ty: &Type) -> bool {
    mentions_marker(ty, "__bindgen_marker_Volatile")
}

/// Whether C++ qualified `ty` itself `volatile`, as opposed to something it
/// points at or is parameterized over.
///
/// Looks through the `const` marker, because `const volatile` carries both and
/// bindgen nests them in the order C++ writes them.
pub(crate) fn is_volatile_qualified(ty: &Type) -> bool {
    matches!(strip_const_markers(ty), Type::Path(typ) if unwrap_volatile(typ).is_some())
}

fn mentions_marker(ty: &Type, marker: &str) -> bool {
    match ty {
        Type::Path(typ) => {
            if unwrap_bindgen_marker(typ, marker).is_some() {
                return true;
            }
            typ.path.segments.iter().any(|seg| {
                let PathArguments::AngleBracketed(args) = &seg.arguments else {
                    return false;
                };
                args.args.iter().any(|arg| match arg {
                    GenericArgument::Type(inner) => mentions_marker(inner, marker),
                    _ => false,
                })
            })
        }
        Type::Array(arr) => mentions_marker(&arr.elem, marker),
        Type::Ptr(ptr) => mentions_marker(&ptr.elem, marker),
        Type::Reference(r) => mentions_marker(&r.elem, marker),
        // bindgen writes a C function pointer as `Option<unsafe extern "C"
        // fn(..)>`, so this is reached through the `Option`'s argument above.
        Type::BareFn(f) => {
            f.inputs.iter().any(|arg| mentions_marker(&arg.ty, marker))
                || match &f.output {
                    syn::ReturnType::Type(_, ty) => mentions_marker(ty, marker),
                    syn::ReturnType::Default => false,
                }
        }
        Type::Paren(inner) => mentions_marker(&inner.elem, marker),
        Type::Group(inner) => mentions_marker(&inner.elem, marker),
        _ => false,
    }
}

/// If `ty` is the array a C++ `std::array<T, N>` was lowered to, return the
/// `[T; N]` itself.
///
/// bindgen writes `[T; N]` for the class and for the C array `T[N]` alike, and
/// the marker is the only thing which says which of the two a type was. What
/// needs to know is a reference: `const T (&)[N]` and `const std::array<T, N>&`
/// are different C++ types, and cxx writes the second for either.
pub(crate) fn unwrap_std_array(ty: &TypePath) -> Option<&syn::Type> {
    unwrap_bindgen_marker(ty, "__bindgen_marker_StdArray")
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

/// Peels the markers which say something about a type without changing what it
/// is laid out as: the `const` qualifier, and the `std::array` a `[T; N]` came
/// from. Both wrap the type they describe, and both are transparent.
///
/// For the walks which read a struct's fields straight off bindgen's output,
/// before the type converter has unwrapped anything for them.
pub(crate) fn strip_layout_markers(mut ty: &Type) -> &Type {
    loop {
        let Type::Path(typ) = ty else { return ty };
        match unwrap_const(typ).or_else(|| unwrap_std_array(typ)) {
            Some(inner) => ty = inner,
            None => return ty,
        }
    }
}

/// [`array_element_type`], for output which may have markers in it.
///
/// Markers and array layers have to be peeled together rather than one after
/// the other: bindgen folds a `const` element type into the array's own
/// constness as well as leaving it on the element (`ir/ty.rs`'s
/// `from_clang_ty`), so `const int a[2][3]` arrives as markers and array layers
/// alternating all the way down, and stopping at the first of either finds
/// nothing useful. A `std::array` of `std::array`s alternates the same way, its
/// marker sitting on each layer.
pub(crate) fn unqualified_array_element_type(ty: &Type) -> &Type {
    let mut ty = strip_layout_markers(ty);
    while let Type::Array(arr) = ty {
        ty = strip_layout_markers(&arr.elem);
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

/// Whether `ty` has an array anywhere `type_to_cpp` would walk to, including
/// as a template argument.
///
/// Written for the one position [`denotes_cpp_array`] does not reach: a
/// concrete template instantiation, whose C++ name is built by writing the
/// arguments out again. `type_to_cpp` writes an array as `std::array<T, N>`,
/// which is what one is everywhere a signature can hold it, but a template
/// argument may be a real C array, and the two cannot be told apart here.
///
/// A bare function type is not walked, because `type_to_cpp` turns one down
/// outright, so nothing an array could hide inside a function pointer reaches
/// a C++ name.
pub(crate) fn mentions_cpp_array(ty: &Type) -> bool {
    match ty {
        Type::Array(_) => true,
        Type::Path(typ) => typ.path.segments.iter().any(|seg| {
            let PathArguments::AngleBracketed(args) = &seg.arguments else {
                return false;
            };
            args.args.iter().any(|arg| match arg {
                GenericArgument::Type(inner) => mentions_cpp_array(inner),
                _ => false,
            })
        }),
        Type::Ptr(ptr) => mentions_cpp_array(&ptr.elem),
        Type::Reference(r) => mentions_cpp_array(&r.elem),
        Type::Paren(inner) => mentions_cpp_array(&inner.elem),
        Type::Group(inner) => mentions_cpp_array(&inner.elem),
        _ => false,
    }
}

/// The element of an array a signature carries, looking through the nesting
/// where an array holds arrays, or `None` where `ty` holds no array.
///
/// Every array a signature is allowed to carry is a `std::array`, whether it
/// stands on its own or is reached through the reference the two other arms
/// here are: a C array standing alone cannot be written in either position, and
/// one behind a reference is refused while the marker saying which it is can
/// still be read. cxx has the same rule about the element in each case, moving
/// the array whole when it is passed and naming the same C++ type when it is
/// not.
pub(crate) fn cpp_array_element(ty: &Type) -> Option<&Type> {
    match ty {
        Type::Array(arr) => Some(cpp_array_element(&arr.elem).unwrap_or(&arr.elem)),
        Type::Reference(TypeReference { elem, .. }) => cpp_array_element(elem),
        Type::Path(typ) => {
            extract_pinned_mutable_reference_type(typ).and_then(|inner| cpp_array_element(inner))
        }
        _ => None,
    }
}

/// Whether `ty` reaches a C++ array through a pointer.
///
/// The part of [`denotes_cpp_array`] a signature still has to turn down, which
/// is neither of the two a signature keeps.
///
/// The array standing alone is not an array at all: no C++ function takes or
/// returns one by value - a parameter decays and a return is ill-formed - so a
/// signature holding one holds a class bindgen lowered to the array it is laid
/// out as, which is `std::array<T, N>`. cxx spells that back the same way, so
/// the bridge names the type the function was declared with.
///
/// A reference is decided earlier, by
/// [`TypeConverter::check_array_referent`](crate::conversion::analysis::type_converter): both C++ types
/// exist there, and telling them apart needs the marker, which is gone by the
/// time a converted signature is being read.
///
/// A pointer is left. `ensure_pointee_is_valid` turns down one written as
/// `T (*)[N]`, but an alias hides the array behind a path until it is
/// resolved, as in `using A = T[N]; void f(A*)`. No `std::array` reaches here
/// that way, pointers to one not being bound at all.
pub(crate) fn denotes_cpp_array_behind_pointer(ty: &Type) -> bool {
    match ty {
        Type::Ptr(TypePtr { elem, .. }) => denotes_cpp_array(elem),
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
