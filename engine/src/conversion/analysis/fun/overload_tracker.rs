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
}

/// Registry of all the overloads of a function found within a given
/// namespace (i.e. mod in bindgen's output). If necessary we'll append
/// a _nnn suffix to a function's Rust name to disambiguate overloads.
/// Note that this is NOT necessarily the same as the suffix added by
/// bindgen to disambiguate overloads it discovers. Its suffix is
/// global across all functions, whereas ours is local within a given
/// type.
/// If bindgen adds a suffix it will be included in 'found_name'
/// but not 'original_name' which is an annotation added by our autocxx-bindgen
/// fork.
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

    pub(crate) fn get_function_real_name(&mut self, found_name: String) -> String {
        self.get_name(None, found_name)
    }

    pub(crate) fn get_method_real_name(&mut self, type_name: &str, found_name: String) -> String {
        self.get_name(Some(type_name), found_name)
    }

    fn get_name(&mut self, type_name: Option<&str>, cpp_method_name: String) -> String {
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
        let offset = scope.offsets.entry(cpp_method_name.clone()).or_default();
        let this_offset = *offset;
        *offset += 1;
        if this_offset == 0 {
            // The first occurrence keeps the real name.
            scope.assigned.insert(cpp_method_name.clone());
            cpp_method_name
        } else {
            let mut n = this_offset;
            loop {
                let candidate = format!("{cpp_method_name}{n}");
                if !reserved_names.contains(&candidate) && !scope.assigned.contains(&candidate) {
                    scope.assigned.insert(candidate.clone());
                    return candidate;
                }
                n += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OverloadTracker;

    #[test]
    fn test_by_function() {
        let mut ot = OverloadTracker::default();
        assert_eq!(ot.get_function_real_name("bob".into()), "bob");
        assert_eq!(ot.get_function_real_name("bob".into()), "bob1");
        assert_eq!(ot.get_function_real_name("bob".into()), "bob2");
    }

    #[test]
    fn test_by_method() {
        let mut ot = OverloadTracker::default();
        assert_eq!(ot.get_method_real_name("Ty1", "bob".into()), "bob");
        assert_eq!(ot.get_method_real_name("Ty1", "bob".into()), "bob1");
        assert_eq!(ot.get_method_real_name("Ty2", "bob".into()), "bob");
        assert_eq!(ot.get_method_real_name("Ty2", "bob".into()), "bob1");
    }

    #[test]
    fn test_suffix_avoids_reserved_real_name() {
        // google/autocxx#1316: overload suffix must not take the name
        // of a real function, regardless of processing order.
        let mut ot = OverloadTracker::default();
        ot.reserve(Some("Ty"), "bob");
        ot.reserve(Some("Ty"), "bob2");
        assert_eq!(ot.get_method_real_name("Ty", "bob".into()), "bob");
        assert_eq!(ot.get_method_real_name("Ty", "bob".into()), "bob1");
        assert_eq!(ot.get_method_real_name("Ty", "bob".into()), "bob3");
        assert_eq!(ot.get_method_real_name("Ty", "bob2".into()), "bob2");
    }

    #[test]
    fn test_suffix_skips_reserved_chain() {
        let mut ot = OverloadTracker::default();
        ot.reserve(None, "g");
        ot.reserve(None, "g1");
        ot.reserve(None, "g2");
        assert_eq!(ot.get_function_real_name("g".into()), "g");
        assert_eq!(ot.get_function_real_name("g".into()), "g3");
        assert_eq!(ot.get_function_real_name("g".into()), "g4");
        assert_eq!(ot.get_function_real_name("g1".into()), "g1");
        assert_eq!(ot.get_function_real_name("g2".into()), "g2");
    }

    #[test]
    fn test_real_name_keeps_name_even_when_reserved() {
        // Reservation of a function's own name must not affect its
        // first occurrence.
        let mut ot = OverloadTracker::default();
        ot.reserve(None, "solo");
        assert_eq!(ot.get_function_real_name("solo".into()), "solo");
    }

    #[test]
    fn test_reserved_overloaded_real_name_with_own_overloads() {
        // f has overloads; f1 is real and itself overloaded.
        let mut ot = OverloadTracker::default();
        ot.reserve(None, "f");
        ot.reserve(None, "f1");
        assert_eq!(ot.get_function_real_name("f".into()), "f");
        assert_eq!(ot.get_function_real_name("f".into()), "f2");
        assert_eq!(ot.get_function_real_name("f1".into()), "f1");
        // f1's own overload takes f11; that's free.
        assert_eq!(ot.get_function_real_name("f1".into()), "f11");
    }

    #[test]
    fn test_reservations_scoped_per_type() {
        // A reservation on Ty2 must not perturb Ty1's numbering
        // (backward compatibility for unrelated types), but must be
        // honoured within Ty2 itself.
        let mut ot = OverloadTracker::default();
        ot.reserve(Some("Ty2"), "bob1");
        assert_eq!(ot.get_method_real_name("Ty1", "bob".into()), "bob");
        assert_eq!(ot.get_method_real_name("Ty1", "bob".into()), "bob1");
        assert_eq!(ot.get_method_real_name("Ty2", "bob".into()), "bob");
        assert_eq!(ot.get_method_real_name("Ty2", "bob".into()), "bob2");
    }

    #[test]
    fn test_free_fn_reservation_does_not_affect_methods() {
        let mut ot = OverloadTracker::default();
        ot.reserve(None, "bob1");
        assert_eq!(ot.get_method_real_name("Ty", "bob".into()), "bob");
        assert_eq!(ot.get_method_real_name("Ty", "bob".into()), "bob1");
    }
}
