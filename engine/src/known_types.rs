// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::types::{make_ident, QualifiedName};
use indexmap::map::IndexMap as HashMap;
use indoc::indoc;
use once_cell::sync::OnceCell;
use syn::{parse_quote, TypePath};

/// The C++ character types which no Rust primitive is, listed as
/// `(C++ spelling, the name bindgen invents for it, our newtype's path)`.
///
/// Each is its own type in C++ - `char16_t` is not `uint16_t`, and a C++
/// compiler checking a function pointer's type says so - but Rust has no
/// equivalent, so bindgen is asked (`use_distinct_char16_t` and friends) to
/// emit a name of its own invention rather than the integer of the same width.
/// This table is what binds that name: it feeds the entries below, the `use`
/// injected into every bindgen module by `engine/src/lib.rs`, and the guard in
/// `parse_bindgen.rs` which keeps that `use` from being read back as a typedef.
pub(crate) const CXX_CHARACTER_TYPES: &[(&str, &str, &str)] = &[
    ("char16_t", "bindgen_cchar16_t", "autocxx::c_char16_t"),
    ("wchar_t", "bindgen_cwchar_t", "autocxx::c_wchar_t"),
    ("char32_t", "bindgen_cchar32_t", "autocxx::c_char32_t"),
    ("char8_t", "bindgen_cchar8_t", "autocxx::c_char8_t"),
];

/// The behavior of the type.
///
/// The three C++ smart pointers are told apart because cxx asks a different
/// question of each one's payload - see [`TypeDatabase::permissible_within_unique_ptr`]
/// and [`TypeDatabase::permissible_within_shared_or_weak_ptr`] - and so is
/// `char` from `bool`, which cxx takes in a `shared_ptr` and not in a
/// `vector`, where `char` is welcome in neither.
#[derive(Debug)]
enum Behavior {
    CxxContainerUniquePtr,
    CxxContainerSharedPtr,
    CxxContainerVector,
    CxxString,
    RustStr,
    RustString,
    RustByValue,
    CByValue,
    CChar,
    CByValueVecSafe,
    /// A C integer which reaches Rust as one of the `autocxx::c_*` newtypes
    /// and the generated C++ as a typedef, because cxx has no atom for it
    /// under that name: the variable-length ones, which cxx cannot spell at
    /// all, and the fixed-width ones, whose own spelling is an atom cxx will
    /// not put in a `unique_ptr`.
    CIntegerWrapper,
    CVoid,
    /// One of [`CXX_CHARACTER_TYPES`]: a C++ character type which no Rust
    /// primitive is, so we wrap it in a newtype of our own and emit a C++
    /// typedef naming it.
    CCharacter,
    RustContainerByValueSafe,
}

impl Behavior {
    /// Whether destroying a value of a type which behaves this way does
    /// nothing at all, so that C++ calls the destructor of a class holding one
    /// trivial. Note this is a stricter question than
    /// [`TypeDatabase::get_pod_safe_types`] asks: a `UniquePtr` is perfectly
    /// safe for Rust to hold by value, and it certainly has a destructor.
    fn destructor_is_trivial(&self) -> bool {
        match self {
            // Primitives, and `Pin<&T>`, which is a reference.
            Behavior::CByValue
            | Behavior::CChar
            | Behavior::CByValueVecSafe
            | Behavior::CIntegerWrapper
            | Behavior::CCharacter
            | Behavior::CVoid
            | Behavior::RustByValue
            // `rust::Str` is a borrowed (pointer, length) pair.
            | Behavior::RustStr => true,
            // Each of these owns something it has to give back: a heap
            // allocation, a refcount, or a Rust `Box`.
            Behavior::CxxString
            | Behavior::CxxContainerUniquePtr
            | Behavior::CxxContainerSharedPtr
            | Behavior::CxxContainerVector
            | Behavior::RustString
            | Behavior::RustContainerByValueSafe => false,
        }
    }

    /// Whether a class standing in for this type goes into the prelude handed
    /// to bindgen, so that bindgen replaces the real C++ type with it.
    ///
    /// These are the types bindgen cannot describe at all - the STL containers
    /// and `std::string`, and cxx's own Rust vocabulary types as C++ sees them.
    /// Everything else in the database is a type bindgen emits for itself and
    /// autocxx merely recognises the name of.
    fn has_prelude_entry(&self) -> bool {
        match self {
            Behavior::RustString
            | Behavior::RustStr
            | Behavior::CxxString
            | Behavior::CxxContainerUniquePtr
            | Behavior::CxxContainerSharedPtr
            | Behavior::CxxContainerVector
            | Behavior::RustContainerByValueSafe => true,
            Behavior::CByValue
            | Behavior::CChar
            | Behavior::CByValueVecSafe
            | Behavior::CIntegerWrapper
            | Behavior::CCharacter
            | Behavior::CVoid
            | Behavior::RustByValue => false,
        }
    }
}

