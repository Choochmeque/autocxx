// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::{
    conversion::{api::Api, apivec::ApiVec, AnalysisPhase, ConvertErrorFromCpp},
    parse_callbacks::CppOriginalName,
    types::QualifiedName,
};
use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;
use itertools::Itertools;
use quote::ToTokens;
use std::iter::once;
use syn::{Token, Type};

/// The suffix we append to a type's name to make the alias which lets us refer
/// to it even though its own name is hidden by a variable. See
/// [`UnshadowingAlias`].
const UNSHADOWING_SUFFIX: &str = "_autocxx_unshadowed";

/// A typedef we generate so that a type whose name is hidden by a same-named
/// variable or function can still be named.
///
/// C++ lets a variable hide a type of the same name in the same scope, which
/// POSIX does all over the place (`struct stat` and `extern struct stat stat`,
/// `struct timeval`, `struct timezone`...), and a function hides a type in
/// exactly the same way (`struct foo {...}; void foo();`). Once hidden, the
/// type can only be
/// named with an elaborated type specifier - `struct stat` rather than `stat` -
/// and that spelling isn't available to us everywhere we need it: `cxx`
/// generates its own C++ referring to `::stat`, and elaborated specifiers can't
/// appear in the `#[cxx_name]` attribute through which we'd have to ask for
/// something else, because `cxx` requires that to be a single identifier.
///
/// So instead we emit, once, a typedef which uses the elaborated specifier:
///
/// ```cpp
/// typedef struct stat stat_autocxx_unshadowed;
/// ```
///
/// and then use `stat_autocxx_unshadowed` as the type's C++ name throughout -
/// in the C++ we generate ourselves, and in the `#[cxx_name]`/`#[namespace]`
/// pair and `type_id!` string which decide what `cxx` generates. A typedef name
/// is not hidden by the declaration which hid the type, and is valid in every
/// context where we spell a type, including as a template argument
/// (`new_appropriately<T>`), in a
/// placement new, and in a pseudo-destructor call (`p->T::~T()`) - none of
/// which accept an elaborated specifier directly.
///
/// The typedef is emitted in the type's own namespace so that only the final
/// segment of the name changes; everything else about how `cxx` and the rest of
/// the engine see the type is unaffected. In particular the alias is a pure
/// type alias, so layout, size and alignment are those of the original type and
/// no `ExternType::Kind` obligation changes.
///
/// The alias does change the `type_id!` string, which is how `cxx` decides that
/// two bridges are talking about the same C++ type. A hand-written
/// `cxx::bridge` sharing one of these types would have to name the alias too.
/// That only affects types which previously produced C++ that didn't compile at
/// all, and a mismatch is a compile error rather than anything worse. Types
/// brought in with `extern_cpp_type!` take their identity from the other bridge
/// and so are deliberately left alone here.
pub(crate) struct UnshadowingAlias {
    /// `struct` or `enum`. Must agree in kind with the declaration, except that
    /// `struct` and `class` are interchangeable (we always say `struct`, and
    /// suppress the resulting `-Wmismatched-tags` where the type was declared
    /// as a `class`).
    pub(crate) tag: &'static str,
    /// The C++ name of the type as it would be spelled without the alias, e.g.
    /// `ns::bar` or `ns::Outer::Inner`.
    pub(crate) original_cpp_name: String,
    /// The final segment of the alias, e.g. `bar_autocxx_unshadowed`.
    pub(crate) alias: String,
}

/// Map from QualifiedName to original C++ name. Original C++ name does not
/// include the namespace; this can be assumed to be the same as the namespace
/// in the QualifiedName.
/// The "original C++ name" is mostly relevant in the case of nested types,
/// where the typename might be A::B within a namespace C::D.
pub(crate) struct CppNameMap {
    original_names: HashMap<QualifiedName, CppOriginalName>,
    /// Types whose plain C++ name is hidden by a variable or function, and the
    /// typedef we generate to get at them anyway.
    unshadowing_aliases: HashMap<QualifiedName, UnshadowingAlias>,
}

