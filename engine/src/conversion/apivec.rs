// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use indexmap::set::IndexSet as HashSet;

use crate::{
    conversion::{api::ApiName, convert_error::ErrorContext, ConvertErrorFromCpp},
    types::{make_ident, QualifiedName},
};

use super::api::{AnalysisPhase, Api};
use crate::conversion::convert_error::ErrorContextType;
use crate::minisyn::Ident;

/// What we append to a function's name to file the stub which records that a
/// type took that name away from it.
const DISPLACED_SUFFIX: &str = "_autocxx_hidden";

/// A type C++ can still name with an elaborated type specifier once something
/// hides it, and which is therefore worth keeping in preference to whatever
/// hid it.
///
/// A typedef is deliberately not one: a hidden typedef name cannot be spelled
/// at all, so keeping it would only produce C++ which doesn't compile.
fn is_unshadowable_type<P: AnalysisPhase>(api: &Api<P>) -> bool {
    matches!(
        api,
        Api::Struct { .. } | Api::Enum { .. } | Api::ForwardDeclaration { .. }
    )
}

/// Whether this is one of the stubs we file for a function a type displaced.
fn is_displaced_function<P: AnalysisPhase>(api: &Api<P>) -> bool {
    matches!(
        api,
        Api::IgnoredItem {
            err: ConvertErrorFromCpp::FunctionHiddenByType,
            ..
        }
    )
}

/// The name of the function such a stub stands for, which is the name it
/// answers to however many times we have had to re-file it.
fn displaced_function_name<P: AnalysisPhase>(stub: &Api<P>) -> Ident {
    match stub {
        Api::IgnoredItem { ctx: Some(ctx), .. } => match ctx.get_type() {
            ErrorContextType::SanitizedItem { lookup, .. } => lookup.clone(),
            other => panic!("a displaced function's stub had context {other:?}"),
        },
        _ => panic!("not a displaced function's stub"),
    }
}

/// Newtype wrapper for a list of APIs, which enforces the invariant
/// that each API has a unique name.
///
/// Specifically, each API should have a unique [`QualifiedName`] which is kept
/// within an [`ApiName`]. The [`QualifiedName`] is used to refer to this API
/// from others, e.g. to represent edges in the graph used for garbage collection,
/// so that's why this uniqueness is so important.
///
/// At present, this type also refuses to allow mutation of an API once it
/// has been added to a set. This is because the autocxx engine is
/// fundamentally organized into lots of analysis phases, each one _adding_
/// fields rather than mutating earlier fields. The idea here is that it's
/// impossible for stupid future maintainers (i.e. me) to make errors by
/// referring to fields before they're filled in. If a field exists, it's
/// correct.
///
/// While this is currently the case, it's possible that in future we could
/// see legitimate reasons to break this latter invariant and allow mutation
/// of APIs within an existing `ApiVec`. But it's extremely important that
/// the naming-uniqueness-invariant remains, so any such mutation should
/// allow mutation only of other fields, not the name.
#[derive(Debug)]
pub(crate) struct ApiVec<P: AnalysisPhase> {
    apis: Vec<Api<P>>,
    names: HashSet<QualifiedName>,
}

impl<P: AnalysisPhase> ApiVec<P> {
    pub(crate) fn push(&mut self, api: Api<P>) {
        let name = api.name().clone();
        if !self.already_contains(&name) {
            self.insert(api);
            return;
        }

        // The stub we file for a displaced function is documentation under a
        // name we invented, so whenever anything else wants that name, the
        // stub is the one that moves. That covers a C++ item which happens to
        // be called `foo_autocxx_hidden` arriving after us, and a stub of ours
        // arriving somewhere its name is already taken.
        if is_displaced_function(&api) {
            self.refile_displaced_function(&api);
            return;
        }
        if let Some(stub) = self.take_displaced_function(&name) {
            self.push(api);
            self.refile_displaced_function(&stub);
            return;
        }

        if api.discard_duplicates() {
            // This is already an IgnoredItem or something else where
            // we can silently drop it.
            log::info!("Discarding duplicate API for {}", name);
            return;
        }

        // C++ lets a function hide a type of the same name declared in the
        // same scope, just as a variable does, and `bindgen` reports both
        // under that one name. Only one of them can keep it, and it has to be
        // the type, since any other API we generate may depend on it -
        // including, quite possibly, the function's own signature.
        //
        // As it happens the type always arrives first, because
        // `ParseBindgen::parse_mod_items` pushes each struct as it walks the
        // mod and only hands over that mod's functions afterwards, in
        // `ParseForeignMod::finished`. Later phases rebuild an `ApiVec` by
        // pushing what an earlier one held, though, and synthesized functions
        // are appended as they are invented, so nothing here leans on that
        // ordering: whichever of the two turns up second, the type ends up
        // with the name.
        let incoming_hides_a_type =
            matches!(api, Api::Function { .. }) && self.contains_unshadowable_type(&name);
        let incoming_is_the_hidden_type =
            is_unshadowable_type(&api) && self.contains_function(&name);
        if incoming_hides_a_type || incoming_is_the_hidden_type {
            if incoming_is_the_hidden_type {
                self.retain(|stored| stored.name() != &name);
                self.insert(api);
            }
            self.record_displaced_function(&name);
            return;
        }

        log::info!(
            "Duplicate API for {} - removing all of them and replacing with an IgnoredItem.",
            name
        );
        self.retain(|api| api.name() != &name);
        self.push(Api::IgnoredItem {
            name: ApiName::new_from_qualified_name(name.clone()),
            err: ConvertErrorFromCpp::DuplicateItemsFoundInParsing,
            ctx: Some(ErrorContext::new_for_item(name.get_final_ident())),
        })
    }

