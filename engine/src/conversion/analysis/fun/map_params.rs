// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Recognising a `std::map` in a function's parameters, for the one rule a
//! parameter cannot decide for itself.
//!
//! cxx has no map type, so nothing standing for every possible `std::map`
//! could be declared in a crate the generated code does not own: the glue for
//! a key and value pair has to be implemented somewhere, and the orphan rule
//! puts that nowhere the generated code can reach for foreign pairs. What
//! autocxx does instead is what it does for any other template instantiation:
//! each specialization the headers use becomes its own generated opaque type,
//! and a parameter declared `const std::map<K, V>&` takes a reference to that
//! type - the C++ type the header wrote, handed over as itself. See
//! [`crate::conversion::api::HolderSurface::Map`], and
//! [`crate::known_types::Behavior::CxxOrderedMap`] for the stand-in which
//! keeps the template arguments bindgen would otherwise discard.
//!
//! What is left for this module is the refusals. bindgen's rendering erases
//! part of a map's shape - `std::less<>` comes out as the bare `std::less`, an
//! `unordered_map`'s hash not at all - so two declarations of one C++ name can
//! be indistinguishable here, and the call autocxx generates for one of them
//! may be the one C++ resolves to the other, or to a function template of the
//! same name. [`super::FnAnalyzer::map_refusal`] decides that, and these
//! predicates are what it asks.
//!
//! Every predicate sees a map through whatever the type converter serves one
//! through: references and `const` markers, pointers, by value, and the
//! typedefs which spell any of those - a map behind an alias is the same map,
//! and a refusal it slips past is a call quietly reaching the wrong function.
//! [`MapSpellings`] carries the alias targets that takes, gathered from the
//! same typedef items the type converter resolves through.

use indexmap::map::IndexMap as HashMap;
use quote::ToTokens;
use syn::{GenericArgument, PathArguments, Type, TypePath};

use crate::{
    conversion::{
        analysis::type_converter::alias_targets,
        api::{AnalysisPhase, FuncToConvert},
        apivec::ApiVec,
        type_helpers::{ptr_is_mut, unwrap_const, unwrap_reference, unwrap_volatile},
    },
    known_types::known_types,
    minisyn::FnArg,
    types::{Namespace, QualifiedName},
};

/// How many alias links a walk will follow. C++ cannot declare an alias
/// cycle, so this is armour against a malformed rendering, not a limit any
/// real chain reaches.
const MAX_ALIAS_DEPTH: usize = 32;

/// The alias targets these predicates resolve through, so that a map behind
/// a typedef answers exactly as the map spelled out would.
pub(super) struct MapSpellings {
    aliases: HashMap<QualifiedName, Type>,
}

/// The erased shape of one map, as the constructor capture census compares
/// maps: the template it instantiates and its rendered key and value, aliases
/// expanded. Two equal shapes came out of one rendering and would be served
/// by one generated type; two unequal shapes prove nothing on their own,
/// because bindgen gives one C++ type more than one spelling - `uint32_t`
/// renders as `u32` where `unsigned int` renders as `c_uint`. What lets a
/// sibling out of the census is only [`Self::provably_differs`].
pub(super) struct ErasedMapShape {
    /// `std::map` or `std::unordered_map`, as
    /// [`crate::known_types::TypeDatabase::is_map`] knows them.
    map: QualifiedName,
    /// The first two rendered template arguments - the key and the value.
    key: Option<Type>,
    value: Option<Type>,
}

/// A constructor parameter which is a reference to one of the two maps
/// itself: the erased shape of the referent and the one fact about the
/// reference the census's binding rules read.
pub(super) struct MapReferenceParameter {
    pub(super) shape: ErasedMapShape,
    /// Whether this is a const lvalue reference, the only reference kind
    /// which binds the const lvalue a `const M&` binding presents - `M&`
    /// and `M&&` never bind one.
    pub(super) binds_const_lvalues: bool,
}

/// The map argument a judged constructor's binding presents, in the
/// category which decides what a reference sibling could bind.
pub(super) enum PresentedMapArgument {
    /// Moved into the call (`M` by value, `M&&`) or handed over as a
    /// mutable lvalue (`M&`): some reference kind binds either - `const
    /// M&` binds both and `M&&` the rvalue.
    RvalueOrMutableLvalue(ErasedMapShape),
    /// Handed over as a const lvalue, from a `const M&` parameter: what
    /// only a const lvalue reference binds.
    ConstLvalue(ErasedMapShape),
}

