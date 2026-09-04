// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::set::IndexSet as HashSet;
use syn::{ForeignItem, Item};

use crate::types::{Namespace, QualifiedName};

/// Find the names of types which C++ will refuse to look up by their plain
/// name, because a variable of the same name is declared in the same scope.
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
/// We spot the shadowing declaration in `bindgen`'s output, where it appears as
/// a `static` in an `extern "C"` block alongside the `struct` of the same name.
/// The rest of the engine never sees it: [`crate::conversion::apivec::ApiVec`]
/// requires each API to have a unique name, so the `IgnoredItem` which the
/// variable would otherwise become is discarded in favour of the type. That's
/// why this scan runs over the raw `bindgen` output rather than over `Api`s.
pub(crate) fn find_types_shadowed_by_variables(items: &[Item]) -> HashSet<QualifiedName> {
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
                    if let ForeignItem::Static(s) = foreign_item {
                        shadowed.insert(QualifiedName::new(ns, s.ident.clone().into()));
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