    /// Add an API whose name we have already established is free.
    fn insert(&mut self, api: Api<P>) {
        self.names.insert(api.name().clone());
        self.apis.push(api);
    }

    fn already_contains(&self, name: &QualifiedName) -> bool {
        self.names.contains(name)
    }

    /// Note that a function has lost its name to a type of the same name.
    ///
    /// The function is not silently forgotten: it stays on as an
    /// `IgnoredItem` saying what happened to it, so that the output mod
    /// carries a documented stub for it. That item has to be filed under a
    /// name of our own, because the one the user knows it by is now the
    /// type's, but it still answers to the original name when `generate!`
    /// directives are matched - which is why a directive naming it is
    /// satisfied by the type rather than being reported as a failure. Giving
    /// the function a Rust name of its own would keep both, but that is a
    /// naming-policy decision rather than a fix for this clash.
    fn record_displaced_function(&mut self, hidden: &QualifiedName) {
        let lookup = hidden.get_final_ident();
        let filed_under = self.first_free_name(&QualifiedName::new(
            hidden.get_namespace(),
            make_ident(format!("{lookup}{DISPLACED_SUFFIX}")),
        ));
        log::info!(
            "Function {} is hidden by a type of the same name; recording it as {}",
            hidden,
            filed_under
        );
        let display = filed_under.get_final_ident();
        self.insert(Api::IgnoredItem {
            name: ApiName::new_from_qualified_name(filed_under),
            err: ConvertErrorFromCpp::FunctionHiddenByType,
            ctx: Some(ErrorContext::new_for_displaced_item(lookup, display)),
        });
    }

    /// File a stub we have already made afresh, which gives it a name nothing
    /// else has taken in the meantime.
    fn refile_displaced_function(&mut self, stub: &Api<P>) {
        self.record_displaced_function(&QualifiedName::new(
            stub.name().get_namespace(),
            displaced_function_name(stub),
        ));
    }

    /// Remove the displaced-function stub filed under this name, if that is
    /// what is there.
    fn take_displaced_function(&mut self, name: &QualifiedName) -> Option<Api<P>> {
        let index = self
            .apis
            .iter()
            .position(|api| api.name() == name && is_displaced_function(api))?;
        let stub = self.apis.remove(index);
        self.names.shift_remove(stub.name());
        Some(stub)
    }

    /// The first name in this family which nothing has claimed.
    fn first_free_name(&self, base: &QualifiedName) -> QualifiedName {
        if !self.already_contains(base) {
            return base.clone();
        }
        for counter in 1.. {
            let candidate = QualifiedName::new(
                base.get_namespace(),
                make_ident(format!("{}{counter}", base.get_final_item())),
            );
            if !self.already_contains(&candidate) {
                return candidate;
            }
        }
        unreachable!("an unbounded sequence of candidate names cannot run out")
    }

    /// Whether we already hold, under this name, a type which C++ can still
    /// name with an elaborated type specifier once something hides it - and
    /// which is therefore worth keeping in preference to the hiding
    /// declaration. See
    /// [`crate::conversion::codegen_cpp::type_to_cpp::UnshadowingAlias`].
    fn contains_unshadowable_type(&self, name: &QualifiedName) -> bool {
        self.apis
            .iter()
            .any(|api| api.name() == name && is_unshadowable_type(api))
    }

    fn contains_function(&self, name: &QualifiedName) -> bool {
        self.apis
            .iter()
            .any(|api| api.name() == name && matches!(api, Api::Function { .. }))
    }

    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn append(&mut self, more: &mut ApiVec<P>) {
        self.extend(more.apis.drain(..))
    }