impl ErasedMapShape {
    /// Whether these two shapes name different C++ types on every platform
    /// autocxx supports - the one fact which lets a reference sibling out of
    /// the capture census, a reference to the same map binding exactly what
    /// the judged constructor hands over. Anything short of proof answers
    /// no, and the census then keeps the sibling capture-capable.
    pub(super) fn provably_differs(&self, other: &Self) -> bool {
        // std::map and std::unordered_map are different class templates; no
        // specialization of one is a specialization of the other.
        if self.map != other.map {
            return true;
        }
        // Only the key and the value prove anything. A trailing comparator
        // argument does not: bindgen strips the arguments of a spelled-out
        // comparator, so `std::map<K, V, std::less<K>>` renders with a bare
        // `less` that `std::map<K, V>` renders without - one C++ type under
        // two renderings again.
        atoms_provably_differ(self.key.as_ref(), other.key.as_ref())
            || atoms_provably_differ(self.value.as_ref(), other.value.as_ref())
    }
}

/// Whether two rendered template arguments provably name different C++
/// types: each pins down the set of C++ types its rendering can stand for,
/// and the sets share nothing. An argument that pins no set down proves
/// nothing - two distinct class paths can still be one type through a
/// template alias [`MapSpellings::expand_aliases`] leaves as spelled - and
/// nor does a missing one.
fn atoms_provably_differ(a: Option<&Type>, b: Option<&Type>) -> bool {
    match (
        a.and_then(possible_cpp_types),
        b.and_then(possible_cpp_types),
    ) {
        (Some(a), Some(b)) => a & b == 0,
        _ => false,
    }
}

/// The C++ types a rendered atom can stand for, one bit each. C++ treats
/// every fundamental type as distinct however the widths fall - `unsigned
/// int` and `unsigned long` are different types where both are 32 bits - so
/// each gets its own bit and only the typedefs span several.
mod cpp {
    pub(super) const BOOL: u32 = 1 << 0;
    pub(super) const CHAR: u32 = 1 << 1;
    pub(super) const SIGNED_CHAR: u32 = 1 << 2;
    pub(super) const UNSIGNED_CHAR: u32 = 1 << 3;
    pub(super) const SHORT: u32 = 1 << 4;
    pub(super) const UNSIGNED_SHORT: u32 = 1 << 5;
    pub(super) const INT: u32 = 1 << 6;
    pub(super) const UNSIGNED_INT: u32 = 1 << 7;
    pub(super) const LONG: u32 = 1 << 8;
    pub(super) const UNSIGNED_LONG: u32 = 1 << 9;
    pub(super) const LONG_LONG: u32 = 1 << 10;
    pub(super) const UNSIGNED_LONG_LONG: u32 = 1 << 11;
    pub(super) const INT128: u32 = 1 << 12;
    pub(super) const UNSIGNED_INT128: u32 = 1 << 13;
    pub(super) const FLOAT: u32 = 1 << 14;
    pub(super) const DOUBLE: u32 = 1 << 15;
    pub(super) const CHAR8_T: u32 = 1 << 16;
    pub(super) const CHAR16_T: u32 = 1 << 17;
    pub(super) const CHAR32_T: u32 = 1 << 18;
    pub(super) const WCHAR_T: u32 = 1 << 19;
    pub(super) const STD_STRING: u32 = 1 << 20;
}

