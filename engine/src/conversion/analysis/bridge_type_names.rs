// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;
use itertools::Itertools;

use crate::conversion::{api::Api, apivec::ApiVec};
use crate::minisyn::Ident;
use crate::types::{make_ident, validate_ident_ok_for_cxx, InvalidIdentError, QualifiedName};

use super::fun::FnPhase;

/// The name each type goes by inside the `cxx::bridge` mod.
///
/// That mod has a flat namespace, so two C++ types which differ only in their
/// namespace - `A::Bob` and `B::Bob` - cannot both be called `Bob` there.
/// Functions have always dodged this with
/// [`crate::conversion::analysis::fun::bridge_name_tracker::BridgeNameTracker`];
/// this is the same idea for types, and is google/autocxx#486.
///
/// A renamed type is still spelled correctly for C++, because the `#[namespace]`
/// and `#[cxx_name]` attributes we already emit say what it really is, and its
/// Rust-facing name is unaffected: the name it is renamed to appears only
/// inside the bridge mod, which is private, and the output mod re-exports or
/// defines the type under its own name in its own namespace mod.
///
/// Only the bridge's own view of a type is renamed. The type's
/// [`QualifiedName`] - the identity everything else in the engine refers to it
/// by, including dependency edges and `type_id!` strings - never changes.
pub(crate) struct BridgeTypeNames(HashMap<QualifiedName, Ident>);

impl BridgeTypeNames {
    /// Allocate a bridge name for every type which will be declared in the
    /// bridge, avoiding the names already spoken for by functions and by the
    /// declarations we are not free to rename.
    ///
    /// Also answers the types we could not find a legal bridge name for, so
    /// that they can be rejected with the same diagnostic a badly-named type
    /// gets rather than reaching `cxx`.
    pub(crate) fn new(apis: &ApiVec<FnPhase>) -> (Self, HashMap<QualifiedName, InvalidIdentError>) {
        let mut taken: HashSet<String> = apis.iter().flat_map(fixed_bridge_names).collect();
        let mut names = HashMap::new();
        let mut unnameable = HashMap::new();
        for api in apis.iter().filter(|api| declares_bridge_type(api)) {
            let name = api.name();
            match allocate(name, &mut taken) {
                Ok(chosen) => {
                    names.insert(name.clone(), make_ident(&chosen));
                }
                Err(e) => {
                    unnameable.insert(name.clone(), e);
                }
            }
        }
        (Self(names), unnameable)
    }

    /// The name this type goes by in the bridge. Types we never allocated for
    /// aren't declared in the bridge at all, so nothing can refer to them
    /// there; answer their own final identifier, which is what the rest of the
    /// engine would have used anyway.
    pub(crate) fn get(&self, name: &QualifiedName) -> Ident {
        self.0
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.get_final_ident())
    }
}

/// Whether this API declares a type in the bridge whose name we are free to
/// choose. The other bridge declarations - functions, `autocxx::c_int` and
/// friends, `extern "Rust"` types, subclass machinery - are named by something
/// outside our control, so they get to keep their names and everything else
/// works around them.
pub(super) fn declares_bridge_type(api: &Api<FnPhase>) -> bool {
    matches!(
        api,
        Api::Struct { .. }
            | Api::Enum { .. }
            | Api::ForwardDeclaration { .. }
            | Api::OpaqueTypedef { .. }
            | Api::ConcreteType { .. }
            | Api::ExternCppType { .. }
    )
}

/// The bridge names this API brings with it, which we cannot reassign.
///
/// The `extern "Rust"` half of the bridge lives in the same mod as the
/// `extern "C++"` half, so what it declares is listed here too. Its functions
/// could not in fact take a name away from a type - Rust keeps types and
/// values in separate namespaces, and
/// `test_extern_rust_fn_name_is_not_reused_for_a_type` pins that a header
/// relying on it still builds - but reserving them costs only a slightly
/// longer name in a rare case, and saves this from depending on that.
pub(super) fn fixed_bridge_names(api: &Api<FnPhase>) -> Vec<String> {
    match api {
        Api::Function { analysis, .. } => vec![analysis.cxxbridge_name.to_string()],
        Api::CType { name, .. }
        | Api::RustType { name, .. }
        | Api::RustFn { name, .. }
        | Api::RustSubclassFn { name, .. }
        | Api::StringConstructor { name } => {
            vec![name.name.get_final_item().to_string()]
        }
        // A subclass turns into three bridge declarations, all named after
        // the subclass the user asked for.
        Api::Subclass { name, .. } => vec![
            name.0.name.get_final_item().to_string(),
            name.holder().to_string(),
            name.cpp().get_final_item().to_string(),
        ],
        // Everything else declares nothing in the bridge at all - constants,
        // variables and typedefs are re-exported straight out of the bindgen
        // mod, and a rejected item leaves only a documentation stub - so it
        // holds no name there, and neither reserves one nor collides.
        _ => Vec::new(),
    }
}

