// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The traits bindgen uses to render a dependent qualified name, and what a
//! template parameter bounded by one of them is being asked for.
//!
//! `typename T::Inner` has no type until `T` is known, so bindgen renders it as
//! `<T as __bindgen_has_inner_type_Inner>::Inner` and bounds `T` by that trait,
//! implementing it for each type which has such an inner type. Both the
//! analysis, which has to know which instantiations can satisfy such a bound,
//! and the Rust codegen, which copies the bound onto its own wrapper, read the
//! bound through this module so that they agree on what one is.
//!
//! Written by `engine/third_party/patches/32-dependent-qualified-types.patch`,
//! which rebases the bug reported upstream as
//! <https://github.com/rust-lang/rust-bindgen/issues/1924>.

use syn::{GenericParam, Generics, Ident, Path, TypeParamBound, WherePredicate};

/// The prefix bindgen gives such a trait. The name it looks up is the rest of
/// it, so `__bindgen_has_inner_type_value_type` is the trait for `value_type`.
pub(crate) const INNER_TYPE_TRAIT_PREFIX: &str = "__bindgen_has_inner_type_";

/// The inner-type trait a path names, however bindgen spelled the path: bare
/// from inside its root module, or through that module from anywhere else.
pub(crate) fn inner_type_trait_named(path: &Path) -> Option<Ident> {
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

/// The inner type a trait of this name looks up: `value_type` for
/// `__bindgen_has_inner_type_value_type`.
pub(crate) fn inner_type_looked_up(trait_name: &Ident) -> String {
    trait_name
        .to_string()
        .trim_start_matches(INNER_TYPE_TRAIT_PREFIX)
        .to_owned()
}

/// For each of a type's template parameters in the order they are declared, the
/// inner types an instantiation's argument has to have.
///
/// Empty for a parameter with no such bound, so the result is as long as the
/// parameter list and can be indexed by the position of a template argument.
pub(crate) fn inner_types_required_of_params(generics: &Generics) -> Vec<Vec<String>> {
    let params: Vec<&Ident> = generics
        .params
        .iter()
        .filter_map(|param| match param {
            GenericParam::Type(ty) => Some(&ty.ident),
            _ => None,
        })
        .collect();
    let mut required = vec![Vec::new(); params.len()];
    let Some(where_clause) = &generics.where_clause else {
        return required;
    };
    for predicate in &where_clause.predicates {
        let WherePredicate::Type(predicate) = predicate else {
            continue;
        };
        let syn::Type::Path(bounded) = &predicate.bounded_ty else {
            continue;
        };
        let Some(bounded) = bounded.path.get_ident() else {
            continue;
        };
        let Some(position) = params.iter().position(|param| *param == bounded) else {
            continue;
        };
        for bound in &predicate.bounds {
            let TypeParamBound::Trait(bound) = bound else {
                continue;
            };
            if let Some(trait_name) = inner_type_trait_named(&bound.path) {
                required[position].push(inner_type_looked_up(&trait_name));
            }
        }
    }
    required
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn required_of(item: syn::ItemStruct) -> Vec<Vec<String>> {
        inner_types_required_of_params(&item.generics)
    }

    #[test]
    fn no_parameters_needs_nothing() {
        assert!(required_of(parse_quote! { struct A { a: u8 } }).is_empty());
    }

    #[test]
    fn an_unbounded_parameter_needs_nothing() {
        assert_eq!(
            required_of(parse_quote! { struct A<T> { a: T } }),
            vec![Vec::<String>::new()]
        );
    }

    #[test]
    fn a_bound_is_read_through_the_root_module() {
        assert_eq!(
            required_of(parse_quote! {
                struct A<T> where T: root::__bindgen_has_inner_type_value_type { a: T }
            }),
            vec![vec!["value_type".to_string()]]
        );
    }

    #[test]
    fn a_bare_bound_is_read_too() {
        assert_eq!(
            required_of(parse_quote! {
                struct A<T> where T: __bindgen_has_inner_type_size_type { a: T }
            }),
            vec![vec!["size_type".to_string()]]
        );
    }

    #[test]
    fn each_parameter_is_answered_at_its_own_position() {
        assert_eq!(
            required_of(parse_quote! {
                struct A<T, U, V>
                where
                    V: root::__bindgen_has_inner_type_value_type,
                    T: Clone,
                { a: T, b: U, c: V }
            }),
            vec![Vec::new(), Vec::new(), vec!["value_type".to_string()]]
        );
    }

    #[test]
    fn two_inner_types_of_one_parameter_are_both_kept() {
        assert_eq!(
            required_of(parse_quote! {
                struct A<T>
                where
                    T: root::__bindgen_has_inner_type_value_type
                        + root::__bindgen_has_inner_type_size_type,
                { a: T }
            }),
            vec![vec!["value_type".to_string(), "size_type".to_string()]]
        );
    }

    #[test]
    fn an_unrelated_bound_is_not_one_of_these() {
        assert_eq!(
            required_of(parse_quote! { struct A<T> where T: Clone { a: T } }),
            vec![Vec::<String>::new()]
        );
    }

    #[test]
    fn a_lifetime_is_not_a_type_parameter() {
        assert_eq!(
            required_of(parse_quote! {
                struct A<'a, T> where T: root::__bindgen_has_inner_type_value_type { a: &'a T }
            }),
            vec![vec!["value_type".to_string()]]
        );
    }
}