/// The set of C++ types this rendered template argument can stand for on a
/// platform autocxx supports, or `None` where the rendering pins no set
/// down.
///
/// The C++ declaration behind each rendering is a [`crate::known_types`]
/// fact -
/// the database says `u32` is bindgen's rendering of `uint32_t` and `c_uint`
/// its rendering of `unsigned int` - and the match here adds the one fact
/// the database does not carry: which fundamental types that C++ name is
/// allowed to be. A fundamental type stands only for itself, because C++
/// keeps every fundamental type distinct whatever its width; a `typedef`
/// gets every type a supported platform is permitted to define it as, one
/// direction only - over-inclusion costs a refusal, omission a silent
/// miscall.
fn possible_cpp_types(ty: &Type) -> Option<u32> {
    let Type::Path(tp) = ty else {
        return None;
    };
    if has_type_arguments(tp) {
        // A template instantiation, not an atom.
        return None;
    }
    let name = QualifiedName::from_type_path(tp);
    if known_types().is_cxx_string(&name) {
        // `std::string`: a class type, never any scalar.
        return Some(cpp::STD_STRING);
    }
    let cpp_name = known_types().special_cpp_name(&name)?;
    Some(match cpp_name.as_str() {
        // The fundamental types, each only itself.
        "bool" => cpp::BOOL,
        // `char` is its own type whatever its signedness, distinct from
        // both `signed char` and `unsigned char`.
        "char" => cpp::CHAR,
        "short" => cpp::SHORT,
        "unsigned short" => cpp::UNSIGNED_SHORT,
        "int" => cpp::INT,
        "unsigned int" => cpp::UNSIGNED_INT,
        "long" => cpp::LONG,
        "unsigned long" => cpp::UNSIGNED_LONG,
        "long long" => cpp::LONG_LONG,
        "unsigned long long" => cpp::UNSIGNED_LONG_LONG,
        "__int128" => cpp::INT128,
        "unsigned __int128" => cpp::UNSIGNED_INT128,
        "float" => cpp::FLOAT,
        "double" => cpp::DOUBLE,
        // The wide and Unicode character types: each a distinct fundamental
        // type, never a typedef of any integer.
        "char8_t" => cpp::CHAR8_T,
        "char16_t" => cpp::CHAR16_T,
        "char32_t" => cpp::CHAR32_T,
        "wchar_t" => cpp::WCHAR_T,
        // The stdint typedefs. int8_t and uint8_t must be the signed and
        // unsigned char types - plain `char` is not among the standard
        // signed or unsigned integer types - and 16 bits is `short` on
        // every supported platform, `int` being at least 32 bits wherever
        // autocxx builds.
        "int8_t" => cpp::SIGNED_CHAR,
        "uint8_t" => cpp::UNSIGNED_CHAR,
        "int16_t" => cpp::SHORT,
        "uint16_t" => cpp::UNSIGNED_SHORT,
        // 32 bits is `int` on the mainstream ABIs and `long` on the ILP32
        // C libraries which define int32_t so, so both stay in the set.
        "int32_t" => cpp::INT | cpp::LONG,
        "uint32_t" => cpp::UNSIGNED_INT | cpp::UNSIGNED_LONG,
        // 64 bits is `long` under LP64 and `long long` under LLP64.
        "int64_t" => cpp::LONG | cpp::LONG_LONG,
        "uint64_t" => cpp::UNSIGNED_LONG | cpp::UNSIGNED_LONG_LONG,
        // `unsigned long` under LP64, `unsigned long long` under LLP64,
        // `unsigned int` on the 32-bit C libraries which say so.
        "size_t" => cpp::UNSIGNED_INT | cpp::UNSIGNED_LONG | cpp::UNSIGNED_LONG_LONG,
        _ => return None,
    })
}

impl MapSpellings {
    pub(super) fn new<A: AnalysisPhase>(apis: &ApiVec<A>) -> Self {
        Self {
            aliases: alias_targets(apis),
        }
    }

    /// Whether this parameter names one of the two maps anywhere in its own
    /// type: as itself, through references, pointers, `const` and `volatile`
    /// markers, or the aliases spelling any of those.
    ///
    /// This is the widest of the predicates here, deliberately: it decides
    /// which parameters a refusal covers, and a refusal is about the company a
    /// declaration keeps rather than about the map's own shape. A map in a
    /// shape autocxx does not serve is turned down by the type converter
    /// regardless.
    pub(super) fn parameter_mentions_map(&self, arg: &FnArg) -> bool {
        let syn::FnArg::Typed(pt) = &arg.0 else {
            return false;
        };
        self.map_within(&pt.ty, MAX_ALIAS_DEPTH).is_some()
    }

    /// This parameter where it is a *reference* to one of the two maps
    /// itself - lvalue or rvalue, `const` or not, spelled directly or
    /// through aliases: the erased shape of the referent, plus whether the
    /// reference is the kind which binds a const lvalue. A reference
    /// reaching a map only through a pointer or an array is not this shape,
    /// and nor is anything that is not a reference at all.
    ///
    /// Asked by the constructor rule - see the table on
    /// [`super::FnAnalyzer::build_constructor_capture_candidates`] for when
    /// a sibling of this shape can capture a call. The shape is compared
    /// against [`Self::presented_map_argument`] of the constructor being
    /// judged, through [`ErasedMapShape::provably_differs`]: both render
    /// through the same alias expansion, but only a provable difference -
    /// never mere inequality - says the sibling's reference cannot bind
    /// what the judged constructor hands over.
    pub(super) fn map_reference_parameter(&self, arg: &FnArg) -> Option<MapReferenceParameter> {
        let syn::FnArg::Typed(pt) = &arg.0 else {
            return None;
        };
        let resolved = self.resolve_alias(&pt.ty, MAX_ALIAS_DEPTH);
        let Type::Path(tp) = resolved else {
            return None;
        };
        if let Some(ptr) = unwrap_reference(tp, false) {
            return Some(MapReferenceParameter {
                shape: self.erased_map_shape(&ptr.elem)?,
                binds_const_lvalues: !ptr_is_mut(&ptr.mutability),
            });
        }
        let ptr = unwrap_reference(tp, true)?;
        Some(MapReferenceParameter {
            shape: self.erased_map_shape(&ptr.elem)?,
            binds_const_lvalues: false,
        })
    }

