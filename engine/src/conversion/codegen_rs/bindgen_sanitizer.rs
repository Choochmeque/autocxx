// Copyright 2026 The autocxx maintainers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Repairs the bindgen-generated mod, which autocxx emits verbatim,
//! in the cases where bindgen hands us Rust that could never compile.
//!
//! # Unbound template parameters
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
//! before emitting the mod.
//!
//! Removal is safe for anything that referenced a pruned alias by
//! that bare name: it could not have compiled either, and alias
//! chains are handled by pruning to a fixpoint. Known limitation:
//! a struct *field* typed via a multi-segment path to a pruned
//! alias (e.g. `root::std::__tree___end_node_t`) is left dangling —
//! the struct was equally uncompilable before, but fixing it needs
//! a cascade to opaque the containing struct, tracked separately.
//!
//! # Colliding names
//!
//! bindgen names a member of a class by joining the names of its
//! ancestors, so `Outer::Inner` becomes `Outer_Inner` and two such
//! `Inner`s never collide. That breaks down for members of a class
//! *template specialization*: bindgen does not record the
//! specialization as the member's parent, so the member is emitted
//! into an enclosing module under its own bare name. Two distinct
//! C++ types then land on one Rust name:
//!
//! ```text
//! pub type iterator = root::pointer;      // absl::cj<l>::iterator
//! pub struct iterator { _unused: [u8; 0] } // absl::j::ct<..>::iterator
//! ```
//!
//! which is E0428, "the name `iterator` is defined multiple times".
//! See google/autocxx#490. (Not to be confused with
//! google/autocxx#486, where two types from *different* namespaces
//! collide in the flat `cxx::bridge` namespace; `check_names` catches
//! that one and reports it.)
//!
//! We cannot repair this by renaming. Every reference bindgen emitted
//! (`root::iterator` in a field, in a type alias, in an `extern "C"`
//! signature) names whichever of the two types the C++ meant, and
//! bindgen's output no longer records which — so renaming one of them
//! would have to guess at each reference site, and guessing wrong
//! silently rewires an FFI signature to the wrong type. Instead we
//! collapse the collision into a single opaque placeholder, so that
//! every reference still resolves and the mod compiles.
//!
//! That is only sound for a name the *parse phase* recorded as
//! duplicated, which is why the caller passes that set in rather than
//! letting us infer it from the emitted mod. For such a name,
//! `ApiVec::push` has already replaced every API with an
//! `Api::IgnoredItem`, so nothing depending on it can be generated and
//! the references left behind are debris. A name which merely *looks*
//! duplicated carries no such guarantee: the parse phase skips some
//! declarations (an unrepresentable struct, say) while a type alias of
//! the same name survives, is re-exported verbatim, and may be
//! genuinely referenced. Collapsing that would quietly turn an alias
//! of a pointer into a zero-sized struct, so we leave it alone.

use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;
use syn::{
    parse_quote, GenericArgument, GenericParam, Ident, Item, ItemMod, PathArguments, ReturnType,
    Type, TypeParamBound, UseTree,
};

use crate::types::{make_ident, Namespace, QualifiedName};

/// Remove type aliases in the bindgen mod (recursively) which refer
/// to type names that are not bound anywhere: not a generic parameter
/// of the alias, not a type defined or imported in the bindgen output,
/// and not a Rust primitive.
///
/// Pruning iterates to a fixpoint: removing an alias takes its name
/// out of scope, which can in turn invalidate aliases that referenced
/// it (`type Good = BadAlias;`).
pub(super) fn remove_unbound_type_aliases(bindgen_mod: &mut ItemMod) {
    let mut defined = HashSet::new();
    collect_defined_type_names(bindgen_mod, &mut defined);
    loop {
        let mut pruned = Vec::new();
        prune_mod(bindgen_mod, &defined, &mut pruned);
        if pruned.is_empty() {
            break;
        }
        for name in pruned {
            defined.swap_remove(&name);
        }
    }
}