impl CppNameMap {
    /// Look through the APIs we've found to assemble the original name
    /// map. `shadowed_types` is the set of names which C++ won't look
    /// up as types, per [`crate::conversion::parse::find_shadowed_types`].
    pub(crate) fn new_from_apis<T: AnalysisPhase>(
        apis: &ApiVec<T>,
        shadowed_types: &HashSet<QualifiedName>,
    ) -> Self {
        let original_names: HashMap<_, _> = apis
            .iter()
            .filter_map(|api| {
                api.cpp_name()
                    .as_ref()
                    .map(|cpp_name| (api.name().clone(), cpp_name.clone()))
            })
            .collect();
        let unshadowing_aliases = apis
            .iter()
            .filter(|api| shadowed_types.contains(api.name()))
            .filter_map(|api| {
                // Only a class, struct or enum can be named with an elaborated
                // type specifier, so those are the only kinds we can rescue. A
                // typedef whose name is shadowed cannot be named at all.
                let tag = match api {
                    Api::Struct { .. } | Api::ForwardDeclaration { .. } => "struct",
                    Api::Enum { .. } => "enum",
                    _ => return None,
                };
                let name = api.name();
                Some((
                    name.clone(),
                    UnshadowingAlias {
                        tag,
                        original_cpp_name: unaliased_cpp_name(&original_names, name),
                        alias: format!("{}{UNSHADOWING_SUFFIX}", name.get_final_item()),
                    },
                ))
            })
            .collect();
        Self {
            original_names,
            unshadowing_aliases,
        }
    }

    /// Build a name map for use during the analysis phases, which spell C++
    /// type names only to use them as lookup keys - never to emit them. The
    /// unshadowing aliases don't exist yet at that point and would only make
    /// those keys harder to match up.
    pub(crate) fn new_for_analysis<T: AnalysisPhase>(apis: &ApiVec<T>) -> Self {
        Self::new_from_apis(apis, &HashSet::new())
    }

    /// The aliases we need to emit typedefs for, in the order the types were
    /// found.
    pub(crate) fn unshadowing_aliases(
        &self,
    ) -> impl Iterator<Item = (&QualifiedName, &UnshadowingAlias)> {
        self.unshadowing_aliases.iter()
    }

    /// The unshadowing alias for this type, if it has one. This is the name
    /// `cxx` must use, via `#[cxx_name]`, since the type's real name is hidden.
    pub(crate) fn unshadowing_alias(&self, qual_name: &QualifiedName) -> Option<&str> {
        self.unshadowing_aliases
            .get(qual_name)
            .map(|alias| alias.alias.as_str())
    }

    /// Imagine a nested struct in namespace::outer::inner
    /// This function converts from the bindgen name, namespace::outer_inner,
    /// to namespace::outer::inner.
    pub(crate) fn map(&self, qual_name: &QualifiedName) -> String {
        if let Some(alias) = self.unshadowing_aliases.get(qual_name) {
            return qual_name
                .get_namespace()
                .iter()
                .chain(once(alias.alias.as_str()))
                .join("::");
        }
        self.unaliased_cpp_name(qual_name)
    }

    /// The C++ name this type would have if we weren't renaming it to dodge a
    /// variable of the same name.
    fn unaliased_cpp_name(&self, qual_name: &QualifiedName) -> String {
        unaliased_cpp_name(&self.original_names, qual_name)
    }

    /// Get a stringified version of the last ident in the name.
    /// e.g. for namespace::outer_inner this will return inner.
    /// This is useful for doing things such as calling constructors
    /// such as inner() or destructors such as ~inner()
    pub(crate) fn get_final_item<'b>(&'b self, qual_name: &'b QualifiedName) -> &'b str {
        if let Some(alias) = self.unshadowing_aliases.get(qual_name) {
            // `p->alias::~alias()` is valid for a typedef name, whereas the
            // elaborated form the type would otherwise need is not.
            return &alias.alias;
        }
        match self.get(qual_name) {
            // Some(n) => match
            Some(n) => match n.get_final_segment_for_special_members() {
                Some(s) => s,
                None => qual_name.get_final_item(),
            },
            None => qual_name.get_final_item(),
        }
    }