    /// The map argument this parameter makes the wrapper present, in the
    /// category which decides what a reference sibling could bind: an
    /// rvalue (taken by value or by rvalue reference, both moved into the
    /// call), a mutable lvalue (taken by non-const lvalue reference), or a
    /// const lvalue (taken by `const&`).
    ///
    /// `None` where no reference binds what is presented: a map behind a
    /// pointer, or no map at all.
    pub(super) fn presented_map_argument(&self, arg: &FnArg) -> Option<PresentedMapArgument> {
        let syn::FnArg::Typed(pt) = &arg.0 else {
            return None;
        };
        let resolved = self.resolve_alias(&pt.ty, MAX_ALIAS_DEPTH);
        let Type::Path(tp) = resolved else {
            // A pointer among other things: no reference binds one.
            return None;
        };
        if let Some(ptr) = unwrap_reference(tp, false) {
            let shape = self.erased_map_shape(&ptr.elem)?;
            return Some(if ptr_is_mut(&ptr.mutability) {
                PresentedMapArgument::RvalueOrMutableLvalue(shape)
            } else {
                PresentedMapArgument::ConstLvalue(shape)
            });
        }
        if let Some(ptr) = unwrap_reference(tp, true) {
            // Moved into the call: an rvalue.
            return Some(PresentedMapArgument::RvalueOrMutableLvalue(
                self.erased_map_shape(&ptr.elem)?,
            ));
        }
        // By value, possibly behind top-level cv markers: the wrapper moves
        // the map it built into the call, an rvalue.
        Some(PresentedMapArgument::RvalueOrMutableLvalue(
            self.erased_map_shape(resolved)?,
        ))
    }

    /// The erased shape of the map `ty` names - `None` where, once aliases
    /// and top-level cv markers are stripped, `ty` is not one of the two
    /// maps itself.
    fn erased_map_shape(&self, ty: &Type) -> Option<ErasedMapShape> {
        let mut ty = ty;
        for _ in 0..MAX_ALIAS_DEPTH {
            let resolved = self.resolve_alias(ty, MAX_ALIAS_DEPTH);
            let Type::Path(tp) = resolved else {
                return None;
            };
            if let Some(inner) = unwrap_const(tp).or_else(|| unwrap_volatile(tp)) {
                ty = inner;
                continue;
            }
            let map = QualifiedName::from_type_path(tp);
            if !known_types().is_map(&map) {
                return None;
            }
            let expanded = self.expand_aliases(resolved, MAX_ALIAS_DEPTH);
            let (key, value) = match &expanded {
                Type::Path(tp) => key_and_value(tp),
                _ => (None, None),
            };
            return Some(ErasedMapShape { map, key, value });
        }
        None
    }

    /// The key under which map-taking declarations of one C++ name are
    /// compared: the overload set's identity - namespace, receiver type and
    /// the name C++ sees - plus [`Self::input_types_key`]. Two declarations
    /// under one key are ones autocxx cannot tell apart in any way at all.
    pub(super) fn map_overload_key(&self, ns: &Namespace, fun: &FuncToConvert) -> String {
        let cpp_name = super::cpp_declared_name(fun);
        let self_ty = fun
            .self_ty
            .as_ref()
            .map(|t| t.to_cpp_name())
            .unwrap_or_default();
        format!(
            "{}|{}|{}|{}",
            ns.iter().collect::<Vec<_>>().join("::"),
            self_ty,
            cpp_name,
            self.input_types_key(fun)
        )
    }

