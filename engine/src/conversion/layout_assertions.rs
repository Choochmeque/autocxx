// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Which types get a size and alignment assertion written for them, on both
//! sides of the bridge.

use indexmap::map::IndexMap;
use indexmap::set::IndexSet;

use crate::conversion::{
    analysis::{
        fun::{FnPhase, PodAndDepAnalysis},
        pod::PodAnalysis,
        tdef::{resolve_typedefs, typedef_targets},
    },
    api::{Api, TypeKind},
    apivec::ApiVec,
};
use crate::known_types::known_types;
use crate::types::QualifiedName;

/// What is to be asserted about one type's layout.
///
/// The numbers are clang's, read when it parsed the header. Rust owns storage
/// of that size for the C++ object - `within_box`, `within_cpp_pin` and stack
/// emplacement each allocate the Rust type's size and then ask C++ to
/// construct one there - so a disagreement is a write past the end of that
/// storage rather than a wrong answer. Everything the two renderings pass
/// through between clang's measurement and the emitted type is checked by
/// holding each of them to that measurement.
pub(crate) struct LayoutAssertion {
    /// The size in bytes, which is what C++'s `sizeof` must answer and what
    /// the generated Rust type must come to.
    pub(crate) size: usize,
    /// The alignment in bytes, which is what C++'s `alignof` must answer.
    pub(crate) align: usize,
    /// Whether the Rust half is to be written. See [`layout_assertions`].
    pub(crate) assert_rust: bool,
    /// Whether the C++ half is to be written. See [`layout_assertions`].
    pub(crate) assert_cpp: bool,
}

/// The types to write a layout assertion for, and which halves each gets.
///
/// Read by both code generators, so that the two halves are decided in one
/// place: the C++ half alone would say nothing about the Rust rendering, which
/// is the half a layout mistake lands in, and the Rust half alone would not
/// notice the header being compiled under options, a standard library or an
/// ABI clang was not given.
///
/// Nothing at all is written for anything but a struct bindgen laid out. A
/// forward declaration and a class template's own pattern reach codegen with
/// no layout, because clang measured none. An `Api::ConcreteType` - a template
/// instantiation autocxx invented, or a subclass's C++ peer class - is not a
/// bindgen struct at all and nothing ever measured it, and neither is an
/// `extern_cpp_type!`, whose Rust side is the user's own type. A class
/// template is left out by `num_generics` as well as by the missing layout,
/// because the Rust side of one is not a cxx type either.
///
/// The C++ half is withheld from a type in an anonymous namespace, which C++
/// has no spelling for: bindgen invents a name for the namespace
/// (`_bindgen_mod_id_47`) and generated C++ using it does not compile. Every
/// other place autocxx would name one is refused for the same reason - see
/// `ConvertErrorFromCpp::MethodInAnonymousNamespace`.
///
/// The Rust half is written only for the types in
/// [`names_with_a_reliable_rust_size`].
///
/// The types autocxx hands bindgen a `replaces=` substitute for need no
/// exception: `parse_bindgen` drops the substitute itself, and a class holding
/// one is padded out to the size clang reported for the class, so the
/// assertion is the check that the padding was computed right rather than a
/// complaint that a stand-in is a stand-in.
pub(crate) fn layout_assertions(
    apis: &ApiVec<FnPhase>,
) -> IndexMap<QualifiedName, LayoutAssertion> {
    let reliable = names_with_a_reliable_rust_size(apis);
    apis.iter()
        .filter_map(|api| match api {
            Api::Struct {
                name,
                details,
                analysis:
                    PodAndDepAnalysis {
                        pod:
                            PodAnalysis {
                                num_generics: 0,
                                in_anonymous_namespace,
                                ..
                            },
                        ..
                    },
            } => details.layout.map(|layout| {
                (
                    name.name.clone(),
                    LayoutAssertion {
                        size: layout.size,
                        align: layout.align,
                        assert_rust: reliable.contains(&name.name),
                        assert_cpp: !in_anonymous_namespace,
                    },
                )
            }),
            _ => None,
        })
        .collect()
}

/// The types whose Rust rendering can be held to the size clang measured for
/// the C++ one, reached by closure over what each holds by value.
///
/// A struct qualifies when every type it holds - a field's, or a base class's -
/// is one bindgen rendered faithfully. Three things count as that: a type
/// `known_types` provided the rendering for, whose stand-in is padded out to
/// the C++ type's size wherever it is held; an enumeration, which is the
/// integer clang gave it; and another struct which itself qualifies. Each name
/// is resolved through any typedefs first, because bindgen reports a base
/// class by the name the derived class was written with and a `using` alias
/// there is that name.
///
/// Everything else disqualifies the struct holding it, and there is one reason
/// for all of them: bindgen wrote a field of a type which is not the C++
/// member, and nothing made up the difference. That happens where autocxx
/// generated nothing for the member's type - it was never allowlisted, or it
/// was dropped for a name collision - and where the Rust side is cxx's opaque
/// stand-in, which is zero-sized on purpose: an abstract class, a template
/// instantiation autocxx invented a holder for, a forward declaration, an
/// opaque typedef, an `extern_cpp_type!`, a subclass's C++ peer class. The
/// enclosing class is then short of the object C++ builds in it, which is a
/// defect of its own: the field wants writing as a blob of the layout clang
/// measured, a change to what bindgen emits rather than to how it is measured.
/// Until that lands there is no size for the Rust half to be held to, and
/// asserting one would fail on every such class.
///
/// A struct whose fields are not all converted disqualifies itself, whatever
/// the ones which were converted turned out to be. A field autocxx could not
/// convert reaches none of the sets read here, so a class holding one is
/// indistinguishable from a class not holding it - and what bindgen wrote for
/// that field is exactly the kind of stand-in the paragraph above is about.
/// `std::unordered_map<std::string, uint32_t>` is one on the MSVC standard
/// library, where bindgen writes fewer template parameters than C++ declared
/// and autocxx will not name a specialization from what is left.
fn names_with_a_reliable_rust_size(apis: &ApiVec<FnPhase>) -> IndexSet<QualifiedName> {
    let targets = typedef_targets(apis);
    let enums: IndexSet<QualifiedName> = apis
        .iter()
        .filter(|api| matches!(api, Api::Enum { .. }))
        .map(|api| api.name().clone())
        .collect();
    let mut reliable = IndexSet::new();
    // Each pass can only add, and only from a finite set of names, so this
    // terminates however the types refer to one another. What bounds the
    // number of passes is the depth of by-value nesting, not the number of
    // types: the garbage collector emits a type before the types it holds, so
    // nothing here can rely on an order which settles a chain in one pass. A
    // pass is a scan of every API either way.
    loop {
        let mut grew = false;
        for api in apis.iter() {
            let Api::Struct {
                name,
                analysis:
                    PodAndDepAnalysis {
                        pod:
                            PodAnalysis {
                                kind,
                                num_generics: 0,
                                all_fields_converted: true,
                                field_definition_deps,
                                bases,
                                ..
                            },
                        ..
                    },
                ..
            } = api
            else {
                continue;
            };
            if matches!(kind, TypeKind::Abstract) || reliable.contains(&name.name) {
                continue;
            }
            if field_definition_deps
                .iter()
                .chain(bases.iter())
                .map(|held| resolve_typedefs(&targets, held))
                .all(|held| {
                    known_types().is_known_type(&held)
                        || enums.contains(&held)
                        || reliable.contains(&held)
                })
            {
                reliable.insert(name.name.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    reliable
}
