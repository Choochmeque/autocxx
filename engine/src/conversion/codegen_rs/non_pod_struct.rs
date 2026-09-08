// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use syn::{
    parse_quote, Attribute, GenericParam, Generics, Ident, Item, Path, TraitBound,
    TraitBoundModifier, TypeParamBound,
};

use crate::types::{make_ident, Namespace, QualifiedName};

use super::find_output_mod_root;
use quote::quote;

/// The prefix bindgen gives the traits through which it renders a dependent
/// qualified name - `typename T::Inner` becomes
/// `<T as __bindgen_has_inner_type_Inner>::Inner`, and the parameter carries a
/// bound naming that trait. Such a trait is declared in bindgen's root module,
/// so a bound copied out of that module has to be requalified to reach it.
const INNER_TYPE_TRAIT_PREFIX: &str = "__bindgen_has_inner_type_";

/// Make an opaque wrapper around a bindgen type.
// Constraints here (thanks to dtolnay@ for this explanation of why the
// following is needed:)
// (1) If the real alignment of the C++ type is smaller and a reference
// is returned from C++ to Rust, mere existence of an insufficiently
// aligned reference in Rust causes UB even if never dereferenced
// by Rust code
// (see https://doc.rust-lang.org/1.47.0/reference/behavior-considered-undefined.html).
// Rustc can use least-significant bits of the reference for other storage.
// (if we have layout information from bindgen we use that instead)
// (2) We want to ensure the type is !Unpin. This is load-bearing for
// soundness, not just ergonomics: a non-POD C++ type is not necessarily
// trivially relocatable (libstdc++'s std::string, for instance, stores a
// pointer to its own inline SSO buffer), so moving one bitwise within Rust
// memory corrupts it. Rust hands out `&mut T` freely once `T: Unpin`, and
// safe code can then call `core::mem::swap`, which is exactly such a bitwise
// move. The `_pinned` field below is what actually makes the type !Unpin;
// without it the type would inherit Unpin from the bindgen struct (whose
// fields are plain pointers and byte arrays) and safe Rust could corrupt
// C++ objects with no `unsafe` anywhere. See google/autocxx#1265.
// (3) We want to ensure it's not Send or Sync
// In addition, we want to avoid UB:
// (4) By marking the data as MaybeUninit we ensure there's no UB
//     by Rust assuming it's initialized
// (5) By marking it as UnsafeCell we perhaps help reduce aliasing UB.
//     This is on the assumption that references to this type may pass
//     through C++ and get duplicated, so there may be multiple Rust
//     references to the same underlying data.
//     The correct solution to this is to put autocxx into the mode
//     where it uses CppRef<T> instead of Rust references, but otherwise,
//     using UnsafeCell here may help a bit. It probably does not
//     eliminate the UB here for the following reasons:
//     a) The references floating around are to the outer type, not the
//        data stored within the UnsafeCell. (I think this is OK)
//     b) C++ may have multiple mutable references, or may have mutable
//        references coexisting with immutable references, and no amount
//        of UnsafeCell can make that safe.
//     Nevertheless the use of UnsafeCell here may (*may*) reduce the
//     opportunities for aliasing UB. Again, the only actual way to
//     eliminate UB is to use CppRef<T> everywhere instead of &T and &mut T.
//
// For opaque types, the Rusty opaque structure could in fact be generated
// by four different things:
// a) bindgen, using its --opaque-type command line argument or the library
//    equivalent;
// b) us (autocxx), by making a [u8; N] byte long structure
// c) us (autocxx), by making a struct containing the bindgen struct
//    in an inaccessible field (that's what we do here)
// d) cxx, using "type B;" in an "extern "C++"" section
// We never use (a) because bindgen requires an allowlist of opaque types.
// Furthermore, it sometimes then discards struct definitions entirely
// and says "type A = [u8;2];" or something else which makes our life
// much more difficult.
// We use (d) for abstract types. For everything else, we do (c)
// for maximal control. See codegen_rs/mod.rs generate_type for more notes.
// We could switch to (b) and earlier version of autocxx did that.
//
// It is worth noting that our constraints here are a bit more severe than
// for cxx. In the case of cxx, C++ types are usually represented as
// zero-sized types within Rust. Zero-sized types, by definition, can't
// have overlapping references and thus can't have aliasing UB. We can't
// do that because we want C++ types to be representable on the Rust stack,
// and thus we need to tell Rust their real size and alignment.
pub(super) fn generate_opaque_type(
    name: &QualifiedName,
    bindgen_generics: &Generics,
    doc_attrs: &[Attribute],
) -> Item {
    let segs = find_output_mod_root(name.get_namespace()).chain(name.get_bindgen_path_idents());
    let final_name = name.get_final_ident().0;

    // The parameters are bindgen's own, not fresh ones, because a parameter may
    // carry a bound - that is how bindgen renders a member whose type is named
    // through the parameter - and a bound naming an arbitrary trait cannot be
    // reinvented here. Only the declaration takes the bounds; the type being
    // wrapped is named with the parameters alone.
    let declaration = rewrite_for_the_output_mod(bindgen_generics, name.get_namespace());
    let params = &declaration.params;
    let where_clause = &declaration.where_clause;
    let params = if params.is_empty() {
        quote! {}
    } else {
        quote! { < #params > }
    };
    let arguments = bindgen_generics.params.iter().map(|param| match param {
        GenericParam::Type(tp) => {
            let ident = &tp.ident;
            quote! { #ident }
        }
        GenericParam::Lifetime(lp) => {
            let lifetime = &lp.lifetime;
            quote! { #lifetime }
        }
        GenericParam::Const(cp) => {
            let ident = &cp.ident;
            quote! { #ident }
        }
    });
    let arguments = if bindgen_generics.params.is_empty() {
        quote! {}
    } else {
        quote! { < #(#arguments),* > }
    };
    let declaration = quote! { #params #where_clause };
    Item::Struct(parse_quote! {
        #[repr(transparent)]
        #(#doc_attrs)*
        pub struct #final_name #declaration {
            _hidden_contents: ::core::cell::UnsafeCell<::core::mem::MaybeUninit<#(#segs)::* #arguments>>,
            // Zero-sized, so `repr(transparent)` still applies to the field
            // above; its only job is to make this type !Unpin. See note (2).
            _pinned: ::core::marker::PhantomData<::core::marker::PhantomPinned>,
        }
    })
}

/// As `generics`, but as the output module can say it.
///
/// Two things differ there. A bound naming an inner-type trait has to reach
/// bindgen's root module, which is where bindgen declares such a trait and which
/// is the only place bindgen's own spelling of it resolves from. And `?Sized` is
/// not a relaxation this wrapper can keep, whatever bindgen declared it for: the
/// wrapped type goes inside a `MaybeUninit`, which wants a size.
///
/// Only the bounds are rewritten. The only predicates bindgen writes have a
/// parameter as their subject, so there is nothing else here to requalify; a
/// predicate about some other bindgen type would need its subject doing too.
fn rewrite_for_the_output_mod(generics: &Generics, ns: &Namespace) -> Generics {
    let mut generics = generics.clone();
    for param in &mut generics.params {
        if let GenericParam::Type(tp) = param {
            tp.bounds = tp
                .bounds
                .iter()
                .filter(|bound| !is_relaxation(bound))
                .cloned()
                .map(|bound| requalified(bound, ns))
                .collect();
        }
    }
    if let Some(where_clause) = &mut generics.where_clause {
        for predicate in &mut where_clause.predicates {
            if let syn::WherePredicate::Type(pt) = predicate {
                pt.bounds = pt
                    .bounds
                    .iter()
                    .filter(|bound| !is_relaxation(bound))
                    .cloned()
                    .map(|bound| requalified(bound, ns))
                    .collect();
            }
        }
    }
    generics
}

fn is_relaxation(bound: &TypeParamBound) -> bool {
    matches!(
        bound,
        TypeParamBound::Trait(TraitBound {
            modifier: TraitBoundModifier::Maybe(_),
            ..
        })
    )
}

fn requalified(mut bound: TypeParamBound, ns: &Namespace) -> TypeParamBound {
    let TypeParamBound::Trait(tb) = &mut bound else {
        return bound;
    };
    let Some(trait_name) = inner_type_trait_named(&tb.path) else {
        return bound;
    };
    let prefix = find_output_mod_root(ns).map(|ident| ident.0).chain(
        ["bindgen", "root"]
            .iter()
            .map(make_ident)
            .map(|ident| ident.0),
    );
    tb.path = parse_quote! { #(#prefix ::)* #trait_name };
    bound
}

/// The inner-type trait a path names, however bindgen spelled the path: bare
/// from inside its root module, or through that module from anywhere else.
fn inner_type_trait_named(path: &Path) -> Option<Ident> {
    if path.leading_colon.is_some() {
        return None;
    }
    let mut segments = path.segments.iter();
    let last = match path.segments.len() {
        1 => segments.next()?,
        2 => {
            let first = segments.next()?;
            if first.ident != "root" || !first.arguments.is_none() {
                return None;
            }
            segments.next()?
        }
        _ => return None,
    };
    (last.arguments.is_none() && last.ident.to_string().starts_with(INNER_TYPE_TRAIT_PREFIX))
        .then(|| last.ident.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::{Generics, ItemStruct};

    /// syn parses a `where` clause as part of the item rather than of its
    /// `Generics`, which is also how one reaches this code: off a bindgen
    /// struct.
    fn generics_of(item: ItemStruct) -> Generics {
        item.generics
    }

    fn where_clause_of(item: ItemStruct, ns: &Namespace) -> String {
        let rewritten = rewrite_for_the_output_mod(&generics_of(item), ns);
        let where_clause = rewritten
            .where_clause
            .expect("the input has a where clause");
        quote! { #where_clause }.to_string()
    }

    /// bindgen names the trait through its own root module, which nothing in the
    /// output module imports under that name.
    #[test]
    fn an_inner_type_trait_bound_reaches_bindgens_root_module() {
        assert_eq!(
            where_clause_of(
                parse_quote! {
                    struct S<T> where T: root::__bindgen_has_inner_type_Inner {}
                },
                &Namespace::new(),
            ),
            quote! { where T: bindgen::root::__bindgen_has_inner_type_Inner }.to_string()
        );
    }

    /// Bare is how it reads inside that module, which is where bindgen writes it
    /// when there are no namespaces to write it from.
    #[test]
    fn a_bare_inner_type_trait_bound_is_reached_the_same_way() {
        assert_eq!(
            where_clause_of(
                parse_quote! {
                    struct S<T> where T: __bindgen_has_inner_type_Inner {}
                },
                &Namespace::new(),
            ),
            quote! { where T: bindgen::root::__bindgen_has_inner_type_Inner }.to_string()
        );
    }

    /// A wrapper in a namespace is that many modules down from the one which
    /// holds `bindgen`.
    #[test]
    fn a_wrapper_in_a_namespace_climbs_out_to_reach_it() {
        assert_eq!(
            where_clause_of(
                parse_quote! {
                    struct S<T> where T: root::__bindgen_has_inner_type_Inner {}
                },
                &Namespace::from_user_input("a::b"),
            ),
            quote! { where T: super::super::bindgen::root::__bindgen_has_inner_type_Inner }
                .to_string()
        );
    }

    #[test]
    fn any_other_bound_is_left_as_bindgen_wrote_it() {
        assert_eq!(
            where_clause_of(
                parse_quote! {
                    struct S<T> where T: Copy + root::SomeOtherTrait {}
                },
                &Namespace::new(),
            ),
            quote! { where T: Copy + root::SomeOtherTrait }.to_string()
        );
    }

    fn wrapper(item: ItemStruct, ns: &Namespace) -> String {
        let name = QualifiedName::new(ns, make_ident("Thing"));
        let wrapper = generate_opaque_type(&name, &generics_of(item), &[]);
        quote! { #wrapper }.to_string()
    }

    /// What the whole wrapper looks like: bindgen's parameter, the bound put
    /// where the output module can resolve it, and the wrapped type named with
    /// the parameter alone.
    #[test]
    fn the_wrapper_declares_bindgens_parameters_and_wraps_its_type() {
        let rendered = wrapper(
            parse_quote! {
                struct S<T> where T: root::__bindgen_has_inner_type_Inner {}
            },
            &Namespace::new(),
        );
        assert!(
            rendered.contains(
                &quote! {
                    pub struct Thing < T >
                    where T: bindgen::root::__bindgen_has_inner_type_Inner
                }
                .to_string()
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(&quote! { MaybeUninit < bindgen::root::Thing < T > > }.to_string()),
            "{rendered}"
        );
    }

    /// A predicate can stand without a parameter of its own to hang off, so it
    /// is emitted whatever the parameter count.
    #[test]
    fn a_where_clause_survives_having_no_parameters() {
        let rendered = wrapper(
            parse_quote! { struct S where u8: Copy {} },
            &Namespace::new(),
        );
        assert!(
            rendered.contains(&quote! { pub struct Thing where u8: Copy }.to_string()),
            "{rendered}"
        );
    }

    /// The wrapped type goes inside a `MaybeUninit`, so the wrapper cannot keep
    /// a relaxation of `Sized` - but it keeps the parameter and its default.
    #[test]
    fn a_sized_relaxation_is_dropped_and_the_default_kept() {
        let generics = generics_of(parse_quote! { struct S<FAM: ?Sized = [u8; 0]> {} });
        let rewritten = rewrite_for_the_output_mod(&generics, &Namespace::new());
        let params = &rewritten.params;
        assert_eq!(
            quote! { #params }.to_string(),
            quote! { FAM = [u8; 0] }.to_string()
        );
    }
}
