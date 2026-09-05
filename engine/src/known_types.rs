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

/// The behavior of the type.
#[derive(Debug)]
enum Behavior {
    CxxContainerPtr,
    CxxContainerVector,
    CxxString,
    RustStr,
    RustString,
    RustByValue,
    CByValue,
    CByValueVecSafe,
    CVariableLengthByValue,
    CVoid,
    CChar16,
    RustContainerByValueSafe,
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
        }
    }

    /// Whether and how to include this in the prelude given to bindgen.
    fn get_prelude_entry(&self) -> Option<String> {
        match self.behavior {
            Behavior::RustString
            | Behavior::RustStr
            | Behavior::CxxString
            | Behavior::CxxContainerPtr
            | Behavior::CxxContainerVector
            | Behavior::RustContainerByValueSafe => {
                let tn = QualifiedName::new_from_cpp_name(&self.rs_name);
                let cxx_name = tn.get_final_item();
                let (templating, payload) = match self.behavior {
                    Behavior::CxxContainerPtr
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
            _ => None,
        }
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
            Behavior::CxxContainerPtr => CxxGenericType::CppPtr,
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
    /// Some generic like cxx::UniquePtr where the contents must be a
    /// complete type.
    CppPtr,
    /// Some generic like cxx::Vector where the contents must be a
    /// complete type, and some types of int are allowed too.
    CppVector,
    /// Some generic like rust::Box where forward declarations are OK
    Rust,
}

pub struct KnownTypeConstructorDetails {
    pub has_move_constructor: bool,
    pub has_const_copy_constructor: bool,
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
                        Behavior::CxxContainerPtr
                        | Behavior::RustStr
                        | Behavior::RustString
                        | Behavior::RustByValue
                        | Behavior::CByValueVecSafe
                        | Behavior::CByValue
                        | Behavior::CVariableLengthByValue
                        | Behavior::CChar16
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

    /// Whether this can only be passed around using `std::move`
    pub(crate) fn lacks_copy_constructor(&self, tn: &QualifiedName) -> bool {
        self.get(tn)
            .map(|td| {
                matches!(
                    td.behavior,
                    Behavior::CxxContainerPtr
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
    /// substitute in the root mod under the name of the type it replaces
    /// (`std::string` becomes `root::string`) and nothing else about it says
    /// where it came from. A type of the user's own in the global namespace
    /// with such a name therefore collides with the substitute and is
    /// discarded along with it: `generate!` then reports that it generated
    /// nothing, which is at least honest, but the type can't be bound. Only
    /// the doc comment `bindgen` copies across (`<div rustbindgen="true"
    /// replaces="std::string">`) distinguishes the two, and relying on a doc
    /// comment surviving would be a good deal more fragile than this.
    /// Namespaced types are unaffected - `mine::string` is nobody's
    /// substitute. See `test_global_type_named_like_known_type_is_rejected`.
    pub(crate) fn is_known_substitute_type(&self, ty: &QualifiedName) -> bool {
        if ty.get_namespace().is_empty() {
            self.all_names()
                .any(|n| n.get_final_item() == ty.get_final_item())
        } else {
            false
        }
    }

    pub(crate) fn known_type_type_path(&self, ty: &QualifiedName) -> Option<TypePath> {
        self.get(ty).map(|td| td.to_type_path())
    }

    /// The canonical name of this type if it is one of the ctypes - the
    /// variable length integers, `void` and `char16_t` - which we need to
    /// wrap, and `None` if it isn't one of them.
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
                    Behavior::CVariableLengthByValue | Behavior::CVoid | Behavior::CChar16
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

    pub(crate) fn permissible_within_vector(&self, ty: &QualifiedName) -> bool {
        self.get(ty)
            .map(|x| matches!(x.behavior, Behavior::CxxString | Behavior::CByValueVecSafe))
            .unwrap_or(true)
    }

    pub(crate) fn permissible_within_unique_ptr(&self, ty: &QualifiedName) -> bool {
        self.get(ty)
            .map(|x| {
                matches!(
                    x.behavior,
                    Behavior::CxxString | Behavior::CxxContainerVector
                )
            })
            .unwrap_or(true)
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
        self.canonical_names.insert(
            QualifiedName::new_from_cpp_name(&td.cpp_name),
            rs_name.clone(),
        );
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
        Behavior::CxxContainerPtr,
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
        Behavior::CxxContainerPtr,
        None,
        true,
        true,
    ));
    db.insert(TypeDetails::new(
        "cxx::WeakPtr",
        "std::weak_ptr",
        Behavior::CxxContainerPtr,
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
            Behavior::CVariableLengthByValue,
            Some(format!("std::os::raw::c_{concatenated_name}")),
            true,
            true,
        ));
        db.insert(TypeDetails::new(
            format!("autocxx::c_u{concatenated_name}"),
            format!("unsigned {cname}"),
            Behavior::CVariableLengthByValue,
            Some(format!("std::os::raw::c_u{concatenated_name}")),
            true,
            true,
        ));
    };

    insert_ctype("long");
    insert_ctype("int");
    insert_ctype("short");
    insert_ctype("long long");

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
        Behavior::CByValue,
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
    db.insert(TypeDetails::new(
        "autocxx::c_char16_t",
        "char16_t",
        Behavior::CChar16,
        Some("c_char16_t".into()),
        false,
        false,
    ));
    // `char16_t` never reaches us under any of the names above. bindgen emits
    // it as a bare `bindgen_cchar16_t`, which `engine/src/lib.rs` binds to
    // `autocxx::c_char16_t` with a `use` injected into every module. Unless
    // that name is known here too, every function which mentions a `char16_t`
    // is discarded for depending on a type we've never heard of.
    db.insert_alias("bindgen_cchar16_t", "autocxx::c_char16_t");
    // TODO: `char16_t`'s three siblings - `char8_t`, `char32_t` and `wchar_t`
    // - and `long double` have no entry here and cannot be given one from this
    // side. They are distinct types in C++, but nothing distinguishing reaches
    // us, so a `char32_t` and a `uint32_t` are the same token by the time we
    // see them and we cannot even refuse the function cleanly. Each is blocked
    // in autocxx-bindgen, but by a different thing:
    //
    // - `char32_t` is collapsed on purpose: `CXType_Char32 =>
    //   TypeKind::Int(IntKind::U32)` in `build_builtin_ty`. `wchar_t` keeps an
    //   `IntKind::WChar` but codegen renders it through
    //   `Layout::known_type_for_size`, so it arrives as a bare `u16`/`u32`.
    //   Both need exactly the edit `char16_t` already had: an option like
    //   `use_distinct_char16_t` and a marker rendering that survives codegen.
    //
    // - `char8_t` is not collapsed but unrecognised: libclang has no
    //   `CXType_Char8` (the kinds go `CXType_UChar`, `CXType_Char16`,
    //   `CXType_Char32`), so `build_builtin_ty` returns `None` and bindgen
    //   falls back to an opaque type of the right layout - we receive
    //   `__bindgen_marker_Opaque<u8>` and unwrap it to `u8`. bindgen cannot
    //   add a `Char8` arm until libclang exposes the kind; it would have to
    //   recognise the type another way first.
    //
    // - `long double` renders by layout size, so where it is 8 bytes (MSVC,
    //   64-bit Arm) it behaves like the collapses above, and where it is 16
    //   (x86-64 System V) `FloatKind::LongDouble` becomes
    //   `integer_type(layout)`, i.e. `u128` - which is not registered here at
    //   all, so on those targets the function is rejected during our own
    //   analysis rather than by the C++ compiler.
    //
    // Once a `bindgen_c*_t` name arrives for one of them, it needs what
    // `char16_t` has: a `TypeDetails` entry above, an `insert_alias` here, the
    // injected `use` in `engine/src/lib.rs`, the guard in `parse_bindgen.rs`,
    // and a `#[repr(transparent)]` newtype in the `autocxx` crate - whose
    // payload, for `wchar_t` and `long double`, has to be chosen per target.
    // The integration tests for all four are written and `#[ignore]`d.
    db
}