/// Details about known special types, mostly primitives.
#[derive(Debug)]
struct TypeDetails {
    /// The name used by cxx (in Rust code) for this type.
    rs_name: String,
    /// C++ equivalent name for a Rust type.
    cpp_name: String,
    /// The behavior of the type.
    behavior: Behavior,
    /// Any extra non-canonical names
    extra_non_canonical_name: Option<String>,
    has_const_copy_constructor: bool,
    has_move_constructor: bool,
    /// Whether [`Self::cpp_name`] is also a name this entry answers to.
    ///
    /// It normally is - `int` is `autocxx::c_int` and nothing else - and
    /// [`TypeDatabase::insert`] records it as such, which is how a C++
    /// spelling reaches the entry at all. The fixed-width wrappers are the
    /// exception: `autocxx::c_u32` is a `uint32_t`, but that spelling belongs
    /// to `u32`, and claiming it would turn every `uint32_t` in the header
    /// into the wrapper instead of only a `unique_ptr` payload.
    owns_cpp_name: bool,
    /// Whether a cxx container of this type has the trait impls it needs.
    ///
    /// For everything else in this database it does: cxx implements them for
    /// its own types, and `autocxx::c_type_vectors` writes the explicit shim
    /// trait impls for the `autocxx::c_*` integers and character types. Three
    /// are the exception, all because `c_type_vectors.h` has to name the C++
    /// type and is compiled on every target autocxx supports, at the C++14
    /// floor: `autocxx::c_i128` and `autocxx::c_u128`, because MSVC has no
    /// `__int128`, and `autocxx::c_char8_t`, because `char8_t` is a C++20
    /// keyword and names nothing before that. A container of any of them would
    /// compile into a call to a `cxxbridge1$unique_ptr$...` symbol nobody
    /// emits. Enforced by the three `permissible_within_*` predicates below.
    has_container_glue: bool,
}

impl TypeDetails {
    fn new(
        rs_name: impl Into<String>,
        cpp_name: impl Into<String>,
        behavior: Behavior,
        extra_non_canonical_name: Option<String>,
        has_const_copy_constructor: bool,
        has_move_constructor: bool,
    ) -> Self {
        TypeDetails {
            rs_name: rs_name.into(),
            cpp_name: cpp_name.into(),
            behavior,
            extra_non_canonical_name,
            has_const_copy_constructor,
            has_move_constructor,
            owns_cpp_name: true,
            has_container_glue: true,
        }
    }

    /// Records that another entry owns this one's C++ spelling. See
    /// [`Self::owns_cpp_name`].
    fn sharing_cpp_name(mut self) -> Self {
        self.owns_cpp_name = false;
        self
    }

    /// Records that no cxx container of this type can be built. See
    /// [`Self::has_container_glue`].
    fn without_container_glue(mut self) -> Self {
        self.has_container_glue = false;
        self
    }

    /// Whether and how to include this in the prelude given to bindgen.
    fn get_prelude_entry(&self) -> Option<String> {
        if !self.behavior.has_prelude_entry() {
            return None;
        }
        let tn = QualifiedName::new_from_cpp_name(&self.rs_name);
        let cxx_name = tn.get_final_item();
        let (templating, payload) = match self.behavior {
            Behavior::CxxContainerUniquePtr
            | Behavior::CxxContainerSharedPtr
            | Behavior::CxxContainerVector
            | Behavior::RustContainerByValueSafe => ("template<typename T> ", "T* ptr"),
            _ => ("", "char* ptr"),
        };
        Some(format!(
            indoc! {"
            /**
            * <div rustbindgen=\"true\" replaces=\"{}\"></div>
            */
            {}class {} {{
                {};
            }};
            "},
            self.cpp_name, templating, cxx_name, payload
        ))
    }

    /// The name bindgen gives the stand-in it puts in the bindings in place of
    /// this type, or `None` if it substitutes nothing for it.
    ///
    /// bindgen renames a prelude class to the final segment of the C++ name the
    /// class says it replaces, so the stand-in for `rust::Str` is `Str` and the
    /// one for `std::string` is `string`. Those eight names are all a bindings
    /// dump contains for this database - checked, because it is the substitute's
    /// name and not the prelude class's.
    fn substitute_name(&self) -> Option<&str> {
        self.behavior.has_prelude_entry().then(|| {
            self.cpp_name
                .rsplit("::")
                .next()
                .expect("a name has a final segment")
        })
    }

    fn to_type_path(&self) -> TypePath {
        let mut segs = self.rs_name.split("::").peekable();
        if segs.peek().map(|seg| seg.is_empty()).unwrap_or_default() {
            segs.next();
            let segs = segs.map(make_ident);
            parse_quote! {
                ::#(#segs)::*
            }
        } else {
            let segs = segs.map(make_ident);
            parse_quote! {
                #(#segs)::*
            }
        }
    }

    fn to_typename(&self) -> QualifiedName {
        QualifiedName::new_from_cpp_name(&self.rs_name)
    }

    fn get_generic_behavior(&self) -> CxxGenericType {
        match self.behavior {
            Behavior::CxxContainerUniquePtr => CxxGenericType::CppUniquePtr,
            Behavior::CxxContainerSharedPtr => CxxGenericType::CppSharedPtr,
            Behavior::CxxContainerVector => CxxGenericType::CppVector,
            Behavior::RustContainerByValueSafe => CxxGenericType::Rust,
            _ => CxxGenericType::Not,
        }
    }
}

/// Database of known types.
#[derive(Default)]
pub(crate) struct TypeDatabase {
    by_rs_name: HashMap<QualifiedName, TypeDetails>,
    canonical_names: HashMap<QualifiedName, QualifiedName>,
    /// For each cxx atom which cannot be a `std::unique_ptr` payload, the
    /// `autocxx::c_*` wrapper naming the same C++ type, which can.
    ///
    /// See [`Self::unique_ptr_payload_wrapper`].
    unique_ptr_payload_wrappers: HashMap<QualifiedName, QualifiedName>,
}

/// Returns a database of known types.
pub(crate) fn known_types() -> &'static TypeDatabase {
    static KNOWN_TYPES: OnceCell<TypeDatabase> = OnceCell::new();
    KNOWN_TYPES.get_or_init(create_type_database)
}