    /// Convert a type to its C++ spelling.
    pub(crate) fn type_to_cpp(&self, ty: &Type) -> Result<String, ConvertErrorFromCpp> {
        match ty {
            Type::Path(typ) => {
                // If this is a std::unique_ptr we do need to pass
                // its argument through.
                let qual_name = QualifiedName::from_type_path(typ);
                let root = self.map(&qual_name);
                if root == "Pin" {
                    // Strip all Pins from type names when describing them in C++.
                    let inner_type = &typ.path.segments.last().unwrap().arguments;
                    if let syn::PathArguments::AngleBracketed(ab) = inner_type {
                        let inner_type = ab.args.iter().next().unwrap();
                        if let syn::GenericArgument::Type(gat) = inner_type {
                            return self.type_to_cpp(gat);
                        }
                    }
                    panic!("Pin<...> didn't contain the inner types we expected");
                }
                let suffix = match &typ.path.segments.last().unwrap().arguments {
                    syn::PathArguments::AngleBracketed(ab) => {
                        let results: Result<Vec<_>, _> = ab
                            .args
                            .iter()
                            .map(|x| match x {
                                syn::GenericArgument::Type(gat) => self.type_to_cpp(gat),
                                _ => Ok("".to_string()),
                            })
                            .collect();
                        Some(results?.join(", "))
                    }
                    syn::PathArguments::None | syn::PathArguments::Parenthesized(_) => None,
                };
                match suffix {
                    None => Ok(root),
                    Some(suffix) => Ok(format!("{root}<{suffix}>")),
                }
            }
            Type::Reference(typr) => match &*typr.elem {
                Type::Path(typ) if typ.path.is_ident("str") => Ok("rust::Str".into()),
                _ => Ok(format!(
                    "{}{}&",
                    get_mut_string(&typr.mutability),
                    self.type_to_cpp(typr.elem.as_ref())?
                )),
            },
            Type::Ptr(typp) => Ok(format!(
                "{}{}*",
                get_mut_string(&typp.mutability),
                self.type_to_cpp(typp.elem.as_ref())?
            )),
            Type::Array(_)
            | Type::BareFn(_)
            | Type::Group(_)
            | Type::ImplTrait(_)
            | Type::Infer(_)
            | Type::Macro(_)
            | Type::Never(_)
            | Type::Paren(_)
            | Type::Slice(_)
            | Type::TraitObject(_)
            | Type::Tuple(_)
            | Type::Verbatim(_) => Err(ConvertErrorFromCpp::UnsupportedType(
                ty.to_token_stream().to_string(),
            )),
            _ => Err(ConvertErrorFromCpp::UnknownType(
                ty.to_token_stream().to_string(),
            )),
        }
    }

    /// Check an individual item in the name map. Returns a thing if
    /// it's an inner type, otherwise returns none.
    pub(crate) fn get(&self, name: &QualifiedName) -> Option<&CppOriginalName> {
        self.original_names.get(name)
    }
}

/// The C++ name a type would have if we weren't renaming it to dodge a
/// variable of the same name. Free-standing so that it can be used while the
/// [`CppNameMap`] is still being assembled.
fn unaliased_cpp_name(
    original_names: &HashMap<QualifiedName, CppOriginalName>,
    qual_name: &QualifiedName,
) -> String {
    if let Some(cpp_name) = original_names.get(qual_name) {
        qual_name
            .get_namespace()
            .iter()
            .chain(once(cpp_name.for_original_name_map()))
            .join("::")
    } else {
        qual_name.to_cpp_name()
    }
}

fn get_mut_string(mutability: &Option<Token![mut]>) -> &'static str {
    match mutability {
        None => "const ",
        Some(_) => "",
    }
}
