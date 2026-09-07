// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{cell::RefCell, fmt::Display, panic::UnwindSafe, rc::Rc};

use crate::types::{make_ident, strip_bindgen_original_suffix, Namespace};
use crate::vendored_bindgen::callbacks::Virtualness;
use crate::vendored_bindgen::callbacks::{
    BaseClassInfo, BaseKind, DataMemberInfo, Deprecation, DiscoveredItem, DiscoveredItemId,
    Explicitness, MethodKind, SpecialMemberKind, Visibility,
};
use crate::vendored_bindgen::callbacks::{ItemInfo, ItemKind, ParseCallbacks, SourceLocation};
use crate::{conversion::CppEffectiveName, types::QualifiedName, RebuildDependencyRecorder};
use indexmap::IndexMap as HashMap;
use indexmap::IndexSet as HashSet;
use quote::quote;

/// Newtype wrapper for a C++ "original name"; that is, an annotation
/// derived from bindgen that this is the original name of the C++ item.
///
/// Keeping this apart from the Rust and cxx::bridge names is only worth
/// anything if a Rust identifier cannot become a C++ name without something
/// saying why it may, so each constructor here is named for where its string
/// comes from.
///
/// The usual source is bindgen's original-name callback
/// ([`AutocxxParseCallbacks::denote_cpp_name`], the only place a bare string
/// reaches the tuple field), and it is a thorough one: bindgen reports a name
/// for every function it emits, whether or not the identifier it chose
/// differs, and for every struct, union, enum and typedef unless that item or
/// one enclosing it is anonymous. The rest are autocxx's own names, for items
/// bindgen did not report: one minted for C++
/// ([`Self::from_final_item_of_generated_cpp_name`]), the name of a function
/// autocxx synthesized, which has no C++ original
/// ([`Self::from_function_without_a_reported_cpp_name`]), and the type name a
/// synthesized constructor has to be recognisable by
/// ([`Self::from_type_name_for_constructor`], which is not a C++ spelling at
/// all and says so). A general "wrap this string" constructor is deliberately
/// absent: there is no honest thing to call one.
#[derive(PartialEq, PartialOrd, Eq, Hash, Clone, Debug)]
pub struct CppOriginalName(String);

impl Display for CppOriginalName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl CppOriginalName {
    pub(crate) fn is_nested(&self) -> bool {
        self.0.contains("::")
    }

    /// The C++ name of an item autocxx names for C++ itself: a subclass's peer
    /// class, or the `_super` helper declared on one. autocxx holds such names
    /// in a [`QualifiedName`] because that is how it holds every name it
    /// generates, and the final segment is the part C++ sees - the namespace
    /// travels separately, in the `ApiName` this ends up in.
    pub(crate) fn from_final_item_of_generated_cpp_name(name: &QualifiedName) -> Self {
        Self(name.get_final_item().to_string())
    }

    pub(crate) fn to_qualified_name(&self) -> QualifiedName {
        QualifiedName::new_from_cpp_name(&self.0)
    }

    pub(crate) fn to_effective_name(&self) -> CppEffectiveName {
        CppEffectiveName(self.0.clone())
    }

    /// This is the main output of this type; it's fed into a mapping
    /// from <weird bindgen name format> to
    /// <sensible namespace::outer::inner format>; this contributes "inner".
    pub(crate) fn for_original_name_map(&self) -> &str {
        &self.0
    }

    /// Used to give the final part of the name which can be used
    /// to figure out the name for constructors, destructors etc.
    pub(crate) fn get_final_segment_for_special_members(&self) -> Option<&str> {
        self.0.rsplit_once("::").map(|(_, suffix)| suffix)
    }