/// Collapse type-namespace items which share a name within the same
/// bindgen module into a single opaque placeholder, so that the mod
/// compiles instead of hitting E0428.
///
/// `names_duplicated_by_bindgen` is the set gathered by the parse
/// phase. A name outside it is left exactly as bindgen wrote it, even
/// if it does collide, because we have no evidence that both
/// definitions are unusable - see the module documentation.
///
/// Only `struct`/`enum`/`union`/`type` are considered: those are the
/// items bindgen derives from C++ types, and a collision among them
/// is the one we have seen in the wild. Two other collisions are
/// possible in principle and are deliberately left to fail loudly
/// rather than be papered over: one in the value namespace (two
/// `const`s, say), and one between a type and a namespace module,
/// where collapsing would mean deleting the module and everything
/// inside it.
///
/// `impl` blocks for a collapsed name go too. autocxx never uses
/// bindgen's inherent impls (it binds the `extern "C"` declarations
/// instead), and keeping impls from two different C++ types on one
/// placeholder risks a fresh duplicate-method error.
pub(super) fn collapse_colliding_type_names(
    bindgen_mod: &mut ItemMod,
    names_duplicated_by_bindgen: &HashSet<QualifiedName>,
) {
    if names_duplicated_by_bindgen.is_empty() {
        return;
    }
    let Some((_, items)) = &mut bindgen_mod.content else {
        return;
    };
    for item in items {
        // With namespaces enabled bindgen puts everything in a mod
        // called `root`, which is the C++ global namespace; the items
        // beside it are autocxx's own and can never collide.
        if let Item::Mod(root_mod) = item {
            if root_mod.ident == "root" {
                collapse_in_mod(root_mod, &Namespace::new(), names_duplicated_by_bindgen);
            }
        }
    }
}

fn collapse_in_mod(
    item_mod: &mut ItemMod,
    ns: &Namespace,
    names_duplicated_by_bindgen: &HashSet<QualifiedName>,
) {
    let Some((_, items)) = &mut item_mod.content else {
        return;
    };
    // A name is only worth collapsing if it really is defined more than
    // once here. The parse phase can record a duplicate whose emitted
    // definitions are not both type items, and rewriting a lone
    // definition would change its meaning for no gain.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in items.iter() {
        if let Some(name) = type_namespace_item_name(item) {
            *counts.entry(name).or_default() += 1;
        }
    }
    let to_collapse: HashSet<String> = counts
        .into_iter()
        .filter(|(name, count)| {
            *count > 1
                && names_duplicated_by_bindgen.contains(&QualifiedName::new(ns, make_ident(name)))
        })
        .map(|(name, _)| name)
        .collect();
    if !to_collapse.is_empty() {
        let mut collapsed: HashSet<String> = HashSet::new();
        let mut replaced = Vec::with_capacity(items.len());
        for item in items.drain(..) {
            if let Some(ident) =
                type_namespace_item_ident(&item).filter(|id| to_collapse.contains(&id.to_string()))
            {
                // Emit the placeholder where the first of the colliding
                // definitions stood, so the surrounding items keep their
                // order, and drop the rest.
                if collapsed.insert(ident.to_string()) {
                    log::info!(
                        "Multiple bindgen items are named {ident}; replacing them all with an opaque type."
                    );
                    replaced.push(parse_quote! {
                        #[repr(C)]
                        pub struct #ident {
                            _unused: [u8; 0],
                        }
                    });
                }
            } else if impl_self_type_name(&item).is_some_and(|name| to_collapse.contains(&name)) {
                // Drop the impl block along with the type it was for.
            } else {
                replaced.push(item);
            }
        }
        *items = replaced;
    }
    for item in items {
        if let Item::Mod(m) = item {
            let child_ns = ns.push(m.ident.to_string());
            collapse_in_mod(m, &child_ns, names_duplicated_by_bindgen);
        }
    }
}

/// The identifier an item binds in the type namespace, for the kinds of
/// item bindgen generates from C++ types.
fn type_namespace_item_ident(item: &Item) -> Option<&Ident> {
    match item {
        Item::Struct(s) => Some(&s.ident),
        Item::Enum(e) => Some(&e.ident),
        Item::Union(u) => Some(&u.ident),
        Item::Type(t) => Some(&t.ident),
        _ => None,
    }
}

fn type_namespace_item_name(item: &Item) -> Option<String> {
    type_namespace_item_ident(item).map(Ident::to_string)
}

/// The bare name of the type an inherent `impl` block is for, if it is
/// written as a single path segment (which is how bindgen writes them).
fn impl_self_type_name(item: &Item) -> Option<String> {
    match item {
        Item::Impl(i) if i.trait_.is_none() => match &*i.self_ty {
            Type::Path(tp) if tp.qself.is_none() && tp.path.segments.len() == 1 => {
                Some(tp.path.segments[0].ident.to_string())
            }
            _ => None,
        },
        _ => None,
    }
}

fn collect_defined_type_names(item_mod: &ItemMod, defined: &mut HashSet<String>) {
    if let Some((_, items)) = &item_mod.content {
        for item in items {
            if let Some(name) = type_namespace_item_name(item) {
                defined.insert(name);
                continue;
            }
            match item {
                // Imports bind bare names too. In particular autocxx
                // injects `use super::{...}` and `use autocxx::c_char16_t
                // as bindgen_cchar16_t` into every bindgen module, and
                // aliases like `pub type Foo = bindgen_cchar16_t;` are
                // legitimate.
                Item::Use(u) => collect_use_names(&u.tree, defined),
                Item::Mod(m) => collect_defined_type_names(m, defined),
                _ => {}
            }
        }
    }
}