    /// The types of a function's inputs, rendered as one string with the
    /// parameter names left out - what C++ overloads on, and nothing it does
    /// not. Aliases are expanded first, because C++ overloads on what an
    /// alias names rather than on its spelling: two declarations which differ
    /// only in aliases are as indistinguishable as two which do not differ at
    /// all.
    pub(super) fn input_types_key(&self, fun: &FuncToConvert) -> String {
        fun.inputs
            .iter()
            .map(|arg| match &arg.0 {
                syn::FnArg::Typed(pt) => self
                    .expand_aliases(&pt.ty, MAX_ALIAS_DEPTH)
                    .to_token_stream()
                    .to_string(),
                other => other.to_token_stream().to_string(),
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The map `ty` names, through any number of references, pointers,
    /// cv markers and aliases.
    fn map_within(&self, ty: &Type, depth: usize) -> Option<QualifiedName> {
        if depth == 0 {
            return None;
        }
        match ty {
            Type::Path(tp) => {
                if let Some(ptr) =
                    unwrap_reference(tp, false).or_else(|| unwrap_reference(tp, true))
                {
                    return self.map_within(&ptr.elem, depth - 1);
                }
                if let Some(inner) = unwrap_const(tp).or_else(|| unwrap_volatile(tp)) {
                    return self.map_within(inner, depth - 1);
                }
                let name = QualifiedName::from_type_path(tp);
                if known_types().is_map(&name) {
                    return Some(name);
                }
                // An alias followed by name alone: one used with template
                // arguments of its own is a template alias, whose stored
                // target still spells the template's parameters and is not
                // this instantiation.
                if !has_type_arguments(tp) {
                    if let Some(target) = self.aliases.get(&name) {
                        return self.map_within(target, depth - 1);
                    }
                }
                None
            }
            Type::Ptr(ptr) => self.map_within(&ptr.elem, depth - 1),
            Type::Reference(r) => self.map_within(&r.elem, depth - 1),
            Type::Array(arr) => self.map_within(&arr.elem, depth - 1),
            _ => None,
        }
    }

    /// `ty` with a top-level alias chain resolved and nothing else rewritten.
    fn resolve_alias<'a>(&'a self, ty: &'a Type, depth: usize) -> &'a Type {
        if depth == 0 {
            return ty;
        }
        if let Type::Path(tp) = ty {
            if !has_type_arguments(tp) {
                if let Some(target) = self.aliases.get(&QualifiedName::from_type_path(tp)) {
                    return self.resolve_alias(target, depth - 1);
                }
            }
        }
        ty
    }

    /// `ty` with every alias in it replaced by what it names, however deep,
    /// so that two spellings of one C++ type render as one string. A template
    /// alias is left as spelled, as in [`Self::map_within`].
    fn expand_aliases(&self, ty: &Type, depth: usize) -> Type {
        if depth == 0 {
            return ty.clone();
        }
        match ty {
            Type::Path(tp) => {
                if !has_type_arguments(tp) {
                    if let Some(target) = self.aliases.get(&QualifiedName::from_type_path(tp)) {
                        return self.expand_aliases(target, depth - 1);
                    }
                }
                let mut tp = tp.clone();
                for segment in tp.path.segments.iter_mut() {
                    if let PathArguments::AngleBracketed(ab) = &mut segment.arguments {
                        for arg in ab.args.iter_mut() {
                            if let GenericArgument::Type(inner) = arg {
                                *inner = self.expand_aliases(inner, depth - 1);
                            }
                        }
                    }
                }
                Type::Path(tp)
            }
            Type::Ptr(ptr) => {
                let mut ptr = ptr.clone();
                *ptr.elem = self.expand_aliases(&ptr.elem, depth - 1);
                Type::Ptr(ptr)
            }
            Type::Reference(r) => {
                let mut r = r.clone();
                *r.elem = self.expand_aliases(&r.elem, depth - 1);
                Type::Reference(r)
            }
            Type::Array(arr) => {
                let mut arr = arr.clone();
                *arr.elem = self.expand_aliases(&arr.elem, depth - 1);
                Type::Array(arr)
            }
            other => other.clone(),
        }
    }
}

/// The first two template arguments of the path's last segment - the key
/// and the value, on a path already known to be one of the two maps.
fn key_and_value(path: &TypePath) -> (Option<Type>, Option<Type>) {
    let Some(PathArguments::AngleBracketed(ab)) =
        path.path.segments.last().map(|seg| &seg.arguments)
    else {
        return (None, None);
    };
    let mut types = ab.args.iter().filter_map(|arg| match arg {
        GenericArgument::Type(ty) => Some(ty.clone()),
        _ => None,
    });
    (types.next(), types.next())
}

/// Whether the path's last segment carries template arguments of its own.
fn has_type_arguments(path: &TypePath) -> bool {
    path.path
        .segments
        .last()
        .is_some_and(|seg| !matches!(seg.arguments, PathArguments::None))
}