    /// The name by which a synthesized constructor is recognised as one.
    ///
    /// `analyze_foreign_fn` asks bindgen which methods are constructors and
    /// falls back to the name for the functions bindgen never classified, which
    /// is what a synthesized one is. That fallback wants a name beginning with
    /// the name of the type constructed, so `synthesize_special_member` has to
    /// hand it one - which means the very string that check compares
    /// against: the C++ final segment for a nested type, and otherwise the
    /// identifier bindgen emitted for the type, which for a type C++ calls
    /// `type` is `type_`. This is therefore a Rust-side name as often as not,
    /// and nothing spells C++ from it: a constructor always gets a C++
    /// wrapper, whose `PlacementNew` body names the type from its
    /// `QualifiedName`, and needing that wrapper is also what suppresses the
    /// `#[cxx_name]` this would otherwise produce.
    pub(crate) fn from_type_name_for_constructor(name: String) -> Self {
        Self(name)
    }

    /// Work out what to call a Rust-side API given a C++-side name.
    pub(crate) fn to_string_for_rust_name(&self) -> String {
        self.0.clone()
    }

    /// Return the string inside for validation purposes.
    pub(crate) fn for_validation(&self) -> &str {
        &self.0
    }

    /// Used for diagnostics early in function analysis before we establish
    /// the correct naming.
    pub(crate) fn diagnostic_display_name(&self) -> &String {
        &self.0
    }

    /// The C++ name of a function for which bindgen reported none, which is
    /// the name autocxx knows it by.
    ///
    /// bindgen's function codegen passes an original name to
    /// [`ParseCallbacks::denote_cpp_name`] unconditionally, so a function with
    /// none is one bindgen never saw: across the whole integration suite every
    /// such function is `Provenance::SynthesizedOther` - a cast, an allocator
    /// or a special member autocxx filled in - and no such function has a C++
    /// original for this to be wrong about. Nothing spells C++ out of it for
    /// one of them either; the call site in `analyze_foreign_fn` says why, and
    /// says what the other route here, through `conversion_tests`, is.
    ///
    /// The assertion is what the newtype can still check: a C++ name may be
    /// qualified, as [`Self::is_nested`] tests for, and a name from the Rust
    /// side never is. A qualified one arriving here would mean a C++ spelling
    /// had been fabricated somewhere rather than reported.
    pub(crate) fn from_function_without_a_reported_cpp_name(name: &str) -> Self {
        debug_assert!(
            !name.contains("::"),
            "`{name}` is qualified, so it is a C++ spelling from somewhere and not a name autocxx gave a function of its own"
        );
        Self(name.to_string())
    }

    /// Determines whether we need to generate a cxxbridge::name attribute
    pub(crate) fn does_not_match_cxxbridge_name(
        &self,
        cxxbridge_name: &crate::minisyn::Ident,
    ) -> bool {
        cxxbridge_name.0 != self.0
    }

    pub(crate) fn generate_cxxbridge_name_attribute(&self) -> proc_macro2::TokenStream {
        let cpp_call_name = &self.to_string_for_rust_name();
        quote!(
            #[cxx_name = #cpp_call_name]
        )
    }
}

/// How many template parameters a C++ alias template declared, and how many of
/// them are template *type* parameters. They differ when C++ declared a
/// non-type or template template parameter: bindgen represents neither, and
/// emits the alias as a plain typedef, which has lost the parameters that
/// naming the alias in C++ still requires.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AliasTemplateParams {
    pub(crate) declared: usize,
    pub(crate) type_params: usize,
}

