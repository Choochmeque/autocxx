// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::{HashMap, HashSet};

type Offsets = HashMap<String, usize>;

/// Names state within one scope (a type, or the namespace's free
/// functions): per-name overload counters plus the names already
/// handed out in that scope.
#[derive(Default)]
struct ScopeNames {
    offsets: Offsets,
    assigned: HashSet<String>,
    /// How many declarations of each C++ name have come through, keyed by
    /// the name C++ spells - not the requested Rust name, which a keyword
    /// rename can land on another function's genuine name.
    declarations: Offsets,
}

/// Registry of all the overloads of a function found within a given
/// namespace (i.e. mod in bindgen's output). If necessary we'll append
/// a _nnn suffix to a function's Rust name to disambiguate overloads.
/// Note that this is NOT necessarily the same as the suffix added by
/// bindgen to disambiguate overloads it discovers. Its suffix is
/// global across all functions, whereas ours is local within a given
/// type.
/// If bindgen adds a suffix it will be included in 'found_name'
/// but not 'original_name', which the patch series adds to the
/// vendored bindgen.
///
/// A generated suffix must not collide with the name of a real
/// function elsewhere in the namespace (e.g. overloads of `byteSwap`
/// alongside a real `byteSwap2` - see google/autocxx#1316). Callers
/// therefore [`OverloadTracker::reserve`] every real name up front;
/// suffix generation then skips reserved and already-assigned names.
/// Reservations are scoped exactly like assignment (per type for
/// methods, per namespace for free functions) so a real name on one
/// type cannot perturb overload numbering on an unrelated type.
#[derive(Default)]
pub(crate) struct OverloadTracker {
    fn_names: ScopeNames,
    method_names_by_type: HashMap<String, ScopeNames>,
    reserved_fn_names: HashSet<String>,
    reserved_method_names_by_type: HashMap<String, HashSet<String>>,
}

impl OverloadTracker {
    /// Note a name which some real function will want for itself, so
    /// that no generated overload suffix takes it. Reservations are
    /// scoped exactly like assignment (per type for methods, per
    /// namespace for free functions) so that a real name on one type
    /// cannot perturb overload numbering on an unrelated type.
    ///
    /// Limitation: functions synthesized later in analysis (e.g.
    /// subclass 'foo_super' wrappers) are not reserved here, and a
    /// first occurrence always keeps its requested name, so such
    /// synthetic names can still collide with real ones. That is
    /// pre-existing behavior unrelated to overload suffixes.
    pub(crate) fn reserve(&mut self, type_name: Option<&str>, name: &str) {
        match type_name {
            Some(type_name) => self
                .reserved_method_names_by_type
                .entry(type_name.to_string())
                .or_default()
                .insert(name.to_string()),
            None => self.reserved_fn_names.insert(name.to_string()),
        };
    }

    pub(crate) fn get_function_real_name(
        &mut self,
        found_name: String,
        original_name: Option<&str>,
    ) -> (String, Option<usize>) {
        self.get_name(None, found_name, original_name)
    }

    pub(crate) fn get_method_real_name(
        &mut self,
        type_name: &str,
        found_name: String,
        original_name: Option<&str>,
    ) -> (String, Option<usize>) {
        self.get_name(Some(type_name), found_name, original_name)
    }