/// Pick the least surprising unused name for this type, in the same spirit as
/// `BridgeNameTracker` does for functions: its own name if that is free, then
/// its name prefixed with its namespace, then a numbered variant.
///
/// A name we synthesize has to clear the same bar as one the user wrote -
/// `cxx` will not take an identifier with two adjacent underscores in it, and
/// joining segments is an easy way to produce one - so every candidate but the
/// first is squashed and checked. The first needs neither: it is the type's own
/// name, which `check_names` has already validated by the time we run.
fn allocate(
    name: &QualifiedName,
    taken: &mut HashSet<String>,
) -> Result<String, InvalidIdentError> {
    let preferred = name.get_final_item().to_string();
    if taken.insert(preferred.clone()) {
        return Ok(preferred);
    }
    let qualified = collapse_underscore_runs(
        &name
            .ns_segment_iter()
            .chain(std::iter::once(name.get_final_item()))
            .join("_"),
    );
    // Number whichever of the two is a legal identifier. The type's own name
    // always is, so there is always something to fall back on.
    let stem = if validate_ident_ok_for_cxx(&qualified).is_ok() {
        if taken.insert(qualified.clone()) {
            return Ok(qualified);
        }
        qualified
    } else {
        preferred
    };
    for counter in 1.. {
        let candidate = collapse_underscore_runs(&format!("{stem}_autocxx{counter}"));
        validate_ident_ok_for_cxx(&candidate)?;
        if taken.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    unreachable!("an unbounded sequence of candidate names cannot run out")
}

/// Reduce every run of underscores to one. `cxx` rejects an identifier
/// containing two in a row, and we can easily make one by joining a namespace
/// which ends in an underscore to a type which starts with one - a shape C++
/// is perfectly happy with.
fn collapse_underscore_runs(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for c in id.chars() {
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::allocate;
    use crate::types::QualifiedName;
    use indexmap::set::IndexSet as HashSet;

    fn allocate_all(names: &[&str], already_taken: &[&str]) -> Vec<String> {
        let mut taken: HashSet<String> =
            already_taken.iter().map(|name| name.to_string()).collect();
        names
            .iter()
            .map(|name| {
                allocate(&QualifiedName::new_from_cpp_name(name), &mut taken)
                    .expect("could not name this type")
            })
            .collect()
    }

    /// The first type to ask for a name keeps its own; whoever comes next has
    /// to be told apart from it, and the namespace is the least surprising way
    /// to do that.
    #[test]
    fn second_type_of_a_name_is_qualified_by_its_namespace() {
        assert_eq!(
            allocate_all(&["A::Bob", "B::Bob", "Bob"], &[]),
            vec!["Bob", "B_Bob", "Bob_autocxx1"]
        );
    }

    /// Where even the qualified name is spoken for - by a function, or by a
    /// type literally called `B_Bob` - we fall back on numbering.
    #[test]
    fn numbering_breaks_a_tie_the_namespace_cannot() {
        assert_eq!(
            allocate_all(&["A::Bob", "B::Bob"], &["Bob", "B_Bob"]),
            vec!["A_Bob", "B_Bob_autocxx1"]
        );
    }

    /// A type whose name nothing else wants is left alone, which is what keeps
    /// the bridge readable in the overwhelmingly common case.
    #[test]
    fn unambiguous_names_are_untouched() {
        assert_eq!(
            allocate_all(&["A::Bob", "B::Fred"], &[]),
            vec!["Bob", "Fred"]
        );
    }

    /// A name we build has to be one cxx will take, and joining segments is an
    /// easy way to produce two adjacent underscores, which it will not. C++ is
    /// happy with the header this comes from, so refusing the type would be a
    /// poor answer; squashing the run is enough.
    #[test]
    fn synthesized_names_do_not_acquire_double_underscores() {
        assert_eq!(
            allocate_all(&["Bob", "a_::Bob", "_Bob", "b::_Bob"], &[]),
            vec!["Bob", "a_Bob", "_Bob", "b_Bob"]
        );
        // Numbering appends to the stem, so it can produce a run of its own.
        assert_eq!(allocate_all(&["a_", "b::a_"], &[]), vec!["a_", "b_a_"]);
        assert_eq!(allocate_all(&["a_", "a_"], &[]), vec!["a_", "a_autocxx1"]);
    }

    /// Whoever holds a name first keeps it, whether that is another type or
    /// something in the `extern "Rust"` half of the bridge.
    #[test]
    fn rust_side_names_are_respected() {
        assert_eq!(
            allocate_all(&["Bob"], &["Bob", "Bob_autocxx1"]),
            vec!["Bob_autocxx2"]
        );
    }
}