/// One C++ data member of a struct or union, as bindgen reported it.
///
/// bindgen reports every member, including the ones it generates no field of
/// their own type for: a run of bitfields shares one allocation unit, and each
/// member of it survives only as accessors over that unit. So this is the only
/// channel which says anything at all about a bitfield's own type.
#[derive(Debug, Clone)]
pub(crate) struct DataMember {
    /// The name of the Rust field bindgen generates for the member, or of the
    /// accessors where it is a bitfield. `None` for an unnamed bitfield.
    pub(crate) name: Option<String>,
    /// Whether C++ declared the member's own type `const`.
    pub(crate) is_const: bool,
    /// Whether the member is a bitfield, and so has no field of its own in the
    /// struct bindgen emitted.
    pub(crate) is_bitfield: bool,
    /// Whether the member has a default member initializer. A member whose
    /// initializer bindgen could not see counts as having none, which is the
    /// direction that costs a `new()` rather than one which C++ deletes.
    pub(crate) has_default_member_initializer: bool,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct NameAndParent {
    parent: DiscoveredItemId,
    name: String,
}

/// The base classes bindgen reported for one type.
#[derive(Debug, Default, Clone)]
pub(crate) struct BaseClasses {
    /// The bases whose identifier resolved to a name autocxx knows.
    pub(crate) named: Vec<BaseClass>,
    /// Whether any base did not. bindgen reports a base by the identifier of
    /// the item its type resolves to, and it announces no such item for a
    /// template instantiation, so `struct D : Base<int>` names nothing here.
    /// It is a base all the same, and a class with one is a class whose
    /// ancestry autocxx does not have.
    pub(crate) any_unnamed: bool,
    /// Whether any base, named or not, is inherited virtually.
    pub(crate) any_virtual: bool,
}

/// A base class of some type, as bindgen reported it.
#[derive(Debug, Clone)]
pub(crate) struct BaseClass {
    pub(crate) name: QualifiedName,
    /// Whether C++ inherits it virtually. Such a base sits at no fixed offset
    /// within the derived class, so the derived class's layout is not the
    /// layout of the fields bindgen emits for it.
    pub(crate) is_virtual: bool,
    pub(crate) is_public: bool,
}

/// A base class before its identifier has been turned back into a name.
#[derive(Debug, Clone)]
struct ReportedBase {
    base: DiscoveredItemId,
    is_virtual: bool,
    is_public: bool,
}

#[derive(Debug, Default, Clone)]
/// Information communicated to us from bindgen using its `ParseCallbacks`
/// mechanism.
///
/// The various accessor methods here return `None` if a
/// given `QualifiedName` can't be found, because bindgen only tells us
/// information when it actually has it.
pub(crate) struct UnindexedParseCallbackResults {
    original_names: HashMap<DiscoveredItemId, CppOriginalName>,
    virtuals: HashMap<DiscoveredItemId, Virtualness>,
    root_mod: Option<DiscoveredItemId>,
    visibility: HashMap<DiscoveredItemId, Visibility>,
    special_member_kinds: HashMap<DiscoveredItemId, SpecialMemberKind>,
    method_kinds: HashMap<DiscoveredItemId, MethodKind>,
    alias_templates: HashMap<DiscoveredItemId, AliasTemplateParams>,
    explicitness: HashMap<DiscoveredItemId, Explicitness>,
    discards_template_param: HashSet<DiscoveredItemId>,
    names: HashMap<DiscoveredItemId, String>,
    mods_for_items: HashMap<DiscoveredItemId, DiscoveredItemId>,
    bases: HashMap<DiscoveredItemId, Vec<ReportedBase>>,
    data_members: HashMap<DiscoveredItemId, Vec<DataMember>>,
    deprecations: HashMap<DiscoveredItemId, Deprecation>,
}

impl UnindexedParseCallbackResults {
    /// What bindgen would have reported for a mod hand-written in a test.
    /// Every name lookup starts from the root mod, so a
    /// [`Default::default()`] instance panics on the first one; nothing else
    /// is needed, because a lookup which finds no entry answers `None`, which
    /// is exactly "bindgen told us nothing about this name".
    #[cfg(test)]
    pub(crate) fn with_only_a_root_mod() -> Self {
        Self {
            root_mod: Some(DiscoveredItemId::new(0)),
            ..Default::default()
        }
    }

