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

use crate::conversion::{
    analysis::{
        fun::{FnPhase, PodAndDepAnalysis},
        pod::PodAnalysis,
    },
    api::{Api, TypeKind},
    apivec::ApiVec,
};
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
/// The Rust half is withheld from an abstract class, whose Rust side is a cxx
/// `type T;` and holds no storage at all: autocxx never lets one be
/// constructed in Rust memory, so there is nothing for the C++ size to be the
/// size of. An abstract class nested in another class is written the way every
/// other opaque type is - a wrapper round the bindgen struct - but the two are
/// not told apart here.
///
/// Nothing else is left out. Every other type autocxx generates is either the
/// bindgen struct or a `repr(transparent)` wrapper round it, so the C++ size is
/// the size the Rust type has to come to, whatever autocxx made of the fields:
/// a member bindgen wrote a narrower type for than the one C++ declared is
/// padded out to the layout clang measured where it is written, so the
/// assertion checks that the padding was computed right rather than complaining
/// that a stand-in is a stand-in. The types autocxx hands bindgen a `replaces=`
/// substitute for are the same story - `parse_bindgen` drops the substitute
/// itself and the class holding one is padded out around it.
pub(crate) fn layout_assertions(
    apis: &ApiVec<FnPhase>,
) -> IndexMap<QualifiedName, LayoutAssertion> {
    apis.iter()
        .filter_map(|api| match api {
            Api::Struct {
                name,
                details,
                analysis:
                    PodAndDepAnalysis {
                        pod:
                            PodAnalysis {
                                kind,
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
                        assert_rust: !matches!(kind, TypeKind::Abstract),
                        assert_cpp: !in_anonymous_namespace,
                    },
                )
            }),
            _ => None,
        })
        .collect()
}