    /// The Rust name this function gets, and - where that is not the name it
    /// asked for - how many declarations of the same C++ name came through
    /// this scope first. That count is what tells a reader of the generated
    /// code which C++ declaration a numbered name came from, and is not
    /// recoverable from the suffix: the suffix skips numbers taken by real
    /// functions.
    ///
    /// The count is keyed by `original_name` - what C++ calls the function -
    /// not by the requested Rust name: `type`'s keyword rename `type_` beside
    /// a genuine `type_` collides in Rust while C++ declares each name once,
    /// so neither is an overload of anything. `None` marks a function with no
    /// C++ declaration behind it (synthesized, or standing for a whole
    /// overload set), which must not shift any declaration's count.
    fn get_name(
        &mut self,
        type_name: Option<&str>,
        cpp_method_name: String,
        original_name: Option<&str>,
    ) -> (String, Option<usize>) {
        let Self {
            fn_names,
            method_names_by_type,
            reserved_fn_names,
            reserved_method_names_by_type,
        } = self;
        static EMPTY: once_cell::sync::Lazy<HashSet<String>> =
            once_cell::sync::Lazy::new(HashSet::new);
        let (scope, reserved_names) = match type_name {
            Some(type_name) => (
                method_names_by_type
                    .entry(type_name.to_string())
                    .or_default(),
                reserved_method_names_by_type
                    .get(type_name)
                    .unwrap_or(&EMPTY),
            ),
            None => (fn_names, &*reserved_fn_names),
        };
        let declaration_ordinal = original_name.map(|original| {
            let count = scope.declarations.entry(original.to_string()).or_default();
            let prior = *count;
            *count += 1;
            prior
        });
        let offset = scope.offsets.entry(cpp_method_name.clone()).or_default();
        let this_offset = *offset;
        *offset += 1;
        if this_offset == 0 && !scope.assigned.contains(&cpp_method_name) {
            // The first occurrence keeps the real name - unless some other
            // name's suffix already landed on it, which `foo` and `foo1`
            // between them can do. Handing it out twice would generate two
            // Rust items of the same name.
            scope.assigned.insert(cpp_method_name.clone());
            return (cpp_method_name, None);
        }
        let mut n = this_offset.max(1);
        loop {
            let candidate = format!("{cpp_method_name}{n}");
            if !reserved_names.contains(&candidate) && !scope.assigned.contains(&candidate) {
                scope.assigned.insert(candidate.clone());
                return (candidate, declaration_ordinal);
            }
            n += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OverloadTracker;

    /// The name assigned, plus the ordinal reported alongside it: `None` where
    /// the function kept the name it asked for, and otherwise how many
    /// declarations of the same C++ name came through this scope first. Here
    /// the C++ name is the requested name, as it is wherever no keyword rename
    /// is in play.
    fn fun(ot: &mut OverloadTracker, name: &str) -> (String, Option<usize>) {
        ot.get_function_real_name(name.into(), Some(name))
    }

    /// A function whose requested Rust name is not what C++ calls it - a
    /// keyword rename, `type` asking for `type_`.
    fn fun_renamed(
        ot: &mut OverloadTracker,
        requested: &str,
        cpp: &str,
    ) -> (String, Option<usize>) {
        ot.get_function_real_name(requested.into(), Some(cpp))
    }

    fn method(ot: &mut OverloadTracker, ty: &str, name: &str) -> (String, Option<usize>) {
        ot.get_method_real_name(ty, name.into(), Some(name))
    }

    #[test]
    fn test_by_function() {
        let mut ot = OverloadTracker::default();
        assert_eq!(fun(&mut ot, "bob"), ("bob".into(), None));
        assert_eq!(fun(&mut ot, "bob"), ("bob1".into(), Some(1)));
        assert_eq!(fun(&mut ot, "bob"), ("bob2".into(), Some(2)));
    }

    #[test]
    fn test_by_method() {
        let mut ot = OverloadTracker::default();
        assert_eq!(method(&mut ot, "Ty1", "bob"), ("bob".into(), None));
        assert_eq!(method(&mut ot, "Ty1", "bob"), ("bob1".into(), Some(1)));
        assert_eq!(method(&mut ot, "Ty2", "bob"), ("bob".into(), None));
        assert_eq!(method(&mut ot, "Ty2", "bob"), ("bob1".into(), Some(1)));
    }

    #[test]
    fn test_suffix_avoids_reserved_real_name() {
        // google/autocxx#1316: overload suffix must not take the name
        // of a real function, regardless of processing order.
        let mut ot = OverloadTracker::default();
        ot.reserve(Some("Ty"), "bob");
        ot.reserve(Some("Ty"), "bob2");
        assert_eq!(method(&mut ot, "Ty", "bob"), ("bob".into(), None));
        assert_eq!(method(&mut ot, "Ty", "bob"), ("bob1".into(), Some(1)));
        // The suffix skipped a number, which is exactly why the ordinal is
        // reported rather than read back off the name.
        assert_eq!(method(&mut ot, "Ty", "bob"), ("bob3".into(), Some(2)));
        assert_eq!(method(&mut ot, "Ty", "bob2"), ("bob2".into(), None));
    }

    #[test]
    fn test_suffix_skips_reserved_chain() {
        let mut ot = OverloadTracker::default();
        ot.reserve(None, "g");
        ot.reserve(None, "g1");
        ot.reserve(None, "g2");
        assert_eq!(fun(&mut ot, "g"), ("g".into(), None));
        assert_eq!(fun(&mut ot, "g"), ("g3".into(), Some(1)));
        assert_eq!(fun(&mut ot, "g"), ("g4".into(), Some(2)));
        assert_eq!(fun(&mut ot, "g1"), ("g1".into(), None));
        assert_eq!(fun(&mut ot, "g2"), ("g2".into(), None));
    }

    #[test]
    fn test_real_name_keeps_name_even_when_reserved() {
        // Reservation of a function's own name must not affect its
        // first occurrence.
        let mut ot = OverloadTracker::default();
        ot.reserve(None, "solo");
        assert_eq!(fun(&mut ot, "solo"), ("solo".into(), None));
    }

    #[test]
    fn test_reserved_overloaded_real_name_with_own_overloads() {
        // f has overloads; f1 is real and itself overloaded.
        let mut ot = OverloadTracker::default();
        ot.reserve(None, "f");
        ot.reserve(None, "f1");
        assert_eq!(fun(&mut ot, "f"), ("f".into(), None));
        assert_eq!(fun(&mut ot, "f"), ("f2".into(), Some(1)));
        assert_eq!(fun(&mut ot, "f1"), ("f1".into(), None));
        // f1's own overload takes f11; that's free.
        assert_eq!(fun(&mut ot, "f1"), ("f11".into(), Some(1)));
    }

    #[test]
    fn test_reservations_scoped_per_type() {
        // A reservation on Ty2 must not perturb Ty1's numbering
        // (backward compatibility for unrelated types), but must be
        // honoured within Ty2 itself.
        let mut ot = OverloadTracker::default();
        ot.reserve(Some("Ty2"), "bob1");
        assert_eq!(method(&mut ot, "Ty1", "bob"), ("bob".into(), None));
        assert_eq!(method(&mut ot, "Ty1", "bob"), ("bob1".into(), Some(1)));
        assert_eq!(method(&mut ot, "Ty2", "bob"), ("bob".into(), None));
        assert_eq!(method(&mut ot, "Ty2", "bob"), ("bob2".into(), Some(1)));
    }

    #[test]
    fn test_free_fn_reservation_does_not_affect_methods() {
        let mut ot = OverloadTracker::default();
        ot.reserve(None, "bob1");
        assert_eq!(method(&mut ot, "Ty", "bob"), ("bob".into(), None));
        assert_eq!(method(&mut ot, "Ty", "bob"), ("bob1".into(), Some(1)));
    }

    #[test]
    fn test_a_name_taken_by_another_names_suffix_reports_no_earlier_overload() {
        // `foo1` arriving after `foo`'s suffix already took that name is the
        // first `foo1`, however it ends up spelled, so there is no overload
        // ordinal to report for it.
        let mut ot = OverloadTracker::default();
        assert_eq!(fun(&mut ot, "foo"), ("foo".into(), None));
        assert_eq!(fun(&mut ot, "foo"), ("foo1".into(), Some(1)));
        assert_eq!(fun(&mut ot, "foo1"), ("foo11".into(), Some(0)));
    }

    #[test]
    fn test_ordinal_counts_cpp_declarations_not_rust_requests() {
        // `void type_(); void type(int);` - the keyword rename of `type`
        // requests `type_` and collides with the genuine `type_`, but C++
        // declares each name once, so neither has an earlier declaration
        // to count.
        let mut ot = OverloadTracker::default();
        assert_eq!(
            fun_renamed(&mut ot, "type_", "type_"),
            ("type_".into(), None)
        );
        assert_eq!(
            fun_renamed(&mut ot, "type_", "type"),
            ("type_1".into(), Some(0))
        );
    }

    #[test]
    fn test_ordinal_counts_every_declaration_of_the_cpp_name() {
        // `void type(); void type(int);` - two declarations of `type`, both
        // requesting the keyword rename `type_`.
        let mut ot = OverloadTracker::default();
        assert_eq!(
            fun_renamed(&mut ot, "type_", "type"),
            ("type_".into(), None)
        );
        assert_eq!(
            fun_renamed(&mut ot, "type_", "type"),
            ("type_1".into(), Some(1))
        );
    }

    #[test]
    fn test_no_original_name_reports_and_shifts_no_ordinal() {
        // A synthesized function has no C++ declaration behind it: it takes a
        // numbered name but neither reports a position nor shifts the count
        // of a real declaration arriving later.
        let mut ot = OverloadTracker::default();
        assert_eq!(
            ot.get_function_real_name("f".into(), None),
            ("f".into(), None)
        );
        assert_eq!(
            ot.get_function_real_name("f".into(), None),
            ("f1".into(), None)
        );
        assert_eq!(
            ot.get_function_real_name("f".into(), Some("f")),
            ("f2".into(), Some(0))
        );
    }
}