    pub(crate) fn index(self) -> ParseCallbackResults {
        let index = self
            .mods_for_items
            .iter()
            .filter_map(|(id, parent)| {
                self.names.get(id).map(|name| {
                    (
                        NameAndParent {
                            parent: *parent,
                            name: name.clone(),
                        },
                        *id,
                    )
                })
            })
            .collect();

        let bases = self
            .bases
            .iter()
            .filter_map(|(derived, reported)| {
                let mut bases = BaseClasses::default();
                for base in reported {
                    bases.any_virtual |= base.is_virtual;
                    match self.qualified_name(base.base) {
                        Some(name) => bases.named.push(BaseClass {
                            name,
                            is_virtual: base.is_virtual,
                            is_public: base.is_public,
                        }),
                        None => bases.any_unnamed = true,
                    }
                }
                Some((self.qualified_name(*derived)?, bases))
            })
            .collect();

        ParseCallbackResults {
            results: self,
            index,
            bases,
        }
    }

    /// The name autocxx knows an item by, from the name and the namespace
    /// bindgen reported for it - the inverse of
    /// [`ParseCallbackResults::id_by_name`], for the facts bindgen reports by
    /// identifier rather than by name.
    ///
    /// `None` for an item bindgen never named - a template instantiation, which
    /// it announces through no callback - and for one it named but never placed
    /// in a module, which is one it abandoned before emitting anything: a class
    /// with non-type template parameters, say.
    ///
    /// The walk terminates because each step moves to the enclosing module and
    /// the root module is reported with no parent at all.
    fn qualified_name(&self, id: DiscoveredItemId) -> Option<QualifiedName> {
        let name = self.names.get(&id)?;
        let root = self.root_mod?;
        let mut segments = Vec::new();
        let mut parent = *self.mods_for_items.get(&id)?;
        while parent != root {
            segments.push(self.names.get(&parent)?.clone());
            parent = *self.mods_for_items.get(&parent)?;
        }
        let ns = segments
            .into_iter()
            .rev()
            .fold(Namespace::new(), |ns, segment| ns.push(segment));
        Some(QualifiedName::new(&ns, make_ident(name)))
    }
}

/// A version of [`UnindexedParseCallbackResults`] with an index constructed
/// for efficient access.
pub(crate) struct ParseCallbackResults {
    results: UnindexedParseCallbackResults,
    index: HashMap<NameAndParent, DiscoveredItemId>,
    bases: HashMap<QualifiedName, BaseClasses>,
}

impl ParseCallbackResults {
    fn get_item_by_parentage(&self, search_key: &NameAndParent) -> Option<DiscoveredItemId> {
        self.index.get(search_key).cloned()
    }

    pub(crate) fn get_fn_original_name(&self, name: &QualifiedName) -> Option<CppOriginalName> {
        self.id_by_name(name)
            .and_then(|id| self.results.original_names.get(&id).cloned())
    }

    pub(crate) fn get_original_name(&self, name: &QualifiedName) -> Option<CppOriginalName> {
        self.id_by_name(name)
            .and_then(|id| self.results.original_names.get(&id).cloned())
    }

    pub(crate) fn get_virtualness(&self, name: &QualifiedName) -> Option<Virtualness> {
        self.id_by_name(name)
            .and_then(|id| self.results.virtuals.get(&id).cloned())
    }

    fn id_by_name(&self, name: &QualifiedName) -> Option<DiscoveredItemId> {
        self.mod_id_by_namespace(name.get_namespace())
            .and_then(|parent| {
                let search_key = NameAndParent {
                    parent,
                    name: name.get_final_item().to_string(),
                };
                self.get_item_by_parentage(&search_key)
            })
    }

    fn get_root_mod(&self) -> DiscoveredItemId {
        self.results
            .root_mod
            .expect("Root mod not yet reported by bindgen")
    }

    fn mod_id_by_namespace(&self, namespace: &Namespace) -> Option<DiscoveredItemId> {
        self.mod_id_by_inner_namespace(self.get_root_mod(), namespace.iter())
    }