fn collect_use_names(tree: &UseTree, defined: &mut HashSet<String>) {
    match tree {
        UseTree::Path(p) => collect_use_names(&p.tree, defined),
        UseTree::Name(n) => {
            defined.insert(n.ident.to_string());
        }
        UseTree::Rename(r) => {
            defined.insert(r.rename.to_string());
        }
        UseTree::Group(g) => {
            for t in &g.items {
                collect_use_names(t, defined);
            }
        }
        UseTree::Glob(_) => {}
    }
}

fn prune_mod(item_mod: &mut ItemMod, defined: &HashSet<String>, pruned: &mut Vec<String>) {
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
                if has_unbound_ident(&t.ty, &params, defined) {
                    pruned.push(t.ident.to_string());
                    false
                } else {
                    true
                }
            }
            _ => true,
        });
        for item in items {
            if let Item::Mod(m) = item {
                prune_mod(m, defined, pruned);
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
        // For trait objects and impl-trait, only inspect the generic
        // arguments of the bounds; the trait names themselves are not
        // collected in `defined`, so checking them would false-positive.
        Type::TraitObject(t) => t
            .bounds
            .iter()
            .any(|b| bound_has_unbound_ident(b, params, defined)),
        Type::ImplTrait(t) => t
            .bounds
            .iter()
            .any(|b| bound_has_unbound_ident(b, params, defined)),
        Type::BareFn(f) => {
            f.inputs
                .iter()
                .any(|arg| has_unbound_ident(&arg.ty, params, defined))
                || match &f.output {
                    ReturnType::Type(_, ty) => has_unbound_ident(ty, params, defined),
                    ReturnType::Default => false,
                }
        }
        // Type::Macro, Type::Verbatim etc.: we can't see inside, so
        // conservatively keep the alias (false negatives are safe;
        // false positives would remove legitimate API).
        _ => false,
    }
}