    pub(crate) fn extend(&mut self, it: impl Iterator<Item = Api<P>>) {
        // Could be optimized in future
        for api in it {
            self.push(api)
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Api<P>> {
        self.apis.iter()
    }

    pub(crate) fn into_iter(self) -> impl Iterator<Item = Api<P>> {
        self.apis.into_iter()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.apis.is_empty()
    }

    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&Api<P>) -> bool,
    {
        self.apis.retain(f);
        self.names.clear();
        self.names
            .extend(self.apis.iter().map(|api| api.name()).cloned());
    }
}

impl<P: AnalysisPhase> Default for ApiVec<P> {
    fn default() -> Self {
        Self {
            apis: Default::default(),
            names: Default::default(),
        }
    }
}

impl<P: AnalysisPhase> FromIterator<Api<P>> for ApiVec<P> {
    fn from_iter<I: IntoIterator<Item = Api<P>>>(iter: I) -> Self {
        let mut this = ApiVec::new();
        for i in iter {
            // Could be optimized in future
            this.push(i);
        }
        this
    }
}

#[cfg(test)]
mod tests {
    use super::{is_displaced_function, ApiVec};
    use crate::conversion::api::{
        Api, ApiName, CppVisibility, FuncToConvert, NullPhase, Provenance, StructDetails,
    };
    use crate::conversion::convert_error::ErrorContextType;
    use crate::conversion::parse::CppRefQualifier;
    use crate::conversion::ConvertErrorFromCpp;
    use crate::types::QualifiedName;
    use syn::parse_quote;

    fn name(id: &str) -> ApiName {
        ApiName::new_from_qualified_name(QualifiedName::new_from_cpp_name(id))
    }