    fn mod_id_by_inner_namespace<'a>(
        &self,
        parent: DiscoveredItemId,
        mut ns_iter: impl Iterator<Item = &'a str>,
    ) -> Option<DiscoveredItemId> {
        match ns_iter.next() {
            Some(child_mod_name) => {
                let search_key = NameAndParent {
                    parent,
                    name: child_mod_name.to_string(),
                };
                self.get_item_by_parentage(&search_key)
                    .and_then(|child_mod_id| self.mod_id_by_inner_namespace(child_mod_id, ns_iter))
            }
            None => Some(parent),
        }
    }

    pub(crate) fn get_cpp_visibility(&self, name: &QualifiedName) -> Visibility {
        self.id_by_name(name)
            .and_then(|id| self.results.visibility.get(&id).cloned())
            .unwrap_or(Visibility::Public)
    }

    pub(crate) fn special_member_kind(&self, name: &QualifiedName) -> Option<SpecialMemberKind> {
        self.id_by_name(name)
            .and_then(|id| self.results.special_member_kinds.get(&id).cloned())
    }

    /// What C++ says a method is. `None` means bindgen reported no kind for
    /// this function: it is either a free function, which has no method kind,
    /// or one autocxx synthesized and bindgen never saw.
    pub(crate) fn get_method_kind(&self, name: &QualifiedName) -> Option<MethodKind> {
        self.id_by_name(name)
            .and_then(|id| self.results.method_kinds.get(&id).cloned())
    }

    /// What C++ marked a function `[[deprecated]]` with, or `None` where it
    /// marked it not at all.
    ///
    /// bindgen reports the marker a function carries itself, so two things
    /// answer `None` here and are left doing what they did before autocxx read
    /// any of this: a deprecated *type*, which bindgen does not report at all,
    /// and a function which is deprecated only because a class or namespace
    /// enclosing it is. Naming either from the generated C++ still draws
    /// `-Wdeprecated-declarations`, which a `-Werror` build still fails on,
    /// and neither is marked on the Rust side. libclang answers for the
    /// declaration asked about and no other, so covering them means asking
    /// about the enclosing declaration - and, for a type, silencing every
    /// place the generated C++ names it rather than the one wrapper a function
    /// gets.
    pub(crate) fn get_deprecation(&self, name: &QualifiedName) -> Option<Deprecation> {
        self.id_by_name(name)
            .and_then(|id| self.results.deprecations.get(&id).cloned())
    }

    pub(crate) fn get_deleted_or_defaulted(&self, name: &QualifiedName) -> Option<Explicitness> {
        self.id_by_name(name)
            .and_then(|id| self.results.explicitness.get(&id).cloned())
    }

    /// `None` if bindgen did not report this item as an alias template, which
    /// includes every ordinary typedef.
    pub(crate) fn alias_template_params(
        &self,
        name: &QualifiedName,
    ) -> Option<AliasTemplateParams> {
        self.id_by_name(name)
            .and_then(|id| self.results.alias_templates.get(&id).cloned())
    }

    /// The C++ data members of a struct or union, in declaration order.
    pub(crate) fn data_members(&self, name: &QualifiedName) -> Option<&[DataMember]> {
        self.id_by_name(name)
            .and_then(|id| self.results.data_members.get(&id))
            .map(Vec::as_slice)
    }

    pub(crate) fn discards_template_param(&self, name: &QualifiedName) -> bool {
        self.id_by_name(name)
            .map(|id| self.results.discards_template_param.contains(&id))
            .unwrap_or_default()
    }

    /// Every base class bindgen reported for a type, including the ones it
    /// generates no field for: a base it finds zero-sized, and a virtual
    /// base, which the object reaches indirectly.
    ///
    /// `None` where bindgen reported nothing, which means either that the type
    /// has no bases or that it is one bindgen wrote no members for - an opaque
    /// type, which may have bases it describes nothing of.
    pub(crate) fn get_bases(&self, name: &QualifiedName) -> Option<&BaseClasses> {
        self.bases.get(name)
    }