/// The type of payload that a cxx generic can contain.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum CxxGenericType {
    /// Not a generic at all
    Not,
    /// `cxx::UniquePtr`, whose contents must be a complete type and none of
    /// cxx's own atoms but `CxxString`.
    CppUniquePtr,
    /// `cxx::SharedPtr` or `cxx::WeakPtr`, which take the same contents as a
    /// `UniquePtr` plus the numeric atoms, and no `CxxVector`.
    CppSharedPtr,
    /// Some generic like cxx::Vector where the contents must be a
    /// complete type, and some types of int are allowed too.
    CppVector,
    /// Some generic like rust::Box where forward declarations are OK
    Rust,
}

pub struct KnownTypeConstructorDetails {
    pub has_move_constructor: bool,
    pub has_const_copy_constructor: bool,
    /// Whether destroying one of these does nothing at all, so that C++ would
    /// call the destructor of a class holding one trivial.
    pub destructor_is_trivial: bool,
}

impl TypeDatabase {
    fn get(&self, ty: &QualifiedName) -> Option<&TypeDetails> {
        // The following line is important. It says that
        // when we encounter something like 'std::unique_ptr'
        // in the bindgen-generated bindings, we'll immediately
        // start to refer to that as 'UniquePtr' henceforth.
        let canonical_name = self.canonical_names.get(ty).unwrap_or(ty);
        self.by_rs_name.get(canonical_name)
    }

    /// Prelude of C++ for squirting into bindgen. This configures
    /// bindgen to output simpler types to replace some STL types
    /// that bindgen just can't cope with. Although we then replace
    /// those types with cxx types (e.g. UniquePtr), this intermediate
    /// step is still necessary because bindgen can't otherwise
    /// give us the templated types (e.g. when faced with the STL
    /// unique_ptr, bindgen would normally give us std_unique_ptr
    /// as opposed to std_unique_ptr<T>.)
    pub(crate) fn get_prelude(&self) -> String {
        itertools::join(
            self.by_rs_name
                .values()
                .filter_map(|t| t.get_prelude_entry()),
            "",
        )
    }

    /// Returns all known types.
    pub(crate) fn all_names(&self) -> impl Iterator<Item = &QualifiedName> {
        self.canonical_names.keys().chain(self.by_rs_name.keys())
    }

