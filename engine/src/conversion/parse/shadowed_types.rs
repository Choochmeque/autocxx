// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::set::IndexSet as HashSet;
use syn::{ForeignItem, Item};

use crate::types::{strip_bindgen_original_suffix_from_ident, Namespace, QualifiedName};

/// Find the names of types which C++ will refuse to look up by their plain
/// name, because something else of the same name is declared in the same scope.
///
/// This is legal, and common, C++:
///
/// ```cpp
/// struct stat { ... };
/// extern struct stat stat;
/// ```
///
/// The variable hides the type, so from that point on `stat` alone names the
/// variable and only the elaborated form `struct stat` names the type. Any
/// C++ we generate which spells such a type by its plain name will fail to
/// compile with "must use 'struct' tag to refer to type 'stat' in this scope".
/// See [`crate::conversion::codegen_cpp::type_to_cpp::CppNameMap`] for what we
/// do about it.
///
/// A function hides a type in exactly the same way and for the same reason -
/// [basic.scope.hiding] lets "the name of a variable, data member, function, or
/// enumerator" hide a class or enumeration name - so
///
/// ```cpp
/// struct foo { ... };
/// void foo();
/// ```
///
/// needs the same treatment.
///
/// We spot the hiding declaration in `bindgen`'s output, where it appears
/// alongside the `struct` of the same name: a variable as a `static` in an
/// `extern "C"` block, a function as a `fn` in one. The rest of the engine
/// never sees either of them, because [`crate::conversion::apivec::ApiVec`]
/// requires each API to have a unique name and resolves the clash in favour of
/// the type. That's why this scan runs over the raw `bindgen` output rather
/// than over `Api`s.
///
/// A name recorded here which turns out not to belong to a type is simply
/// never looked up: the alias map is built by intersecting this set with the
/// types we actually found.
pub(crate) fn find_shadowed_types(items: &[Item]) -> HashSet<QualifiedName> {
    let mut shadowed = HashSet::new();
    // With namespaces enabled, bindgen puts everything inside a mod called
    // 'root', which corresponds to the global namespace.
    for item in items {
        if let Item::Mod(root_mod) = item {
            if let Some((_, items)) = &root_mod.content {
                scan_mod(items, &Namespace::new(), &mut shadowed);
            }
        }
    }
    shadowed
}

fn scan_mod(items: &[Item], ns: &Namespace, shadowed: &mut HashSet<QualifiedName>) {
    for item in items {
        match item {
            Item::ForeignMod(fm) => {
                for foreign_item in &fm.items {
                    match foreign_item {
                        ForeignItem::Static(s) => {
                            shadowed.insert(QualifiedName::new(ns, s.ident.clone().into()));
                        }
                        ForeignItem::Fn(f) => {
                            // `bindgen` renames the functions it declares so
                            // that we can generate wrappers of our own beside
                            // them; the C++ name is what does the hiding.
                            // Methods arrive here too, but with their class
                            // name flattened into them (`Bob_get`), so they
                            // can't be confused with a namespace-scope type.
                            shadowed.insert(QualifiedName::new(
                                ns,
                                strip_bindgen_original_suffix_from_ident(&f.sig.ident).into(),
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Item::Mod(itm) => {
                if let Some((_, items)) = &itm.content {
                    scan_mod(items, &ns.push(itm.ident.to_string()), shadowed);
                }
            }
            _ => {}
        }
    }
}