    /// The types which inherit at least one base virtually, so are laid out
    /// differently from the fields bindgen shows for them.
    pub(crate) fn types_inheriting_virtually(&self) -> impl Iterator<Item = &QualifiedName> {
        self.bases
            .iter()
            .filter(|(_, bases)| bases.any_virtual)
            .map(|(name, _)| name)
    }

    /// The types bindgen reported any base class for at all.
    pub(crate) fn types_with_bases(&self) -> impl Iterator<Item = &QualifiedName> {
        self.bases
            .iter()
            .filter(|(_, bases)| !bases.named.is_empty() || bases.any_unnamed)
            .map(|(name, _)| name)
    }
}

#[derive(Debug)]
pub(crate) struct AutocxxParseCallbacks {
    pub(crate) rebuild_dependency_recorder: Option<Box<dyn RebuildDependencyRecorder>>,
    pub(crate) results: Rc<RefCell<UnindexedParseCallbackResults>>,
}

impl AutocxxParseCallbacks {
    pub(crate) fn new(
        rebuild_dependency_recorder: Option<Box<dyn RebuildDependencyRecorder>>,
        results: Rc<RefCell<UnindexedParseCallbackResults>>,
    ) -> Self {
        Self {
            rebuild_dependency_recorder,
            results,
        }
    }
}

impl UnwindSafe for AutocxxParseCallbacks {}

impl ParseCallbacks for AutocxxParseCallbacks {
    fn include_file(&self, filename: &str) {
        if let Some(rebuild_dependency_recorder) = &self.rebuild_dependency_recorder {
            rebuild_dependency_recorder.record_header_file_dependency(filename);
        }
    }

    fn generated_name_override(&self, _item_info: ItemInfo<'_>) -> Option<String> {
        // We rename all functions in the original bindgen mod because
        // we will generate alternative implementations instead. We still need
        // to retain the functions so that we can detect them as we
        // parse the bindgen output.
        // For free functions, this isn't necessary: we simply avoid
        // adding a 'use bindgen::root::some_function' in the output
        // namespace. But for methods, we have no way to avoid conflicts
        // if we generate an alternative implementation of a method
        // with a given name.
        match _item_info.kind {
            ItemKind::Function => Some(format!("{}_bindgen_original", _item_info.name)),
            _ => None,
        }
    }

    fn denote_cpp_name(
        &self,
        id: DiscoveredItemId,
        original_name: Option<&str>,
        namespace_mod: Option<DiscoveredItemId>,
    ) {
        let mut results = self.results.borrow_mut();
        if let Some(original_name) = original_name {
            // bindgen reports a function under the name
            // `generated_name_override` gave it, so the rename has to come off
            // again here. It comes off type names too, which is wrong for a
            // C++ class genuinely called `X_bindgen_original` - but the index
            // this feeds keys a renamed function and a same-named type
            // identically, so a type's entry has to survive being reached
            // through a function's name. See
            // `test_function_hidden_by_type_is_documented`.
            let original_name = strip_bindgen_original_suffix(original_name);
            results
                .original_names
                .insert(id, CppOriginalName(original_name.to_string()));
        }
        if let Some(namespace_mod) = namespace_mod {
            results.mods_for_items.insert(id, namespace_mod);
        }
    }

    fn denote_virtualness(&self, id: DiscoveredItemId, virtualness: Virtualness) {
        self.results.borrow_mut().virtuals.insert(id, virtualness);
    }