    /// Types which are known to be safe (or unsafe) to hold and pass by
    /// value in Rust.
    pub(crate) fn get_pod_safe_types(&self) -> impl Iterator<Item = (QualifiedName, bool)> {
        let pod_safety = self
            .all_names()
            .map(|tn| {
                (
                    tn.clone(),
                    match self.get(tn).unwrap().behavior {
                        Behavior::CxxContainerUniquePtr
                        | Behavior::CxxContainerSharedPtr
                        | Behavior::RustStr
                        | Behavior::RustString
                        | Behavior::RustByValue
                        | Behavior::CByValueVecSafe
                        | Behavior::CByValue
                        | Behavior::CChar
                        | Behavior::CIntegerWrapper
                        | Behavior::CCharacter
                        | Behavior::RustContainerByValueSafe => true,
                        Behavior::CxxString | Behavior::CxxContainerVector | Behavior::CVoid => {
                            false
                        }
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        pod_safety.into_iter()
    }

    pub(crate) fn get_constructor_details(
        &self,
        qn: &QualifiedName,
    ) -> Option<KnownTypeConstructorDetails> {
        self.get(qn).map(|x| KnownTypeConstructorDetails {
            has_move_constructor: x.has_move_constructor,
            has_const_copy_constructor: x.has_const_copy_constructor,
            destructor_is_trivial: x.behavior.destructor_is_trivial(),
        })
    }

    /// Whether this TypePath should be treated as a value in C++
    /// but a reference in Rust. This only applies to rust::Str
    /// (C++ name) which is &str in Rust.
    pub(crate) fn should_dereference_in_cpp(&self, tn: &QualifiedName) -> bool {
        self.get(tn)
            .map(|td| matches!(td.behavior, Behavior::RustStr))
            .unwrap_or(false)
    }

    /// Whether a value of this type can be copied out of a `volatile` object.
    ///
    /// Being copyable is not enough. C++ copies a class by calling a
    /// constructor, and an implicitly declared copy constructor takes
    /// `const T&` or `T&` - neither of which a `volatile T` lvalue binds to -
    /// so `std::string`, `rust::String` and everything else class-shaped here
    /// cannot be read out of a `volatile` member however ordinary its copy
    /// constructor is. What can is a scalar: C++ copies one by reading it, and
    /// reading a `volatile` glvalue is exactly the volatile access which makes
    /// such a getter honest.
    pub(crate) fn copyable_from_volatile(&self, tn: &QualifiedName) -> bool {
        self.get(tn)
            .map(|td| {
                matches!(
                    td.behavior,
                    Behavior::CByValue
                        | Behavior::CByValueVecSafe
                        | Behavior::CChar
                        | Behavior::CCharacter
                        | Behavior::CIntegerWrapper
                )
            })
            .unwrap_or(false)
    }

    /// Whether this can only be passed around using `std::move`
    pub(crate) fn lacks_copy_constructor(&self, tn: &QualifiedName) -> bool {
        self.get(tn)
            .map(|td| {
                matches!(
                    td.behavior,
                    Behavior::CxxContainerUniquePtr
                        | Behavior::CxxContainerSharedPtr
                        | Behavior::CxxContainerVector
                        | Behavior::RustContainerByValueSafe
                )
            })
            .unwrap_or(false)
    }

    /// Here we substitute any names which we know are Special from
    /// our type database, e.g. std::unique_ptr -> UniquePtr.
    /// We strip off and ignore
    /// any PathArguments within this TypePath - callers should
    /// put them back again if needs be.
    pub(crate) fn consider_substitution(&self, tn: &QualifiedName) -> Option<TypePath> {
        self.get(tn).map(|td| td.to_type_path())
    }

    pub(crate) fn special_cpp_name(&self, rs: &QualifiedName) -> Option<String> {
        self.get(rs).map(|x| x.cpp_name.to_string())
    }

    pub(crate) fn is_known_type(&self, ty: &QualifiedName) -> bool {
        self.get(ty).is_some()
    }

    /// Whether this is the substitute type we made for some known type.
    ///
    /// This matches on the final name alone, because `bindgen` puts the
    /// substitute under the name of the type it replaces and nothing else about
    /// it says where it came from. A type of the user's own in the global
    /// namespace with such a name therefore collides with the substitute and is
    /// discarded along with it: `generate!` then reports that it generated
    /// nothing, which is at least honest, but the type can't be bound. Only
    /// the doc comment `bindgen` copies across (`<div rustbindgen="true"
    /// replaces="std::string">`) distinguishes the two, and relying on a doc
    /// comment surviving would be a good deal more fragile than this.
    /// Namespaced types are unaffected - `mine::string` is nobody's
    /// substitute. See `test_global_type_named_like_known_type_is_rejected`.
    ///
    /// The price is paid only by the names `bindgen` actually substitutes
    /// something for, which is the eight with a prelude entry - and not by the
    /// rest of the database, which is every C++ type autocxx can spell. Asking
    /// about all of them made the user's own `c_u32`, `c_int`, `c_wchar_t` and
    /// the rest of the `autocxx::c_*` family unbindable, none of which
    /// `bindgen` replaces anything with. See
    /// `test_global_type_named_like_a_ctype_wrapper_is_generated`.
    ///
    /// The names in this database which have no namespace of their own -
    /// `usize`, `bool`, `str`, `uint32_t` - are a separate matter and are
    /// declined still, a few lines later in
    /// [`crate::conversion::parse::parse_bindgen`], by [`Self::is_known_type`]:
    /// they *are* the names bindgen writes for those types, so a struct
    /// arriving under one has to be examined rather than assumed to be the
    /// user's. A C++ class named `u32` does not even arrive under that name -
    /// bindgen escapes it to `u32_`, which is the name a `generate!` directive
    /// then has to use.
    ///
    /// Measured: of those eight substitutes, `bindgen` puts `Str`, `String` and
    /// `Box` in the root mod and the five `std` ones in `root::std`, so the
    /// namespace test below leaves the latter to
    /// [`Self::is_known_type`], which recognises them by their own names. A
    /// global `struct string` is therefore nobody's substitute either, and is
    /// declined here all the same; untangling that is a behaviour change which
    /// the test named above pins as it stands.
    ///
    /// One combination is refused rather than bound, and was refused before
    /// this narrowed too: a header with a global type named after one of the
    /// `autocxx::c_*` wrappers *and* a use of that same C type, which makes
    /// [`crate::conversion::analysis::ctypes`] declare a bridge type of the
    /// wrapper's name. The two are one name in a flat bridge, and the
    /// generated C++ would likewise declare `typedef unsigned int c_u32;`
    /// beside the user's `struct c_u32`, which C++ has no room for either.
    /// They annihilate in `ApiVec`, cxx rejects the bindings which still
    /// mention the name, and the build stops - loudly, but saying less than it
    /// should. Serving both would take the generated typedefs out of the
    /// user's global namespace, which is a change to every generated header.
    /// `test_ctype_wrapper_name_collision_is_refused` pins the refusal.
    pub(crate) fn is_known_substitute_type(&self, ty: &QualifiedName) -> bool {
        ty.get_namespace().is_empty()
            && self
                .by_rs_name
                .values()
                .filter_map(|td| td.substitute_name())
                .any(|substitute| substitute == ty.get_final_item())
    }

    pub(crate) fn known_type_type_path(&self, ty: &QualifiedName) -> Option<TypePath> {
        self.get(ty).map(|td| td.to_type_path())
    }

    /// The canonical name of this type if it is one of the ctypes - the
    /// variable length integers, `void` and the C++ character types - which we
    /// need to wrap, and `None` if it isn't one of them.
    ///
    /// The answer is the canonical name rather than the name asked about
    /// because these types reach us under aliases - `char16_t` arrives as
    /// bindgen's `bindgen_cchar16_t` - and the wrapper has to be declared
    /// under the name the generated code then uses.
    pub(crate) fn as_ctype(&self, ty: &QualifiedName) -> Option<QualifiedName> {
        self.get(ty)
            .filter(|td| {
                matches!(
                    td.behavior,
                    Behavior::CIntegerWrapper | Behavior::CVoid | Behavior::CCharacter
                )
            })
            .map(|td| td.to_typename())
    }

    /// Whether this is a generic type acceptable to cxx. Otherwise,
    /// if we encounter a generic, we'll replace it with a synthesized concrete
    /// type.
    pub(crate) fn cxx_generic_behavior(&self, ty: &QualifiedName) -> CxxGenericType {
        self.get(ty)
            .map(|x| x.get_generic_behavior())
            .unwrap_or(CxxGenericType::Not)
    }

    pub(crate) fn is_cxx_acceptable_receiver(&self, ty: &QualifiedName) -> bool {
        self.get(ty).is_none() // at present, none of our known types can have
                               // methods attached.
    }

    /// Whether `std::vector<ty>` is something we can hand to cxx as a
    /// `CxxVector<ty>`.
    ///
    /// [`Behavior::CByValueVecSafe`] types are cxx's own built-in vector
    /// elements. [`Behavior::CIntegerWrapper`] and [`Behavior::CCharacter`]
    /// types - the `autocxx::c_int` and `autocxx::c_char16_t` families - are
    /// not, but we ask cxx to make them so by emitting
    /// `impl CxxVector<c_int> {}` into the generated bridge alongside the
    /// `type c_int = autocxx::c_int;` alias. See google/autocxx#422.
    ///
    /// Mirrors `check_type_cxx_vector`, cxx-gen 0.7.200
    /// `src/syntax/check.rs:211`, which takes neither `bool` nor `c_char`.
    pub(crate) fn permissible_within_vector(&self, ty: &QualifiedName) -> bool {
        self.get(ty)
            .map(|x| {
                x.has_container_glue
                    && matches!(
                        x.behavior,
                        Behavior::CxxString
                            | Behavior::CByValueVecSafe
                            | Behavior::CIntegerWrapper
                            | Behavior::CCharacter
                    )
            })
            .unwrap_or(true)
    }

    /// Whether cxx holds `ty` in an array with nothing else asked of it: one
    /// of its own atoms.
    ///
    /// cxx spells a Rust `[T; N]` as `std::array<T, N>` and moves one whole,
    /// so `T` has to be something it holds by value with no indirection of its
    /// own. An atom is that outright.
    ///
    /// An extern type the bridge declares is that too, but only once something
    /// in the bridge requires it to be trivially movable: `is_unsized` counts
    /// an alias as sized exactly then, and what decides it is
    /// `required_trivial_reasons`, cxx-gen 0.7.200
    /// `src/syntax/trivial.rs:30`, which reads a function argument, a return,
    /// a struct field, a `Box`, a `Vec` and a slice - and no array. So an
    /// alias is held in an array exactly when some *other* signature in the
    /// same bridge happens to take one by value, which is not a rule anyone
    /// can be given. autocxx states the requirement itself instead, in one of
    /// the forms cxx does read - see `array_element_witnesses`.
    ///
    /// Unknown to this database means a type the header declared. That is the
    /// alias case, and whether it may be an element is decided by whether
    /// autocxx proved it POD, which the caller is the one to know.
    pub(crate) fn permissible_within_array(&self, ty: &QualifiedName) -> bool {
        self.get(ty)
            .map(|x| {
                matches!(
                    x.behavior,
                    Behavior::CByValue | Behavior::CByValueVecSafe | Behavior::CChar
                )
            })
            .unwrap_or(false)
    }

    /// Whether `ty` is one of the newtypes autocxx declares to the bridge
    /// under a name of its own, each a transparent wrapper over a Rust
    /// primitive.
    ///
    /// These are trivially relocatable whatever width the platform gave the C
    /// type, so the only thing between one and an array element is the
    /// certificate cxx will not write for itself. That makes
    /// `[autocxx::c_uint; 4]` reachable, which is what a header saying
    /// `std::array<uint32_t, 4>` asks for.
    pub(crate) fn relocatable_newtype(&self, ty: &QualifiedName) -> bool {
        self.get(ty)
            .map(|x| matches!(x.behavior, Behavior::CIntegerWrapper | Behavior::CCharacter))
            .unwrap_or(false)
    }

    /// Whether cxx can accommodate `ty` inside a `std::unique_ptr`.
    ///
    /// cxx implements [`cxx::memory::UniquePtrTarget`] for `CxxString`,
    /// `CxxVector<T>` and the opaque C++ types a bridge declares - and it
    /// rejects, in its own macro, any `unique_ptr` whose target is one of its
    /// built-in atoms, so `UniquePtr<u32>` is out of reach from here.
    /// [`Behavior::CIntegerWrapper`] and [`Behavior::CCharacter`] types - the
    /// `autocxx::c_int` and `autocxx::c_char16_t` families - are not atoms as
    /// far as cxx is concerned, so the explicit shim trait impls this crate
    /// writes in `autocxx::c_type_vectors` make them work like any other named
    /// type. See google/autocxx#422.
    ///
    /// Mirrors `check_type_unique_ptr`, cxx-gen 0.7.200
    /// `src/syntax/check.rs:147`.
    pub(crate) fn permissible_within_unique_ptr(&self, ty: &QualifiedName) -> bool {
        self.get(ty)
            .map(|x| {
                x.has_container_glue
                    && matches!(
                        x.behavior,
                        Behavior::CxxString
                            | Behavior::CxxContainerVector
                            | Behavior::CIntegerWrapper
                            | Behavior::CCharacter
                    )
            })
            .unwrap_or(true)
    }

    /// Whether cxx can accommodate `ty` inside a `std::shared_ptr` or a
    /// `std::weak_ptr`.
    ///
    /// These two are more generous than `unique_ptr`: cxx implements
    /// `SharedPtrTarget` and `WeakPtrTarget` for every numeric atom and for
    /// `bool`, so `SharedPtr<u32>` needs no help from us. What it will not
    /// take is a `CxxVector` payload, or `c_char`, or a `String`. Asking the
    /// `unique_ptr` question of all three refused the numeric payloads for no
    /// reason and let `shared_ptr<vector<T>>` through to be refused by cxx.
    ///
    /// Mirrors `check_type_shared_ptr` and `check_type_weak_ptr`, cxx-gen
    /// 0.7.200 `src/syntax/check.rs:165` and `:188`, which agree with each
    /// other and with the trait impls in cxx's own `src/shared_ptr.rs:460`
    /// and `src/weak_ptr.rs:166`.
    pub(crate) fn permissible_within_shared_or_weak_ptr(&self, ty: &QualifiedName) -> bool {
        self.get(ty)
            .map(|x| {
                x.has_container_glue
                    && matches!(
                        x.behavior,
                        Behavior::CxxString
                            | Behavior::CByValue
                            | Behavior::CByValueVecSafe
                            | Behavior::CIntegerWrapper
                            | Behavior::CCharacter
                    )
            })
            .unwrap_or(true)
    }

    /// The `autocxx::c_*` wrapper to name a `std::unique_ptr` payload of `ty`
    /// with, where `ty` is a cxx atom which cannot be one.
    ///
    /// cxx turns down a `unique_ptr` of any of its own atoms, so
    /// `std::unique_ptr<uint32_t>` has no cxx spelling - even though
    /// `std::unique_ptr<unsigned int>`, which on most targets is the same C++
    /// type, has one as `UniquePtr<c_uint>`. The wrapper mirrors the width C++
    /// wrote rather than a width the target happens to agree with, so the
    /// generated shim says `uint32_t` and binds against the real signature
    /// wherever `uint32_t` is not an `unsigned int`.
    ///
    /// Only `unique_ptr` asks. A `uint32_t` by value, in a `std::vector` or in
    /// a `std::shared_ptr` keeps its atom, which cxx supports natively there
    /// and which is what callers have always received.
    pub(crate) fn unique_ptr_payload_wrapper(&self, ty: &QualifiedName) -> Option<&QualifiedName> {
        self.unique_ptr_payload_wrappers.get(ty)
    }

    pub(crate) fn conflicts_with_built_in_type(&self, ty: &QualifiedName) -> bool {
        self.get(ty).is_some()
    }

    pub(crate) fn convertible_from_strs(&self, ty: &QualifiedName) -> bool {
        self.get(ty)
            .map(|x| matches!(x.behavior, Behavior::CxxString))
            .unwrap_or(false)
    }

    /// Records one more name by which a type already in the database may be
    /// known, for the cases `TypeDetails::extra_non_canonical_name` can't
    /// express.
    fn insert_alias(&mut self, alias: &str, canonical_rs_name: &str) {
        self.canonical_names.insert(
            QualifiedName::new_from_cpp_name(alias),
            QualifiedName::new_from_cpp_name(canonical_rs_name),
        );
    }

    fn insert(&mut self, td: TypeDetails) {
        let rs_name = td.to_typename();
        if let Some(extra_non_canonical_name) = &td.extra_non_canonical_name {
            self.canonical_names.insert(
                QualifiedName::new_from_cpp_name(extra_non_canonical_name),
                rs_name.clone(),
            );
        }
        if td.owns_cpp_name {
            self.canonical_names.insert(
                QualifiedName::new_from_cpp_name(&td.cpp_name),
                rs_name.clone(),
            );
        }
        self.by_rs_name.insert(rs_name, td);
    }

    pub(crate) fn get_moveit_safe_types(&self) -> impl Iterator<Item = QualifiedName> + '_ {
        self.all_names()
            .filter(|tn| {
                !matches!(
                    self.get(tn).unwrap().behavior,
                    Behavior::CxxString | Behavior::CxxContainerVector
                )
            })
            .cloned()
    }
}

fn create_type_database() -> TypeDatabase {
    let mut db = TypeDatabase::default();
    db.insert(TypeDetails::new(
        "cxx::UniquePtr",
        "std::unique_ptr",
        Behavior::CxxContainerUniquePtr,
        None,
        false,
        true,
    ));
    db.insert(TypeDetails::new(
        "cxx::CxxVector",
        "std::vector",
        Behavior::CxxContainerVector,
        None,
        false,
        true,
    ));
    db.insert(TypeDetails::new(
        "cxx::SharedPtr",
        "std::shared_ptr",
        Behavior::CxxContainerSharedPtr,
        None,
        true,
        true,
    ));
    db.insert(TypeDetails::new(
        "cxx::WeakPtr",
        "std::weak_ptr",
        Behavior::CxxContainerSharedPtr,
        None,
        true,
        true,
    ));
    db.insert(TypeDetails::new(
        "cxx::CxxString",
        "std::string",
        Behavior::CxxString,
        None,
        true,
        true,
    ));
    db.insert(TypeDetails::new(
        "str",
        "rust::Str",
        Behavior::RustStr,
        None,
        true,
        false,
    ));
    db.insert(TypeDetails::new(
        "String",
        "rust::String",
        Behavior::RustString,
        None,
        true,
        true,
    ));
    db.insert(TypeDetails::new(
        "std::boxed::Box",
        "rust::Box",
        Behavior::RustContainerByValueSafe,
        None,
        false,
        true,
    ));
    db.insert(TypeDetails::new(
        "i8",
        "int8_t",
        Behavior::CByValueVecSafe,
        Some("std::os::raw::c_schar".into()),
        true,
        true,
    ));
    db.insert(TypeDetails::new(
        "u8",
        "uint8_t",
        Behavior::CByValueVecSafe,
        Some("std::os::raw::c_uchar".into()),
        true,
        true,
    ));
    for (cpp_type, rust_type) in (4..7).map(|x| 2i32.pow(x)).flat_map(|x| {
        vec![
            (format!("uint{x}_t"), format!("u{x}")),
            (format!("int{x}_t"), format!("i{x}")),
        ]
    }) {
        db.insert(TypeDetails::new(
            rust_type,
            cpp_type,
            Behavior::CByValueVecSafe,
            None,
            true,
            true,
        ));
    }

    // The same eight C++ types under names cxx has no atom for, so that a
    // `std::unique_ptr` of one has a payload cxx will accept. Nothing reaches
    // these by their C++ spelling - `sharing_cpp_name` keeps `uint8_t` and
    // friends meaning the atoms inserted above - only
    // `unique_ptr_payload_wrapper`, which is asked in the one position where
    // the atom is refused. See google/autocxx#422.
    for (cpp_type, atom) in (3..7).map(|x| 2i32.pow(x)).flat_map(|x| {
        vec![
            (format!("uint{x}_t"), format!("u{x}")),
            (format!("int{x}_t"), format!("i{x}")),
        ]
    }) {
        let wrapper = format!("autocxx::c_{atom}");
        db.insert(
            TypeDetails::new(
                wrapper.clone(),
                cpp_type,
                Behavior::CIntegerWrapper,
                None,
                true,
                true,
            )
            .sharing_cpp_name(),
        );
        db.unique_ptr_payload_wrappers.insert(
            QualifiedName::new_from_cpp_name(&atom),
            QualifiedName::new_from_cpp_name(&wrapper),
        );
    }

    db.insert(TypeDetails::new(
        "bool",
        "bool",
        Behavior::CByValue,
        None,
        true,
        true,
    ));

    db.insert(TypeDetails::new(
        "core::pin::Pin",
        "Pin",
        Behavior::RustByValue, // because this is actually Pin<&something>
        Some("std::pin::Pin".to_string()),
        true,
        false,
    ));

    let mut insert_ctype = |cname: &str| {
        let concatenated_name = cname.replace(' ', "");
        db.insert(TypeDetails::new(
            format!("autocxx::c_{concatenated_name}"),
            cname,
            Behavior::CIntegerWrapper,
            Some(format!("std::os::raw::c_{concatenated_name}")),
            true,
            true,
        ));
        db.insert(TypeDetails::new(
            format!("autocxx::c_u{concatenated_name}"),
            format!("unsigned {cname}"),
            Behavior::CIntegerWrapper,
            Some(format!("std::os::raw::c_u{concatenated_name}")),
            true,
            true,
        ));
    };

    insert_ctype("long");
    insert_ctype("int");
    insert_ctype("short");
    insert_ctype("long long");

    // The two 128-bit C++ integers, which reach us as bare `i128` and `u128`.
    // cxx has no atom that wide, so each travels as a named type exactly like
    // `autocxx::c_int` - but without the container glue, because
    // `autocxx::c_type_vectors` compiles on every target autocxx supports and
    // MSVC has neither type.
    //
    // A bare `u128` means `unsigned __int128` and nothing else only because
    // `34-float128-newtype-marker.patch` marks the other claimant on that
    // token; see the note at the end of this function.
    for (rs_name, cpp_name, bindgen_name) in [
        ("autocxx::c_i128", "__int128", "i128"),
        ("autocxx::c_u128", "unsigned __int128", "u128"),
    ] {
        db.insert(
            TypeDetails::new(
                rs_name,
                cpp_name,
                Behavior::CIntegerWrapper,
                Some(bindgen_name.into()),
                true,
                true,
            )
            .without_container_glue(),
        );
    }

    db.insert(TypeDetails::new(
        "f32",
        "float",
        Behavior::CByValueVecSafe,
        None,
        true,
        true,
    ));
    db.insert(TypeDetails::new(
        "f64",
        "double",
        Behavior::CByValueVecSafe,
        None,
        true,
        true,
    ));
    db.insert(TypeDetails::new(
        "::std::os::raw::c_char",
        "char",
        Behavior::CChar,
        None,
        true,
        true,
    ));
    db.insert(TypeDetails::new(
        "usize",
        "size_t",
        Behavior::CByValueVecSafe,
        None,
        true,
        true,
    ));
    db.insert(TypeDetails::new(
        "autocxx::c_void",
        "void",
        Behavior::CVoid,
        Some("std::os::raw::c_void".into()),
        false,
        false,
    ));
    for (cpp_name, bindgen_name, rs_name) in CXX_CHARACTER_TYPES {
        let details = TypeDetails::new(
            *rs_name,
            *cpp_name,
            Behavior::CCharacter,
            Some(
                rs_name
                    .rsplit("::")
                    .next()
                    .expect("a path has a final segment")
                    .to_string(),
            ),
            false,
            false,
        );
        // `char8_t` gets no cxx container glue. Writing it would take a
        // `typedef char8_t c_char8_t;` in `c_type_vectors.h`, which this crate
        // compiles at the C++14 floor every consumer gets, and `char8_t` is a
        // C++20 keyword: before C++20 the typedef names nothing and the file
        // does not build for anybody. Raising that floor, or guessing the
        // consumer's standard, is not something a container payload is worth -
        // and a typedef to some other one-byte type would be a different C++
        // type wearing the same shim names.
        db.insert(if *cpp_name == "char8_t" {
            details.without_container_glue()
        } else {
            details
        });
        // None of these reaches us under any of the names above: bindgen emits
        // the fake name, which `engine/src/lib.rs` binds to the newtype with a
        // `use` injected into every module. Unless that name is known here
        // too, every function which mentions the type is discarded for
        // depending on a type we've never heard of.
        db.insert_alias(bindgen_name, rs_name);
    }
    // `long double` is the fifth C++ built-in with no Rust equivalent, and the
    // one which gets no entry here, because what to put in it differs by
    // target: `double` under another name on MSVC and Apple Arm, an 80-bit x87
    // float in 16 bytes on x86-64 System V, an IEEE binary128 on AArch64
    // Linux. Rust has no type for the last two, and for the first it has one
    // which is the wrong C++ type - which cxx catches, because it checks a
    // function's exact type. So the answer is a refusal rather than a newtype,
    // and `25-long-double-newtype-marker.patch` is what makes the refusal
    // possible: it marks the type so that `type_converter` can name what it is
    // turning down instead of seeing bindgen's same-sized substitute. See
    // `ConvertErrorFromCpp::LongDouble`.
    //
    // `u128` used to be no more bindable than `long double`, for the same
    // reason: three C++ types arrived as that one token - `unsigned __int128`,
    // a 16-byte `long double`, and `__float128`, which `FloatKind::Float128`
    // renders as a literal `u128` - and nothing which survived to this side
    // told them apart. An entry would have had to name one of the three in the
    // C++ it generates, which is a miscompile wherever the header meant
    // another: they are a different register class and a different value.
    //
    // Both of the other two are marked now -
    // `25-long-double-newtype-marker.patch` and
    // `34-float128-newtype-marker.patch` - and refused by name of their own,
    // which leaves the bare token meaning `unsigned __int128` and nothing
    // else. So it is registered above, beside `__int128`, which never had the
    // problem.
    db
}