fn bound_has_unbound_ident(
    bound: &TypeParamBound,
    params: &HashSet<String>,
    defined: &HashSet<String>,
) -> bool {
    match bound {
        TypeParamBound::Trait(tb) => tb.path.segments.iter().any(|seg| match &seg.arguments {
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
        }),
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
    fn keeps_alias_to_imported_name() {
        // autocxx injects imports (including a rename) into every
        // bindgen module; aliases to those names are legitimate.
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                #[allow(unused_imports)]
                use super::{cxxbridge, output};
                use autocxx::c_char16_t as bindgen_cchar16_t;
                pub type Foo = bindgen_cchar16_t;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 1);
    }

    #[test]
    fn prunes_alias_chains_to_fixpoint() {
        // GoodLooking references BadAlias which itself gets pruned;
        // both must go.
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub struct Real {
                    _p: u8,
                }
                pub type BadAlias = Real<_CharT>;
                pub type GoodLooking = BadAlias;
                pub type Unaffected = Real;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 1);
    }

    #[test]
    fn removes_unbound_in_trait_object_bound_args() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub type Bad = *const dyn SomeTrait<_CharT>;
            }
        };
        remove_unbound_type_aliases(&mut m);
        assert_eq!(count_aliases(&m), 0);
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

    /// The names bound in the type namespace by the items directly
    /// inside the named module, in order.
    fn type_names_in(item_mod: &ItemMod, wanted: &str) -> Vec<String> {
        fn walk(item_mod: &ItemMod, wanted: &str, out: &mut Vec<String>) {
            if item_mod.ident == wanted {
                if let Some((_, items)) = &item_mod.content {
                    out.extend(items.iter().filter_map(type_namespace_item_name));
                }
            }
            if let Some((_, items)) = &item_mod.content {
                for item in items {
                    if let Item::Mod(m) = item {
                        walk(m, wanted, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(item_mod, wanted, &mut out);
        out
    }

    /// The set the parse phase hands us, written in C++ form: `iterator`
    /// lives in the global namespace, `a::iterator` in namespace `a`.
    fn duplicated_names(names: &[&str]) -> HashSet<QualifiedName> {
        names
            .iter()
            .copied()
            .map(QualifiedName::new_from_cpp_name)
            .collect()
    }

    #[test]
    fn collapses_colliding_type_names() {
        // The google/autocxx#490 shape: members of two different class
        // template specializations both land on `root::iterator`.
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub type iterator = root::pointer;
                    pub type pointer = *mut root::Elem;
                    #[repr(C)]
                    pub struct Cursor {
                        pub it: root::iterator,
                    }
                    #[repr(C)]
                    pub struct iterator {
                        _unused: [u8; 0],
                    }
                }
            }
        };
        collapse_colliding_type_names(&mut m, &duplicated_names(&["iterator"]));
        assert_eq!(
            type_names_in(&m, "root"),
            vec!["iterator", "pointer", "Cursor"]
        );
    }

    #[test]
    fn collapses_more_than_two_colliding_names() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub type iterator = u8;
                    pub struct iterator {
                        _unused: [u8; 0],
                    }
                    pub union iterator {
                        a: u8,
                    }
                    pub enum iterator {
                        A,
                    }
                }
            }
        };
        collapse_colliding_type_names(&mut m, &duplicated_names(&["iterator"]));
        assert_eq!(type_names_in(&m, "root"), vec!["iterator"]);
    }

    #[test]
    fn collapsing_drops_impls_of_the_collapsed_type() {
        let mut m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub struct iterator {
                        pub a: u8,
                    }
                    impl iterator {
                        pub fn get(&self) -> u8 {
                            self.a
                        }
                    }
                    pub type iterator = u8;
                    pub struct other {
                        _unused: [u8; 0],
                    }
                    impl other {
                        pub fn ok() {}
                    }
                }
            }
        };
        collapse_colliding_type_names(&mut m, &duplicated_names(&["iterator"]));
        let items = match &m.content.as_ref().unwrap().1[0] {
            Item::Mod(root) => root.content.as_ref().unwrap().1.clone(),
            _ => panic!("expected root mod"),
        };
        assert_eq!(
            items
                .iter()
                .filter_map(impl_self_type_name)
                .collect::<Vec<_>>(),
            vec!["other"]
        );
    }

    /// Assert that collapsing left the mod exactly as bindgen wrote it.
    fn assert_collapse_is_a_no_op(m: &ItemMod, duplicated: &HashSet<QualifiedName>) {
        let mut after = m.clone();
        collapse_colliding_type_names(&mut after, duplicated);
        assert_eq!(
            quote::ToTokens::to_token_stream(&after).to_string(),
            quote::ToTokens::to_token_stream(m).to_string()
        );
    }

    #[test]
    fn collapsing_leaves_distinct_and_differently_scoped_names_alone() {
        // Same name in two different modules is not a collision, and
        // neither is a type sharing a name with a function - however the
        // parse phase came to record those names as duplicated.
        let m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub mod a {
                        pub struct iterator {
                            _unused: [u8; 0],
                        }
                    }
                    pub mod b {
                        pub struct iterator {
                            _unused: [u8; 0],
                        }
                    }
                    pub struct thing {
                        _unused: [u8; 0],
                    }
                    pub fn thing() {}
                }
            }
        };
        assert_collapse_is_a_no_op(
            &m,
            &duplicated_names(&["a::iterator", "b::iterator", "thing"]),
        );
    }

    #[test]
    fn collapsing_leaves_a_collision_the_parse_phase_did_not_record_alone() {
        // Two `iterator`s in one mod, but the parse phase did not record
        // that name as duplicated: it skipped one of the declarations for
        // its own reasons, and the alias survived as a real type which
        // other items may legitimately refer to. Collapsing on the
        // syntactic collision alone would turn that alias into a
        // zero-sized struct and quietly change the FFI signatures using
        // it, so the mod is emitted unaltered. The set is non-empty so
        // that the per-name gate is what the test exercises, not the
        // empty-set shortcut.
        let m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub type iterator = *mut root::Elem;
                    pub struct iterator {
                        _unused: [u8; 0],
                    }
                    #[repr(C)]
                    pub struct Cursor {
                        pub it: root::iterator,
                    }
                }
            }
        };
        assert_collapse_is_a_no_op(&m, &duplicated_names(&["Elem"]));
    }

    #[test]
    fn collapsing_leaves_a_type_colliding_with_a_module_alone() {
        // Collapsing would mean deleting the module and everything in
        // it, so this collision is left to fail loudly instead.
        let m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub struct iterator {
                        _unused: [u8; 0],
                    }
                    pub mod iterator {
                        pub struct Inner {
                            _unused: [u8; 0],
                        }
                    }
                }
            }
        };
        assert_collapse_is_a_no_op(&m, &duplicated_names(&["iterator"]));
    }

    #[test]
    fn collapsing_leaves_a_value_namespace_collision_alone() {
        // An opaque struct is no substitute for a constant, so a
        // collision in the value namespace is left to fail loudly too.
        let m: ItemMod = parse_quote! {
            mod bindgen {
                pub mod root {
                    pub const LIMIT: u32 = 1;
                    pub const LIMIT: u32 = 2;
                }
            }
        };
        assert_collapse_is_a_no_op(&m, &duplicated_names(&["LIMIT"]));
    }
}