    fn new_item_found(
        &self,
        id: DiscoveredItemId,
        item: DiscoveredItem,
        // Where in the headers the item was written. autocxx locates items by
        // name and parentage instead - see `ParseCallbackResults::id_by_name` -
        // so it has no use for this.
        _source_location: Option<&SourceLocation>,
    ) {
        match item {
            // Only a function is renamed - `generated_name_override` above
            // does it, and autocxx knows one by the name with the suffix taken
            // off again - so only a function's name is stripped here. Doing it
            // to a type would file a C++ class genuinely called
            // `Widget_bindgen_original` under `Widget`, where it would answer
            // lookups meant for a different class and, once bases are reported
            // by name, invent inheritance between the two.
            DiscoveredItem::Function { final_name } => {
                let final_name = strip_bindgen_original_suffix(&final_name);
                self.results.borrow_mut().names.insert(id, final_name);
            }
            DiscoveredItem::Struct { final_name, .. }
            | DiscoveredItem::Enum { final_name, .. }
            | DiscoveredItem::Union { final_name, .. }
            | DiscoveredItem::Alias {
                alias_name: final_name,
                ..
            } => {
                self.results.borrow_mut().names.insert(id, final_name);
            }
            DiscoveredItem::Mod {
                final_name,
                parent_id,
            } => {
                let mut results = self.results.borrow_mut();
                results.names.insert(id, final_name);
                if let Some(parent_id) = parent_id {
                    results.mods_for_items.insert(id, parent_id);
                } else {
                    results.root_mod.replace(id);
                }
            }
            _ => {}
        }
    }

    fn denote_visibility(&self, id: DiscoveredItemId, visibility: Visibility) {
        if !matches!(visibility, Visibility::Public) {
            // Public is the default; no need to record
            self.results.borrow_mut().visibility.insert(id, visibility);
        }
    }

    fn denote_special_member(&self, id: DiscoveredItemId, kind: SpecialMemberKind) {
        self.results
            .borrow_mut()
            .special_member_kinds
            .insert(id, kind);
    }

    fn denote_explicit(&self, id: DiscoveredItemId, explicitness: Explicitness) {
        self.results
            .borrow_mut()
            .explicitness
            .insert(id, explicitness);
    }

    fn denote_method_kind(&self, id: DiscoveredItemId, kind: MethodKind) {
        self.results.borrow_mut().method_kinds.insert(id, kind);
    }

    fn denote_deprecation(&self, id: DiscoveredItemId, deprecation: &Deprecation) {
        self.results
            .borrow_mut()
            .deprecations
            .insert(id, deprecation.clone());
    }

    fn denote_alias_template(
        &self,
        id: DiscoveredItemId,
        declared_params: usize,
        type_params: usize,
    ) {
        self.results.borrow_mut().alias_templates.insert(
            id,
            AliasTemplateParams {
                declared: declared_params,
                type_params,
            },
        );
    }

    fn denote_discards_template_param(&self, id: DiscoveredItemId) {
        self.results.borrow_mut().discards_template_param.insert(id);
    }

    fn denote_base_class(&self, derived: DiscoveredItemId, base: BaseClassInfo<'_>) {
        self.results
            .borrow_mut()
            .bases
            .entry(derived)
            .or_default()
            .push(ReportedBase {
                base: base.base,
                is_virtual: matches!(base.kind, BaseKind::Virtual),
                is_public: matches!(base.visibility, Visibility::Public),
            });
    }

    fn denote_data_member(&self, parent: DiscoveredItemId, member: DataMemberInfo<'_>) {
        self.results
            .borrow_mut()
            .data_members
            .entry(parent)
            .or_default()
            .push(DataMember {
                name: member.name.map(str::to_string),
                is_const: member.is_const,
                is_bitfield: member.bitfield_width.is_some(),
                // bindgen answers `None` where it could not ask clang - a
                // member declared by a macro expansion - and its own
                // documentation says to read that as "assume the worst".
                has_default_member_initializer: member
                    .has_default_member_initializer
                    .unwrap_or(false),
            });
    }
}
