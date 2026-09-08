#![doc = include_str!("../README.md")]
#![cfg_attr(nightly, feature(unsize))]
#![cfg_attr(nightly, feature(dispatch_from_dyn))]
#![cfg_attr(nightly, feature(arbitrary_self_types))]

// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// The crazy macro_rules magic in this file is thanks to dtolnay@
// and is a way of attaching rustdoc to each of the possible directives
// within the include_cpp outer macro. None of the directives actually
// do anything - all the magic is handled entirely by
// autocxx_macro::include_cpp_impl.

mod fallible;
mod reference_wrapper;
mod rvalue_param;
pub mod subclass;
mod value_param;

pub use fallible::{StackSlot, TryWithinBox, TryWithinUniquePtr};
pub use reference_wrapper::{
    AsCppMutRef, AsCppRef, CppLtRef, CppMutLtRef, CppMutRef, CppPin, CppRef, CppUniquePtrPin,
};

/// Include some C++ headers in your Rust project.
///
/// This macro allows you to include one or more C++ headers within
/// your Rust code, and call their functions fairly naturally.
///
/// # Examples
///
/// C++ header (`input.h`):
/// ```cpp
/// #include <cstdint>
///
/// uint32_t do_math(uint32_t a);
/// ```
///
/// Rust code:
/// ```
/// # use autocxx_macro::include_cpp_impl as include_cpp;
/// include_cpp!(
/// #   parse_only!()
///     #include "input.h"
///     generate!("do_math")
///     safety!(unsafe)
/// );
///
/// # mod ffi { pub fn do_math(a: u32) -> u32 { a+3 } }
/// # fn main() {
/// ffi::do_math(3);
/// # }
/// ```
///
/// The resulting bindings will use idiomatic Rust wrappers for types from the [cxx]
/// crate, for example [`cxx::UniquePtr`] or [`cxx::CxxString`]. Due to the care and thought
/// that's gone into the [cxx] crate, such bindings are pleasant and idiomatic to use
/// from Rust, and usually don't require the `unsafe` keyword.
///
/// For full documentation, see [the manual](https://google.github.io/autocxx/).
///
/// # The [`include_cpp`] macro
///
/// Within the braces of the `include_cpp!{...}` macro, you should provide
/// a list of at least the following:
///
/// * `#include "cpp_header.h"`: a header filename to parse and include
/// * `generate!("type_or_function_name")`: a type or function name whose declaration
///   should be made available to C++. (See the section on Allowlisting, below).
/// * Optionally, `safety!(unsafe)` - see discussion of [`safety`].
///
/// Other directives are possible as documented in this crate.
///
/// Now, try to build your Rust project. `autocxx` may fail to generate bindings
/// for some of the items you specified with [generate] directives: remove
/// those directives for now, then see the next section for advice.
///
/// # Allowlisting
///
/// How do you inform autocxx which bindings to generate? There are three
/// strategies:
///
/// * *Recommended*: provide various [`generate`] directives in the
///   [`include_cpp`] macro. This can specify functions or types.
/// * *Not recommended*: in your `build.rs`, call `Builder::auto_allowlist`.
///   This will attempt to spot _uses_ of FFI bindings anywhere in your Rust code
///   and build the allowlist that way. This is experimental and has known limitations.
/// * *Strongly not recommended*: use [`generate_all`]. This will attempt to
///   generate Rust bindings for _any_ C++ type or function discovered in the
///   header files. This is generally a disaster if you're including any
///   remotely complex header file: we'll try to generate bindings for all sorts
///   of STL types. This will be slow, and some may well cause problems.
///   Effectively this is just a debug option to discover such problems. Don't
///   use it!
///
/// # Internals
///
/// For documentation on how this all actually _works_, see
/// `IncludeCppEngine` within the `autocxx_engine` crate.
#[macro_export]
macro_rules! include_cpp {
    (
        $(#$include:ident $lit:literal)*
        $($mac:ident!($($arg:tt)*))*
    ) => {
        $($crate::$include!{__docs})*
        $($crate::$mac!{__docs})*
        $crate::include_cpp_impl! {
            $(#include $lit)*
            $($mac!($($arg)*))*
        }
    };
}

/// Include a C++ header. A directive to be included inside
/// [include_cpp] - see [include_cpp] for details
#[macro_export]
macro_rules! include {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Generate Rust bindings for the given C++ type or function.
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
/// See also [generate_pod].
#[macro_export]
macro_rules! generate {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Generate as "plain old data" and add to allowlist.
/// Generate Rust bindings for the given C++ type such that
/// it can be passed and owned by value in Rust. This only works
/// for C++ types which have trivial move constructors and no
/// destructor - you'll encounter a compile error otherwise.
/// If your type doesn't match that description, use [generate]
/// instead, and own the type using [UniquePtr][cxx::UniquePtr].
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! generate_pod {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Generate Rust bindings for all C++ types and functions
/// in a given namespace.
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
/// See also [generate].
#[macro_export]
macro_rules! generate_ns {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Generate Rust bindings for all C++ types and functions
/// found. Highly experimental and not recommended.
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
/// See also [generate].
#[macro_export]
macro_rules! generate_all {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Generate as "plain old data". For use with [generate_all]
/// and similarly experimental.
#[macro_export]
macro_rules! pod {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Skip the normal generation of a `make_string` function
/// and other utilities which we might generate normally.
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! exclude_utilities {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Pretty-print the generated Rust instead of emitting it as one
/// enormous line of tokens. This costs a little build time and changes
/// nothing about what the bindings do; it is for when you want to read
/// the file `AUTOCXX_RS_FILE` names, or diff it between two runs.
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! pretty {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Entirely block some type from appearing in the generated
/// code. This can be useful if there is a type which is not
/// understood by bindgen or autocxx, and incorrect code is
/// otherwise generated.
/// This is 'greedy' in the sense that any functions/methods
/// which take or return such a type will _also_ be blocked.
/// See also [`opaque`].
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! block {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Instruct `bindgen` to generate a type as an opaque type -
/// that is, without fields inside it. This should be used
/// only when there's a need to workaround some `bindgen`
/// issue where it's incorrectly generating the type.
/// See the [bindgen documentation](https://rust-lang.github.io/rust-bindgen/opaque.html)
/// for what exactly this means.
///
/// At first glance, this might seem to have no effect for
/// types which are marked non-POD. However, it prevents
/// autocxx from inferring whether the type has implicit
/// constructors, and thus limits the options for even
/// allocating the type. This should therefore only be used
/// when trying to work around a bug.
///
/// The types which are generated when you use this are
/// rather useless - it's hard to interact with them at all
/// from within the generated Rust. Worse still, if they're
/// included as members of any other type, those types are
/// "infected" and also become useless (in the sense that
/// we can't figure out what constructors those types might
/// have). Use with caution and only when really needed.
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! opaque {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Avoid generating implicit constructors for this type.
/// The rules for when to generate C++ implicit constructors
/// are complex, and if autocxx gets it wrong, you can block
/// such constructors using this.
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! block_constructors {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// The name of the mod to be generated with the FFI code.
/// The default is `ffi`.
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! name {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// A concrete type to make, for example
/// `concrete!("Container<Contents>")`.
/// All types must already be on the allowlist by having used
/// `generate!` or similar.
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! concrete {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Specifies a global safety policy for functions generated
/// from these headers. By default (without such a `safety!`
/// directive) all such functions are marked as `unsafe` and
/// therefore can only be called within an `unsafe {}` block
/// or some `unsafe` function which you create.
///
/// Alternatively, by specifying a `safety!` block you can
/// declare that most generated functions are in fact safe.
/// Specifically, you'd specify:
/// `safety!(unsafe)`
/// or
/// `safety!(unsafe_ffi)`
/// These two options are functionally identical. If you're
/// unsure, simply use `unsafe`. The reason for the
/// latter option is if you have code review policies which
/// might want to give a different level of scrutiny to
/// C++ interop as opposed to other types of unsafe Rust code.
/// Maybe in your organization, C++ interop is less scary than
/// a low-level Rust data structure using pointer manipulation.
/// Or maybe it's more scary. Either way, using `unsafe` for
/// the data structure and using `unsafe_ffi` for the C++
/// interop allows you to apply different linting tools and
/// policies to the different options.
///
/// Irrespective, C++ code is of course unsafe. It's worth
/// noting that use of C++ can cause unexpected unsafety at
/// a distance in faraway Rust code. As with any use of the
/// `unsafe` keyword in Rust, *you the human* are declaring
/// that you've analyzed all possible ways that the code
/// can be used and you are guaranteeing to the compiler that
/// no badness can occur. Good luck.
///
/// Generated C++ APIs which use raw pointers remain `unsafe`
/// no matter what policy you choose.
///
/// There's an additional possible experimental safety
/// policy available here:
/// `safety!(unsafe_references_wrapped)`
/// This policy treats C++ references as scary and requires
/// them to be wrapped. A `const T&` becomes a [`CppRef`]
/// and a `T&` a [`CppMutRef`], both as a parameter and as
/// the object a method is called on; a returned reference
/// becomes the lifetime-carrying [`CppLtRef`] or
/// [`CppMutLtRef`]. No C++ reference reaches you as a Rust
/// reference, so none of them can alias one. To hand
/// something of your own to such a function, put it in a
/// [`CppPin`] (or a [`CppUniquePtrPin`], for a
/// [`cxx::UniquePtr`]) and ask that for the reference.
/// This only works on nightly Rust because it
/// depends upon an unstable feature
/// (`arbitrary_self_types`). However, it should
/// eliminate all undefined behavior related to Rust's
/// stricter aliasing rules than C++.
#[macro_export]
macro_rules! safety {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Whether to avoid generating [`cxx::UniquePtr`] and [`cxx::Vector`]
/// implementations. This is primarily useful for reducing test cases and
/// shouldn't be used in normal operation.
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! exclude_impls {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Indicates that a C++ type is not to be generated by autocxx in this case,
/// but instead should refer to some pre-existing Rust type.
///
/// If you wish for the type to be POD, you can use a `pod!` directive too
/// (but see the "requirements" section below).
///
/// The syntax is:
/// `extern_cpp_type!("CppNameGoesHere", path::to::rust::type)`
///
/// Generally speaking, this should be used only to refer to types
/// generated elsewhere by `autocxx` or `cxx` to ensure that they meet
/// all the right requirements. It's possible - but fragile - to
/// define such types yourself.
///
/// # Requirements for externally defined Rust types
///
/// It's generally expected that you would make such a type
/// in Rust using a separate `include_cpp!` macro, or
/// a manual `#[cxx::bridge]` directive somehwere. That is, this
/// directive is intended mainly for use in cross-linking different
/// sets of bindings in different mods, rather than truly to point to novel
/// external types.
///
/// But with that in mind, here are the requirements you must stick to.
///
/// For non-POD external types:
/// * The size and alignment of this type *must* be correct.
///
/// For POD external types:
/// * As above
/// * Your type must correspond to the requirements of
///   [`cxx::kind::Trivial`]. In general, that means, no move constructor
///   and no destructor. If you generate this type using `cxx` itself
///   (or `autocxx`) this will be enforced using `static_assert`s
///   within the generated C++ code. Without using those tools, you're
///   on your own for determining this... and it's hard because the presence
///   of particular fields or base classes may well result in your type
///   violating those rules.
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! extern_cpp_type {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Indicates that a C++ type is not to be generated by autocxx in this case,
/// but instead should refer to some pre-existing Rust type. Unlike
/// `extern_cpp_type!`, there's no need for the size and alignment of this
/// type to be correct.
///
/// The syntax is:
/// `extern_cpp_opaque_type!("CppNameGoesHere", path::to::rust::type)`
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! extern_cpp_opaque_type {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Deprecated - use [`extern_rust_type`] instead.
#[macro_export]
#[deprecated]
macro_rules! rust_type {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// See [`extern_rust::extern_rust_type`].
#[macro_export]
macro_rules! extern_rust_type {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// See [`subclass::subclass`].
#[macro_export]
macro_rules! subclass {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Indicates that a C++ type can definitely be instantiated. This has effect
/// in two cases, both of them types autocxx cannot inspect for itself:
///
/// First:
/// * the type is a typedef to something else
/// * the 'something else' can't be fully inspected by autocxx, possibly
///   becaue it relies on dependent qualified types or some other template
///   arrangement that bindgen cannot fully understand.
///
/// In such circumstances, autocxx normally has to err on the side of caution
/// and assume that some type within the 'something else' is itself a forward
/// declaration. That means, the opaque typedef won't be storable within
/// a [`cxx::UniquePtr`]. If you know that no forward declarations are involved,
/// you can declare the typedef type is instantiable and then you'll be able to
/// own it within Rust.
///
/// Second, the type is a concrete instantiation of a C++ class template -
/// named either by a typedef, `typedef A<uint32_t> C;`, or by a
/// [`concrete!`] directive. `bindgen` reports the template, never the
/// specialization, so autocxx is told nothing about such a type: not its
/// members, not its bases, not one constructor it declares. Declaring it
/// instantiable is you saying that C++ gives it a default constructor, and
/// autocxx then generates a `new()` for it. Your C++ compiler is the arbiter:
/// if the class hasn't really got one - because it declares a constructor of
/// its own, or because a `const` template argument deletes it - the generated
/// C++ fails to compile rather than misbehaving. Without the directive such a
/// type keeps its previous shape, usable by reference and through the
/// functions which return one.
///
/// This `new()` hands back a [`cxx::UniquePtr`] directly rather than something
/// to finish with `.within_unique_ptr()`. autocxx does not know how big such a
/// type is - it is declared to `cxx` as a plain opaque type - so only C++ can
/// allocate one, and there is no choice of storage to offer. That is the same
/// rule autocxx applies to a function which *returns* one of these types, and
/// to a subclass's C++ peer, which is opaque to cxx for its own reasons.
///
/// Copy and move constructors are not generated for the same reason: both
/// build the new object into storage the caller provides, which for one of
/// these types Rust cannot provide. A destructor is, so that C++ destroys the
/// object however Rust lets go of it.
///
/// One thing does not follow the alias: `throws!` on such a constructor has
/// to name the instantiation the way autocxx names it, because a designation
/// is matched against a function's own C++ name and nothing resolves a typedef
/// on the way. A `concrete!` type answers to the identifier that directive
/// gives it - `throws!("AConc")` - but an instantiation reached through a
/// typedef answers only to autocxx's generated name. An undesignated
/// constructor which does throw terminates the process, as any undesignated
/// function does.
///
/// The syntax is:
/// `instantiable!("CppNameGoesHere")`
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! instantiable {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Indicates that a C++ function may throw exceptions. When a function
/// is marked with this directive, its Rust binding will return
/// `Result<T, cxx::Exception>` instead of `T`, allowing the caller
/// to handle C++ exceptions that propagate across the FFI boundary.
///
/// The syntax is:
/// `throws!("function_name")`
///
/// Qualified names are supported for namespaced functions and methods:
/// * `throws!("do_something")` - matches any function named `do_something`
/// * `throws!("MyClass::do_something")` - matches method `do_something` on `MyClass`
/// * `throws!("my_namespace::do_something")` - matches `do_something` in `my_namespace`
///
/// # Constructors
///
/// A constructor is named the way C++ names it - `throws!("MyClass::MyClass")` -
/// and marking one marks every overload, since they all share that name.
///
/// A constructor which can throw cannot hand back a
/// [`moveit::new::New`], whose contract is to leave the place it was given
/// initialized. It hands back a [`moveit::new::TryNew`] instead, which is
/// finished with [`TryWithinUniquePtr::try_within_unique_ptr`],
/// [`TryWithinBox::try_within_box`], [`TryWithinBox::try_within_cpp_pin`], or
/// a [`stack_slot!`] and [`StackSlot::try_emplace`] - the fallible spellings
/// of the ways an ordinary constructor is finished. The same is true of a
/// `throws!` function which returns a non-POD type by value, which C++ builds
/// into a caller-provided place in just the same way.
///
/// The exceptions are the constructors which never offered a choice of place
/// to begin with - a subclass's C++ peer and a concrete template instantiation,
/// both of which allocate in C++ - and whose fallible form therefore hands back
/// a `Result<cxx::UniquePtr<Self>, cxx::Exception>` with nothing left to finish.
///
/// See the book's exceptions chapter for the whole picture, including what to
/// do about a subclass whose superclass constructor throws.
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! throws {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Chooses how a C++ `enum` is rendered in Rust.
///
/// By default every C++ enum becomes a native Rust `enum`, which is ideal
/// for enumerations whose values are a closed set. It is a poor fit for flag
/// enums. Combining two enumerators in C++ yields an `int` - the operands are
/// promoted, unless the enum overloads `operator|` - and converting that back
/// to the enum type is ordinary practice: a C++ enum object may hold values no
/// enumerator names. For an enum with a fixed underlying type, such as
/// `enum Flags : int`, that is every value the type can represent; for one
/// without, every value in the bit range its enumerators span. A Rust `enum`
/// may not, since a value which is none of its variants is instant undefined
/// behaviour. Use this directive to pick a representation that suits each
/// enum.
///
/// The syntax is:
/// `enum_style!(StyleName, "FirstEnum", "SecondEnum")`
///
/// The styles are:
/// * `BitfieldEnum` - an integer newtype whose variants are associated
///   constants, plus the bitwise operators `&`, `|`, `^` and `!`. This is
///   what you want for flags.
/// * `NewtypeEnum` - the same newtype without the operators.
///
/// The integer either newtype wraps is the enum's underlying type as the C++
/// compiler sees it, so `flags.0` is a `c_int` for `enum Flags : int` and a
/// `c_uint` for `enum Flags : unsigned`. An unscoped enum with no fixed
/// underlying type has no portable answer: the compiler picks, and it may
/// pick differently from one target or set of flags to the next. On the
/// targets autocxx tests, MSVC gives it `int`, while gcc and clang give one
/// whose enumerators are all non-negative `unsigned int`. Name the underlying
/// type in C++ if you intend to write `.0`'s type down.
/// * `RustifiedEnum` - a native Rust `enum`. This is the default, so naming
///   it is only useful for emphasis.
/// * `RustifiedNonExhaustiveEnum` - a native Rust `enum` marked
///   `#[non_exhaustive]`, so that your `match`es must have a catch-all arm
///   and adding a variant on the C++ side isn't a breaking change.
///
/// Name each enum exactly as you would in [`generate`] - so a namespaced enum
/// is `"ns::Thing"`, and an enum nested inside a class is `"Outer_Inner"`,
/// because that is the name `bindgen` gives it. Repeat the directive to give
/// different styles to different enums. Asking for two different styles for
/// the same enum is an error, as is anything that isn't a plain name.
///
/// # `BitfieldEnum` and `NewtypeEnum` need [`generate_pod`]
///
/// These two styles reach Rust as a `struct`, not an `enum`, and their
/// variants are associated constants on it. autocxx only re-exports the
/// generated type - constants and all - for types it holds by value, so
/// those two styles must be requested with [`generate_pod`]. A plain
/// [`generate`] gives you an opaque type with no constants on it, which is
/// unlikely to be what you wanted. The two rustified styles work with
/// either.
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! enum_style {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

/// Puts extra `#[derive(..)]` traits on the Rust type generated for a C++ one.
///
/// The syntax is:
/// `derive!("Type", "Debug", "PartialEq")`
///
/// Name the type exactly as you would in [`generate`] - so a namespaced type
/// is `"ns::Thing"`, and a type nested inside a class is `"Outer_Inner"`,
/// because that is the name `bindgen` gives it. Repeat the directive to add
/// more traits, or to name more types; asking twice for the same trait on the
/// same type is an error.
///
/// Each trait is written as you would inside `#[derive(..)]`, so a path works
/// too: `derive!("Thing", "num_enum::TryFromPrimitive")`. It is your job to
/// make sure the derive macro is in scope where the bindings are generated,
/// and that the type can satisfy it: `Clone` needs every field to be `Clone`,
/// `PartialEq` needs every field to be `PartialEq`, and so on. autocxx does
/// not check, so an impossible request comes back from `rustc` rather than
/// from autocxx.
///
/// # Only for types held by value
///
/// The trait goes onto the type autocxx re-exports, which means the directive
/// only works for a [`generate_pod`] type or an enum. Everything else is an
/// opaque type - autocxx emits a wrapper with no fields, precisely because
/// Rust must not look inside it - so there would be nothing for a derive to
/// work from, and asking is an error rather than a no-op.
///
/// `Default` on an enum is refused too. Nothing makes one enumerator of a C++
/// enum the default, and a derived `Default` on an enum with no variant marked
/// `#[default]` does not compile.
///
/// A directive to be included inside
/// [include_cpp] - see [include_cpp] for general information.
#[macro_export]
macro_rules! derive {
    ($($tt:tt)*) => { $crate::usage!{$($tt)*} };
}

#[doc(hidden)]
#[macro_export]
macro_rules! usage {
    (__docs) => {};
    ($($tt:tt)*) => {
        compile_error! {r#"usage:  include_cpp! {
                   #include "path/to/header.h"
                   generate!(...)
                   generate_pod!(...)
               }
"#}
    };
}

use std::pin::Pin;

#[doc(hidden)]
pub use autocxx_macro::include_cpp_impl;

#[doc(hidden)]
pub use autocxx_macro::cpp_semantics;

/// A transparent newtype over `$p` which cxx sees as the named C++ type `$c`.
macro_rules! ctype_newtype {
    ($r:ident, $c:expr, $p:ty, $d:expr) => {
        #[doc=$d]
        #[derive(Debug, Eq, Copy, Clone, PartialEq, Hash)]
        #[allow(non_camel_case_types)]
        #[repr(transparent)]
        pub struct $r(pub $p);

        /// # Safety
        ///
        /// We assert that the namespace and type ID refer to a C++
        /// type which is equivalent to this Rust type.
        unsafe impl cxx::ExternType for $r {
            type Id = cxx::type_id!($c);
            type Kind = cxx::kind::Trivial;
        }

        impl From<$p> for $r {
            fn from(val: $p) -> Self {
                Self(val)
            }
        }

        impl From<$r> for $p {
            fn from(val: $r) -> Self {
                val.0
            }
        }
    };
}

/// One of the variable-length C integers, whose Rust width is whatever
/// `std::os::raw` says it is on this target.
macro_rules! ctype_wrapper {
    ($r:ident, $c:expr, $d:expr) => {
        ctype_newtype!($r, $c, ::std::os::raw::$r, $d);
    };
}

ctype_wrapper!(
    c_ulonglong,
    "c_ulonglong",
    "Newtype wrapper for an unsigned long long"
);
ctype_wrapper!(c_longlong, "c_longlong", "Newtype wrapper for a long long");
ctype_wrapper!(c_ulong, "c_ulong", "Newtype wrapper for an unsigned long");
ctype_wrapper!(c_long, "c_long", "Newtype wrapper for a long");
ctype_wrapper!(
    c_ushort,
    "c_ushort",
    "Newtype wrapper for an unsigned short"
);
ctype_wrapper!(c_short, "c_short", "Newtype wrapper for an short");
ctype_wrapper!(c_uint, "c_uint", "Newtype wrapper for an unsigned int");
ctype_wrapper!(c_int, "c_int", "Newtype wrapper for an int");
ctype_wrapper!(c_uchar, "c_uchar", "Newtype wrapper for an unsigned char");

// The fixed-width C integers, under names cxx has no atom for.
//
// cxx spells `uint32_t` as its `u32` atom and turns down a `unique_ptr` of any
// of its atoms - `check_type_unique_ptr` - so `std::unique_ptr<uint32_t>` has
// no cxx spelling at all. It is the same C++ type as some `unsigned` something
// which does have a wrapper above, so autocxx names a `unique_ptr` payload
// with the wrapper of the width C++ actually wrote, and cxx sees a named type
// it will emit shims for on request. See google/autocxx#422.
//
// These are not substituted anywhere else: a `uint32_t` by value, in a
// `std::vector` or in a `std::shared_ptr` is a plain `u32`, which is what cxx
// wants there and what callers have always seen.
ctype_newtype!(c_u8, "c_u8", u8, "Newtype wrapper for a uint8_t");
ctype_newtype!(c_i8, "c_i8", i8, "Newtype wrapper for an int8_t");
ctype_newtype!(c_u16, "c_u16", u16, "Newtype wrapper for a uint16_t");
ctype_newtype!(c_i16, "c_i16", i16, "Newtype wrapper for an int16_t");
ctype_newtype!(c_u32, "c_u32", u32, "Newtype wrapper for a uint32_t");
ctype_newtype!(c_i32, "c_i32", i32, "Newtype wrapper for an int32_t");
ctype_newtype!(c_u64, "c_u64", u64, "Newtype wrapper for a uint64_t");
ctype_newtype!(c_i64, "c_i64", i64, "Newtype wrapper for an int64_t");

// `__int128`, which cxx has no atom for at all - so unlike the eight above,
// this wrapper is how the type crosses in every position, not only inside a
// `unique_ptr`. There is deliberately no `c_u128`: bindgen renders `unsigned
// __int128` and `__float128` as one `u128` token, so a wrapper for it would be
// the right type for one of the two and a miscompile for the other. (A 16-byte
// `long double` used to share that token; it is refused by name of its own
// now.) `autocxx::c_type_vectors` has no entry for this
// one either, because MSVC has no `__int128` and that file compiles
// everywhere; the engine refuses a container of it and says so.
ctype_newtype!(c_i128, "c_i128", i128, "Newtype wrapper for a C++ __int128");

/// Newtype wrapper for a C void. Only useful as a `*c_void`
#[allow(non_camel_case_types)]
#[repr(transparent)]
pub struct c_void(pub ::std::os::raw::c_void);

/// # Safety
///
/// We assert that the namespace and type ID refer to a C++
/// type which is equivalent to this Rust type.
unsafe impl cxx::ExternType for c_void {
    type Id = cxx::type_id!(c_void);
    type Kind = cxx::kind::Trivial;
}

/// The `std::vector` and smart pointer glue for the `c_*` newtypes above.
///
/// Lives in its own file because `cxx-build` reads the source to find the
/// bridge, and honours any `#[cfg]` it finds there - so the feature gate has
/// to be here, on the `mod`, rather than on the bridge itself.
#[cfg(feature = "c-type-vectors")]
mod c_type_vectors;

/// A C++ `char16_t`. Like the other C type wrappers here, this is a
/// transparent newtype over the Rust integer of the same width, so a value
/// crosses between the two with `.0` or [`From`].
#[derive(Debug, Eq, Copy, Clone, PartialEq, Hash)]
#[allow(non_camel_case_types)]
#[repr(transparent)]
pub struct c_char16_t(pub u16);

/// # Safety
///
/// We assert that the namespace and type ID refer to a C++
/// type which is equivalent to this Rust type.
unsafe impl cxx::ExternType for c_char16_t {
    type Id = cxx::type_id!(c_char16_t);
    type Kind = cxx::kind::Trivial;
}

impl From<u16> for c_char16_t {
    fn from(val: u16) -> Self {
        Self(val)
    }
}

impl From<c_char16_t> for u16 {
    fn from(val: c_char16_t) -> Self {
        val.0
    }
}

/// The Rust integer which a C++ `wchar_t` is, on this target.
///
/// Unlike `char16_t`, `wchar_t` has no fixed representation: its width and its
/// signedness are both the target's to choose, so this is a `cfg` and not a
/// constant. Getting the width wrong would make [`c_wchar_t`] a different size
/// from the C++ type it stands in for and every value would be read from the
/// wrong bytes, with nothing in the generated code to notice - so the four arms
/// below were checked against `clang -dM -E`'s `__WCHAR_TYPE__` for every
/// target `rustc --print target-list` names, and agree with it on all of them.
/// They are mutually exclusive and exhaustive by construction.
///
/// - 16-bit unsigned: everything using the Microsoft ABI, plus Cygwin and
///   UEFI, which are `unsigned short` for the same reason.
/// - 16-bit signed: AVR and MSP430, where `int` is itself 16 bits.
/// - 32-bit unsigned: AAPCS (`arm`, `aarch64`) - except Darwin, NetBSD and
///   OpenBSD, each of which overrides it - and AIX.
/// - 32-bit signed: everywhere else, which is most places.
///
/// The `libc` crate keeps the same table and differs on three tier-3 targets
/// (`csky`, `hexagon`, and `riscv64` on Android, where it says unsigned and
/// clang says `int`); clang is the authority here, because clang is what
/// compiles the C++ this has to match.
///
/// The integration test `test_wchar_t_values` asks the C++ compiler for
/// `sizeof(wchar_t)` and its signedness and asserts the choice here matches, so
/// every platform the test suite runs on checks its own row: CI's Linux
/// x86-64, macOS Arm and both Windows targets.
#[cfg(any(windows, target_os = "cygwin", target_os = "uefi"))]
#[allow(non_camel_case_types)]
pub type wchar_t = u16;

/// The Rust integer which a C++ `wchar_t` is, on this target. See the first
/// definition of this alias for the rationale.
#[cfg(all(
    not(any(windows, target_os = "cygwin", target_os = "uefi")),
    any(target_arch = "avr", target_arch = "msp430")
))]
#[allow(non_camel_case_types)]
pub type wchar_t = i16;

/// The Rust integer which a C++ `wchar_t` is, on this target. See the first
/// definition of this alias for the rationale.
#[cfg(all(
    not(any(windows, target_os = "cygwin", target_os = "uefi")),
    not(any(target_arch = "avr", target_arch = "msp430")),
    any(
        target_os = "aix",
        all(
            any(target_arch = "arm", target_arch = "aarch64"),
            not(target_vendor = "apple"),
            not(any(target_os = "netbsd", target_os = "openbsd"))
        )
    )
))]
#[allow(non_camel_case_types)]
pub type wchar_t = u32;

/// The Rust integer which a C++ `wchar_t` is, on this target. See the first
/// definition of this alias for the rationale. This arm is the complement of
/// the three above.
#[cfg(all(
    not(any(windows, target_os = "cygwin", target_os = "uefi")),
    not(any(target_arch = "avr", target_arch = "msp430")),
    not(any(
        target_os = "aix",
        all(
            any(target_arch = "arm", target_arch = "aarch64"),
            not(target_vendor = "apple"),
            not(any(target_os = "netbsd", target_os = "openbsd"))
        )
    ))
))]
#[allow(non_camel_case_types)]
pub type wchar_t = i32;

/// A C++ `wchar_t`. Like the other C type wrappers here, this is a
/// transparent newtype over the Rust integer of the same width, so a value
/// crosses between the two with `.0` or [`From`]. That integer is
/// [`wchar_t`], which is chosen per target.
#[derive(Debug, Eq, Copy, Clone, PartialEq, Hash)]
#[allow(non_camel_case_types)]
#[repr(transparent)]
pub struct c_wchar_t(pub wchar_t);

/// # Safety
///
/// We assert that the namespace and type ID refer to a C++
/// type which is equivalent to this Rust type.
unsafe impl cxx::ExternType for c_wchar_t {
    type Id = cxx::type_id!(c_wchar_t);
    type Kind = cxx::kind::Trivial;
}

impl From<wchar_t> for c_wchar_t {
    fn from(val: wchar_t) -> Self {
        Self(val)
    }
}

impl From<c_wchar_t> for wchar_t {
    fn from(val: c_wchar_t) -> Self {
        val.0
    }
}

/// A C++ `char32_t`. Like the other C type wrappers here, this is a
/// transparent newtype over the Rust integer of the same width, so a value
/// crosses between the two with `.0` or [`From`].
#[derive(Debug, Eq, Copy, Clone, PartialEq, Hash)]
#[allow(non_camel_case_types)]
#[repr(transparent)]
pub struct c_char32_t(pub u32);

/// # Safety
///
/// We assert that the namespace and type ID refer to a C++
/// type which is equivalent to this Rust type.
unsafe impl cxx::ExternType for c_char32_t {
    type Id = cxx::type_id!(c_char32_t);
    type Kind = cxx::kind::Trivial;
}

impl From<u32> for c_char32_t {
    fn from(val: u32) -> Self {
        Self(val)
    }
}

impl From<c_char32_t> for u32 {
    fn from(val: c_char32_t) -> Self {
        val.0
    }
}

/// A C++20 `char8_t`. Like the other C type wrappers here, this is a
/// transparent newtype over the Rust integer of the same width, so a value
/// crosses between the two with `.0` or [`From`].
#[derive(Debug, Eq, Copy, Clone, PartialEq, Hash)]
#[allow(non_camel_case_types)]
#[repr(transparent)]
pub struct c_char8_t(pub u8);

/// # Safety
///
/// We assert that the namespace and type ID refer to a C++
/// type which is equivalent to this Rust type.
unsafe impl cxx::ExternType for c_char8_t {
    type Id = cxx::type_id!(c_char8_t);
    type Kind = cxx::kind::Trivial;
}

impl From<u8> for c_char8_t {
    fn from(val: u8) -> Self {
        Self(val)
    }
}

impl From<c_char8_t> for u8 {
    fn from(val: c_char8_t) -> Self {
        val.0
    }
}

/// autocxx couldn't generate these bindings.
/// If you come across a method, type or function which refers to this type,
/// it indicates that autocxx couldn't generate that binding. A documentation
/// comment should be attached indicating the reason.
#[allow(dead_code)]
pub struct BindingGenerationFailure {
    _unallocatable: [*const u8; 0],
    _pinned: core::marker::PhantomData<core::marker::PhantomPinned>,
}

/// Tools to export Rust code to C++.
// These are in a mod to avoid shadowing the definitions of the
// directives above, which, being macro_rules, are unavoidably
// in the crate root but must be function-style macros to keep
// the include_cpp impl happy.
pub mod extern_rust {

    /// Declare that this is a Rust type which is to be exported to C++.
    /// You can use this in two ways:
    /// * as an attribute macro on a Rust type, for instance:
    ///   ```
    ///   # use autocxx_macro::extern_rust_type as extern_rust_type;
    ///   #[extern_rust_type]
    ///   struct Bar;
    ///   ```
    /// * as a directive within the [include_cpp] macro, in which case
    ///   provide the type path in brackets:
    ///   ```
    ///   # use autocxx_macro::include_cpp_impl as include_cpp;
    ///   include_cpp!(
    ///   #   parse_only!()
    ///       #include "input.h"
    ///       extern_rust_type!(Bar)
    ///       safety!(unsafe)
    ///   );
    ///   struct Bar;
    ///   ```
    /// These may be used within references in the signatures of C++ functions,
    /// for instance. This will contribute to an `extern "Rust"` section of the
    /// generated `cxx` bindings, and this type will appear in the C++ header
    /// generated for use in C++.
    ///
    /// # Finding these bindings from C++
    ///
    /// You will likely need to forward-declare this type within your C++ headers
    /// before you can use it in such function signatures. autocxx can't generate
    /// headers (with this type definition) until it's parsed your header files;
    /// logically therefore if your header files mention one of these types
    /// it's impossible for them to see the definition of the type.
    ///
    /// If you're using multiple sets of `include_cpp!` directives, or
    /// a mixture of `include_cpp!` and `#[cxx::bridge]` bindings, then you
    /// may be able to `#include "cxxgen.h"` to refer to the generated C++
    /// function prototypes. In this particular circumstance, you'll want to know
    /// how exactly the `cxxgen.h` header is named, because one will be
    /// generated for each of the sets of bindings encountered. The pattern
    /// can be set manually using `autocxxgen`'s command-line options. If you're
    /// using `autocxx`'s `build.rs` support, those headers will be named
    /// `cxxgen.h`, `cxxgen1.h`, `cxxgen2.h` according to the order in which
    /// the `include_cpp` or `cxx::bridge` bindings are encountered.
    pub use autocxx_macro::extern_rust_type;

    /// Declare that a given function is a Rust function which is to be exported
    /// to C++. This is used as an attribute macro on a Rust function, for instance:
    /// ```
    /// # use autocxx_macro::extern_rust_function as extern_rust_function;
    /// #[extern_rust_function]
    /// pub fn call_me_from_cpp() { }
    /// ```
    ///
    /// See [`extern_rust_type`] for details of how to find the generated
    /// declarations from C++.
    pub use autocxx_macro::extern_rust_function;
}

/// Equivalent to [`std::convert::AsMut`], but returns a pinned mutable reference
/// such that cxx methods can be called on it.
pub trait PinMut<T>: AsRef<T> {
    /// Return a pinned mutable reference to a type.
    fn pin_mut(&mut self) -> std::pin::Pin<&mut T>;
}

/// Provides utility functions to emplace any [`moveit::New`] into a
/// [`cxx::UniquePtr`]. Automatically imported by the autocxx prelude
/// and implemented by any (autocxx-related) [`moveit::New`].
pub trait WithinUniquePtr {
    type Inner: UniquePtrTarget + MakeCppStorage;
    /// Create this item within a [`cxx::UniquePtr`].
    fn within_unique_ptr(self) -> cxx::UniquePtr<Self::Inner>;
}

/// Provides utility functions to emplace any [`moveit::New`] into a
/// [`Box`]. Automatically imported by the autocxx prelude
/// and implemented by any (autocxx-related) [`moveit::New`].
pub trait WithinBox {
    type Inner;
    /// Create this item inside a pinned box. This is a good option if you
    /// want to own this object within Rust, and want to create Rust references
    /// to it.
    fn within_box(self) -> Pin<Box<Self::Inner>>;
    /// Create this item inside a [`CppPin`]. This is a good option if you
    /// want to own this option within Rust, but you want to create [`CppRef`]
    /// C++ references to it.
    fn within_cpp_pin(self) -> CppPin<Self::Inner>;
}

use cxx::kind::Trivial;
use cxx::ExternType;
use moveit::Emplace;
use moveit::MakeCppStorage;

impl<N, T> WithinUniquePtr for N
where
    N: New<Output = T>,
    T: UniquePtrTarget + MakeCppStorage,
{
    type Inner = T;
    fn within_unique_ptr(self) -> cxx::UniquePtr<T> {
        UniquePtr::emplace(self)
    }
}

impl<N, T> WithinBox for N
where
    N: New<Output = T>,
{
    type Inner = T;
    fn within_box(self) -> Pin<Box<T>> {
        Box::emplace(self)
    }
    fn within_cpp_pin(self) -> CppPin<Self::Inner> {
        CppPin::from_pinned_box(Box::emplace(self))
    }
}

/// Emulates the [`WithinUniquePtr`] trait, but for trivial (plain old data) types.
/// This allows such types to behave identically if a type is changed from
/// `generate!` to `generate_pod!`.
///
/// (Ideally, this would be the exact same trait as [`WithinUniquePtr`] but this runs
/// the risk of conflicting implementations. Negative trait bounds would solve
/// this!)
pub trait WithinUniquePtrTrivial: UniquePtrTarget + Sized + Unpin {
    fn within_unique_ptr(self) -> cxx::UniquePtr<Self>;
}

impl<T> WithinUniquePtrTrivial for T
where
    T: UniquePtrTarget + ExternType<Kind = Trivial> + Sized + Unpin,
{
    fn within_unique_ptr(self) -> cxx::UniquePtr<T> {
        UniquePtr::new(self)
    }
}

/// Emulates the [`WithinBox`] trait, but for trivial (plain old data) types.
/// This allows such types to behave identically if a type is changed from
/// `generate!` to `generate_pod!`.
///
/// (Ideally, this would be the exact same trait as [`WithinBox`] but this runs
/// the risk of conflicting implementations. Negative trait bounds would solve
/// this!)
pub trait WithinBoxTrivial: Sized + Unpin {
    fn within_box(self) -> Pin<Box<Self>>;
}

impl<T> WithinBoxTrivial for T
where
    T: ExternType<Kind = Trivial> + Sized + Unpin,
{
    fn within_box(self) -> Pin<Box<T>> {
        Pin::new(Box::new(self))
    }
}

use cxx::memory::UniquePtrTarget;
use cxx::UniquePtr;
use moveit::New;
pub use rvalue_param::RValueParam;
pub use rvalue_param::RValueParamHandler;
pub use value_param::as_copy;
pub use value_param::as_mov;
pub use value_param::as_new;
pub use value_param::ValueParam;
pub use value_param::ValueParamHandler;

/// Imports which you're likely to want to use.
pub mod prelude {
    pub use crate::as_copy;
    pub use crate::as_mov;
    pub use crate::as_new;
    pub use crate::c_i128;
    pub use crate::c_i16;
    pub use crate::c_i32;
    pub use crate::c_i64;
    pub use crate::c_i8;
    pub use crate::c_int;
    pub use crate::c_long;
    pub use crate::c_longlong;
    pub use crate::c_short;
    pub use crate::c_u16;
    pub use crate::c_u32;
    pub use crate::c_u64;
    pub use crate::c_u8;
    pub use crate::c_uchar;
    pub use crate::c_uint;
    pub use crate::c_ulong;
    pub use crate::c_ulonglong;
    pub use crate::c_ushort;
    pub use crate::c_void;
    pub use crate::cpp_semantics;
    pub use crate::include_cpp;
    pub use crate::stack_slot;
    pub use crate::AsCppMutRef;
    pub use crate::AsCppRef;
    pub use crate::CppMutRef;
    pub use crate::CppPin;
    pub use crate::CppRef;
    pub use crate::CppUniquePtrPin;
    pub use crate::PinMut;
    pub use crate::RValueParam;
    pub use crate::TryWithinBox;
    pub use crate::TryWithinUniquePtr;
    pub use crate::ValueParam;
    pub use crate::WithinBox;
    pub use crate::WithinBoxTrivial;
    pub use crate::WithinUniquePtr;
    pub use crate::WithinUniquePtrTrivial;
    pub use cxx::UniquePtr;
    pub use moveit::moveit;
    pub use moveit::new::New;
    pub use moveit::new::TryNew;
    pub use moveit::Emplace;
}

/// Re-export moveit for ease of consumers.
pub use moveit;

/// Re-export cxx such that clients can use the same version as
/// us. This doesn't enable clients to avoid depending on the cxx
/// crate too, unfortunately, since generated cxx::bridge code
/// refers explicitly to ::cxx. See
/// <https://github.com/google/autocxx/issues/36>
pub use cxx;
