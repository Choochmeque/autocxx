// Copyright 2026 The autocxx maintainers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Removes items from the bindgen-generated mod which could never
//! compile because they reference template parameters that bindgen
//! failed to bind.
//!
//! With some standard library headers (particularly newer libc++,
//! e.g. Xcode 26), bindgen emits type aliases for class-scoped
//! typedefs where the right-hand side still names a template
//! parameter of the enclosing C++ class, but the alias itself
//! declares no such generic parameter:
//!
//! ```text
//! pub type basic_string___self_view = root::std::basic_string_view<_CharT>;
//! ```
//!
//! `_CharT` is not declared anywhere, so the generated code fails
//! with E0425 "cannot find type `_CharT` in this scope". See
//! google/autocxx#1480 and google/autocxx#1051. Since autocxx
//! includes bindgen's output verbatim, we must prune such items
//! before emitting the mod. Nothing can reference these aliases
//! successfully anyway, so removing them is safe.

use indexmap::set::IndexSet as HashSet;
use syn::{GenericArgument, GenericParam, Item, ItemMod, PathArguments, ReturnType, Type};

/// Remove type aliases in the bindgen mod (recursively) which refer
/// to type names that are not bound anywhere: not a generic parameter
/// of the alias, not a type defined in the bindgen output, and not a
/// Rust primitive.
pub(super) fn remove_unbound_type_aliases(bindgen_mod: &mut ItemMod) {
    let mut defined = HashSet::new();
    collect_defined_type_names(bindgen_mod, &mut defined);
    prune_mod(bindgen_mod, &defined);
}

fn collect_defined_type_names(item_mod: &ItemMod, defined: &mut HashSet<String>) {
    if let Some((_, items)) = &item_mod.content {
        for item in items {
            match item {
                Item::Struct(s) => {
                    defined.insert(s.ident.to_string());
                }
                Item::Enum(e) => {
                    defined.insert(e.ident.to_string());
                }
                Item::Union(u) => {
                    defined.insert(u.ident.to_string());
                }
                Item::Type(t) => {
                    defined.insert(t.ident.to_string());
                }
                Item::Mod(m) => collect_defined_type_names(m, defined),
                _ => {}
            }
        }
    }
}

fn prune_mod(item_mod: &mut ItemMod, defined: &HashSet<String>) {
    if let Some((_, items)) = &mut item_mod.content {
        items.retain(|item| match item {
            Item::Type(t) => {
                let params: HashSet<String> = t
                    .generics
                    .params
                    .iter()
                    .filter_map(|p| match p {
                        GenericParam::Type(tp) => Some(tp.ident.to_string()),
                        _ => None,
                    })
                    .collect();
                !has_unbound_ident(&t.ty, &params, defined)
            }
            _ => true,
        });
        for item in items {
            if let Item::Mod(m) = item {
                prune_mod(m, defined);
            }
        }
    }
}

fn is_primitive(ident: &str) -> bool {
    matches!(
        ident,
        "bool"
            | "char"
            | "str"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f32"
            | "f64"
    )
}

fn has_unbound_ident(ty: &Type, params: &HashSet<String>, defined: &HashSet<String>) -> bool {
    match ty {
        Type::Path(tp) => {
            if let Some(qself) = &tp.qself {
                if has_unbound_ident(&qself.ty, params, defined) {
                    return true;
                }
            }
            if tp.qself.is_none()
                && tp.path.leading_colon.is_none()
                && tp.path.segments.len() == 1
                && tp.path.segments[0].arguments.is_none()
            {
                let ident = tp.path.segments[0].ident.to_string();
                if !params.contains(&ident) && !defined.contains(&ident) && !is_primitive(&ident) {
                    return true;
                }
            }
            tp.path.segments.iter().any(|seg| match &seg.arguments {
                PathArguments::AngleBracketed(ab) => ab.args.iter().any(|arg| match arg {
                    GenericArgument::Type(ty) => has_unbound_ident(ty, params, defined),
                    GenericArgument::AssocType(at) => has_unbound_ident(&at.ty, params, defined),
                    _ => false,
                }),
                PathArguments::Parenthesized(p) => {
                    p.inputs
                        .iter()
                        .any(|ty| has_unbound_ident(ty, params, defined))
                        || match &p.output {
                            ReturnType::Type(_, ty) => has_unbound_ident(ty, params, defined),
                            ReturnType::Default => false,
                        }
                }
                PathArguments::None => false,
            })
        }
        Type::Reference(r) => has_unbound_ident(&r.elem, params, defined),
        Type::Ptr(p) => has_unbound_ident(&p.elem, params, defined),
        Type::Slice(s) => has_unbound_ident(&s.elem, params, defined),
        Type::Array(a) => has_unbound_ident(&a.elem, params, defined),
        Type::Group(g) => has_unbound_ident(&g.elem, params, defined),
        Type::Paren(p) => has_unbound_ident(&p.elem, params, defined),
        Type::Tuple(t) => t
            .elems
            .iter()
            .any(|ty| has_unbound_ident(ty, params, defined)),
        Type::BareFn(f) => {
            f.inputs
                .iter()
                .any(|arg| has_unbound_ident(&arg.ty, params, defined))
                || match &f.output {
                    ReturnType::Type(_, ty) => has_unbound_ident(ty, params, defined),
                    ReturnType::Default => false,
                }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn count_aliases(item_mod: &ItemMod) -> usize {
        let mut count = 0;
        fn walk(item_mod: &ItemMod, count: &mut usize) {
            if let Some((_, items)) = &item_mod.content {
                for item in items {
                    match item {
                        Item::Type(_) => *count += 1,
                        Item::Mod(m) => walk(m, count),
                        _ => {}
                    }
                }
            }
        }
        walk(item_mod, &mut count);
        count
    }

    #[test]
    fn removes_alias_with_unbound_param() {
        // The google/autocxx#1480 shape: RHS names _CharT but the
        // alias declares no generics and _CharT is defined nowhere.
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub struct basic_string_view {
                    _p: u8,
                }
                pub type basic_string___self_view = root::std::basic_string_view<_CharT>;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 0);
    }

    #[test]
    fn keeps_alias_with_declared_param() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub struct basic_stream {
                    _p: u8,
                }
                pub type sentry_stream_type<_CharT> = root::basic_stream<_CharT>;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 1);
    }

    #[test]
    fn keeps_alias_to_defined_and_primitive_types() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub struct Concrete {
                    _p: u8,
                }
                pub type A = Concrete;
                pub type B = u32;
                pub type C = *mut Concrete;
                pub type D = ::std::os::raw::c_char;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 4);
    }

    #[test]
    fn removes_unbound_in_nested_mod_and_nested_position() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub mod std {
                        pub type bad = super::basic_thing<_Traits>;
                        pub type bad_ref = *const _Pointer;
                        pub type good = u8;
                    }
                }
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 1);
    }
}