    fn struct_api(id: &str) -> Api<NullPhase> {
        let ident = quote::format_ident!("{}", id);
        Api::Struct {
            name: name(id),
            details: Box::new(StructDetails {
                item: parse_quote! { pub struct #ident { pub a: u32 } },
                has_rvalue_reference_fields: false,
            }),
            analysis: (),
        }
    }

    fn static_api(id: &str) -> Api<NullPhase> {
        Api::Static {
            name: name(id),
            cpp_ty: None,
        }
    }

    fn fn_api(id: &str) -> Api<NullPhase> {
        let ident = quote::format_ident!("{}", id);
        Api::Function {
            name: name(id),
            fun: Box::new(FuncToConvert {
                provenance: Provenance::Bindgen,
                ident: ident.into(),
                doc_attrs: Vec::new(),
                inputs: Default::default(),
                variadic: false,
                output: parse_quote! {},
                vis: parse_quote! { pub },
                virtualness: None,
                cpp_vis: CppVisibility::Public,
                special_member: None,
                original_name: None,
                self_ty: None,
                synthesized_this_type: None,
                add_to_trait: None,
                synthetic_cpp: None,
                is_deleted: None,
                ref_qualifier: CppRefQualifier::None,
            }),
            analysis: (),
        }
    }

    /// C++ lets a variable share a name with a type - `struct stat {...};
    /// extern struct stat stat;` - and bindgen calls both of them `stat`. Only
    /// one of them can keep the name, and it has to be the type, since
    /// anything else we generate may depend on it.
    ///
    /// This is why [`Api::discard_duplicates`] answers `true` for
    /// [`Api::Static`]; see the note there about why the type is always the
    /// one already in the vec.
    #[test]
    fn variable_clashing_with_type_yields_to_it() {
        let mut apis = ApiVec::new();
        apis.push(struct_api("stat"));
        apis.push(static_api("stat"));
        let survivors: Vec<_> = apis.iter().collect();
        assert_eq!(survivors.len(), 1);
        assert!(
            matches!(survivors[0], Api::Struct { .. }),
            "the type should have survived, but we kept {:?}",
            survivors[0]
        );
    }

    /// A function hides a type of the same name in C++ exactly as a variable
    /// does - `struct foo {...}; void foo();` - and the type has to win for the
    /// same reason. The function is not forgotten, though: it stays on as an
    /// `IgnoredItem` under a name of its own, so that we can say what became
    /// of it.
    #[test]
    fn function_clashing_with_type_yields_to_it() {
        let mut apis = ApiVec::new();
        apis.push(struct_api("foo"));
        apis.push(fn_api("foo"));
        let survivors: Vec<_> = apis.iter().collect();
        assert_eq!(survivors.len(), 2);
        assert!(
            matches!(survivors[0], Api::Struct { .. }),
            "the type should have kept the name, but we kept {:?}",
            survivors[0]
        );
        let Api::IgnoredItem {
            err: ConvertErrorFromCpp::FunctionHiddenByType,
            ctx: Some(ctx),
            ..
        } = survivors[1]
        else {
            panic!(
                "expected the function to survive as an ignored item, got {:?}",
                survivors[1]
            );
        };
        assert_eq!(survivors[1].name().to_cpp_name(), "foo_autocxx_hidden");
        // The stub goes under a name of its own, but the user still knows the
        // item as `foo`, so that is what their directives are matched against.
        assert!(matches!(
            ctx.get_type(),
            ErrorContextType::SanitizedItem { lookup, display }
                if lookup == "foo" && display == "foo_autocxx_hidden"
        ));
    }

    /// The parse phase happens to offer the type first, but nothing relies on
    /// that, so the other order has to reach the same place.
    #[test]
    fn type_arriving_after_the_function_still_wins() {
        let mut apis = ApiVec::new();
        apis.push(fn_api("foo"));
        apis.push(struct_api("foo"));
        let survivors: Vec<_> = apis.iter().collect();
        assert_eq!(survivors.len(), 2);
        assert!(
            matches!(survivors[0], Api::Struct { .. }),
            "the type should have taken the name, but we kept {:?}",
            survivors[0]
        );
        assert!(is_displaced_function(survivors[1]));
        assert_eq!(survivors[1].name().to_cpp_name(), "foo_autocxx_hidden");
    }

    /// `foo_autocxx_hidden` is a name a C++ author may perfectly well have
    /// used. If they got there first, the stub takes the next name along
    /// rather than being dropped on the floor.
    #[test]
    fn stub_gives_way_to_a_real_api_which_got_the_name_first() {
        let mut apis = ApiVec::new();
        apis.push(struct_api("foo_autocxx_hidden"));
        apis.push(struct_api("foo"));
        apis.push(fn_api("foo"));
        let survivors: Vec<_> = apis.iter().collect();
        assert_eq!(survivors.len(), 3);
        assert_eq!(survivors[0].name().to_cpp_name(), "foo_autocxx_hidden");
        assert!(matches!(survivors[0], Api::Struct { .. }));
        assert_eq!(survivors[1].name().to_cpp_name(), "foo");
        assert!(is_displaced_function(survivors[2]));
        assert_eq!(survivors[2].name().to_cpp_name(), "foo_autocxx_hidden1");
    }

    /// And if they get there second, the stub moves aside: it is documentation
    /// under a name we invented, and the real API is neither.
    #[test]
    fn stub_gives_way_to_a_real_api_which_arrives_later() {
        let mut apis = ApiVec::new();
        apis.push(struct_api("foo"));
        apis.push(fn_api("foo"));
        apis.push(struct_api("foo_autocxx_hidden"));
        let mut survivors: Vec<_> = apis.iter().collect();
        survivors.sort_by_key(|api| api.name().to_cpp_name());
        assert_eq!(survivors.len(), 3);
        assert_eq!(survivors[0].name().to_cpp_name(), "foo");
        assert_eq!(survivors[1].name().to_cpp_name(), "foo_autocxx_hidden");
        assert!(
            matches!(survivors[1], Api::Struct { .. }),
            "the real type should have the name it asked for, but we kept {:?}",
            survivors[1]
        );
        assert!(is_displaced_function(survivors[2]));
        assert_eq!(survivors[2].name().to_cpp_name(), "foo_autocxx_hidden1");
    }

    /// The concession above is only made to a type C++ can still name once it
    /// is hidden. Nothing can rescue a hidden typedef, so a clash with one is
    /// still reported rather than resolved.
    #[test]
    fn function_clashing_with_unrescuable_type_is_not_preferred() {
        let mut apis = ApiVec::new();
        apis.push(Api::Typedef {
            name: name("foo"),
            item: crate::conversion::api::TypedefKind::Use(Box::new(parse_quote! { u32 })),
            old_tyname: None,
            analysis: (),
        });
        apis.push(fn_api("foo"));
        let survivors: Vec<_> = apis.iter().collect();
        assert_eq!(survivors.len(), 1);
        assert!(matches!(survivors[0], Api::IgnoredItem { .. }));
    }

    /// Two APIs which genuinely can't be told apart still lose out to an
    /// `IgnoredItem`, so that we document the problem rather than picking one
    /// arbitrarily.
    #[test]
    fn indistinguishable_duplicates_become_ignored() {
        let mut apis = ApiVec::new();
        apis.push(struct_api("Bob"));
        apis.push(struct_api("Bob"));
        let survivors: Vec<_> = apis.iter().collect();
        assert_eq!(survivors.len(), 1);
        assert!(matches!(survivors[0], Api::IgnoredItem { .. }));
    }

    /// Two functions which really are duplicates of each other are still
    /// reported: the concession above is to a type, not to whichever function
    /// happened to arrive first.
    #[test]
    fn duplicate_functions_become_ignored() {
        let mut apis = ApiVec::new();
        apis.push(fn_api("foo"));
        apis.push(fn_api("foo"));
        let survivors: Vec<_> = apis.iter().collect();
        assert_eq!(survivors.len(), 1);
        assert!(matches!(survivors[0], Api::IgnoredItem { .. }));
    }
}
