// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

mod bindgen_sanitizer;
mod fun_codegen;
mod function_wrapper_rs;
mod impl_item_creator;
mod lifetime;
mod namespace_organizer;
mod non_pod_struct;
pub(crate) mod unqualify;
mod utils;

use crate::vendored_bindgen::callbacks::SpecialMemberKind;
use crate::vendored_bindgen::callbacks::Visibility as CppVisibility;
use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;

use autocxx_parser::{ExternCppType, IncludeCppConfig, RustFun, UnsafePolicy};

use itertools::Itertools;
use proc_macro2::{Span, TokenStream};
use syn::{
    parse_quote, punctuated::Punctuated, token::Comma, Attribute, Expr, FnArg, ForeignItem,
    ForeignItemFn, Generics, Ident, ImplItem, Item, ItemForeignMod, ItemMod, TraitItem, Type,
    TypePath,
};
use utils::{find_output_mod_root, generate_cxx_use_stmt, generate_cxx_use_stmt_for_id};

use crate::{
    conversion::array_witness::{array_element_witnesses, witness_name},
    conversion::codegen_rs::unqualify::{unqualify_params, unqualify_ret_type, unqualify_type},
    minisyn::minisynize_punctuated,
    types::{make_ident, Namespace, QualifiedName},
};
use impl_item_creator::create_impl_items;

use self::{
    fun_codegen::gen_function,
    namespace_organizer::{HasNs, NamespaceEntries},
};

use super::{
    analysis::{
        bridge_type_names::BridgeTypeNames,
        doc_label::make_doc_attrs,
        fun::{FnKind, FnPhase, PodAndDepAnalysis, ReceiverMutability, SubclassAnalysis},
        pod::PodAnalysis,
        tdef::{resolve_typedefs, typedef_targets},
    },
    api::{
        AnalysisPhase, Api, ConstRefShim, CustomPtrShim, HolderSurface, SharedPtrShim,
        SubclassName, TypeKind, UniquePtrShim, VectorShim, WeakPtrShim, SUPER_FN_SUFFIX,
    },
    convert_error::ErrorContextType,
    derives::DeriveRequests,
    doc_attr::get_doc_attrs,
};
use super::{
    api::{Provenance, RustSubclassFnDetails, SuperclassMethod, TraitImplSignature},
    apivec::ApiVec,
    codegen_cpp::type_to_cpp::CppNameMap,
};
use super::{convert_error::ErrorContext, ConvertErrorFromCpp, RsCodegenInputs};
use quote::quote;

/// An entry which needs to go into an `impl` block for a given type.
struct ImplBlockDetails {
    item: ImplItem,
    ty: Type,
}

struct TraitImplBlockDetails {
    item: TraitItem,
    key: TraitImplSignature,
}

/// Names the `_supers` trait item by which a subclass calls each of these
/// methods' superclass implementations, in the same order as `methods`.
///
/// Ordinarily that's `foo_super` for `foo`, matching the peer class method it
/// forwards to. But a superclass may perfectly well have a method called
/// `foo_super` of its own, and then the `_methods` trait - which inherits from
/// `_supers` - would carry two items of that name and neither could be called.
/// So keep marking the name with `autocxx` until it's free of the superclass's
/// own methods and of the names we've already handed out.
///
/// Both places which generate these names work from the same list of methods
/// for a superclass, so they agree on the answer.
fn super_fn_names(methods: &[SuperclassMethod]) -> Vec<Ident> {
    let mut taken: HashSet<String> = methods.iter().map(|m| m.name.to_string()).collect();
    methods
        .iter()
        .map(|m| {
            let mut marks = String::new();
            let mut candidate = format!("{}{}{}", m.name, marks, SUPER_FN_SUFFIX);
            while taken.contains(&candidate) {
                marks.push_str("_autocxx");
                candidate = format!("{}{}{}", m.name, marks, SUPER_FN_SUFFIX);
            }
            taken.insert(candidate.clone());
            make_ident(candidate).0
        })
        .collect()
}

/// What one C++ superclass contributes to the `_methods` and `_supers` traits
/// named after it.
#[derive(Default)]
struct SuperclassTraitContents {
    /// The virtual methods a Rust subclass may override, in the order the
    /// traits list them.
    methods: Vec<SuperclassMethod>,
    /// Whether the superclass itself can implement those traits, which it can
    /// only do by forwarding each method to its own binding for that method.
    /// See <https://github.com/google/autocxx/issues/609>.
    superclass_implements_traits: bool,
}

/// The Rust names of the methods which codegen will really emit, grouped by
/// the type they're implemented on and limited to the types `wanted` asks
/// about. Analysis records a good many methods it then declines to make
/// callable - a protected one, say - so this is the only trustworthy answer to
/// "can generated code call `Type::method`?".
fn emitted_method_names(
    apis: &ApiVec<FnPhase>,
    wanted: impl Fn(&QualifiedName) -> bool,
) -> HashMap<&QualifiedName, HashSet<&str>> {
    let mut results: HashMap<&QualifiedName, HashSet<&str>> = HashMap::new();
    for api in apis.iter() {
        if let Api::Function { analysis, .. } = api {
            // Keep in step with the same test at the top of `gen_function`.
            if analysis.ignore_reason.is_err() || !analysis.externally_callable {
                continue;
            }
            if let FnKind::Method { impl_for, .. } = &analysis.kind {
                if wanted(impl_for) {
                    results
                        .entry(impl_for)
                        .or_default()
                        .insert(analysis.rust_name.as_str());
                }
            }
        }
    }
    results
}

fn get_string_items() -> Vec<Item> {
    [
        Item::Trait(parse_quote! {
            /// A trait to be implemented by any type that can be turned
            /// into a C++ string.
            /// This trait is generated once per autocxx FFI mod and each
            /// implementation is incompatible and separate, because each
            /// will use a function generated independently for each mod
            /// in order to do the actual conversion to a C++ string.
            pub trait ToCppString {
                /// Convert `self` into a C++ string in a [`cxx::UniquePtr`].
                fn into_cpp(self) -> cxx::UniquePtr<cxx::CxxString>;
            }
        }),
        // We can't just impl<T: AsRef<str>> ToCppString for T
        // because the compiler says that this trait could be implemented
        // in future for cxx::UniquePtr<cxx::CxxString>. Fair enough.
        Item::Impl(parse_quote! {
            impl ToCppString for &str {
                fn into_cpp(self) -> cxx::UniquePtr<cxx::CxxString> {
                    make_string(self)
                }
            }
        }),
        Item::Impl(parse_quote! {
            impl ToCppString for String {
                fn into_cpp(self) -> cxx::UniquePtr<cxx::CxxString> {
                    make_string(&self)
                }
            }
        }),
        Item::Impl(parse_quote! {
            impl ToCppString for &String {
                fn into_cpp(self) -> cxx::UniquePtr<cxx::CxxString> {
                    make_string(self)
                }
            }
        }),
        Item::Impl(parse_quote! {
            impl ToCppString for cxx::UniquePtr<cxx::CxxString> {
                fn into_cpp(self) -> cxx::UniquePtr<cxx::CxxString> {
                    self
                }
            }
        }),
    ]
    .to_vec()
}

/// Type which handles generation of Rust code.
/// In practice, much of the "generation" involves connecting together
/// existing lumps of code within the Api structures.
pub(crate) struct RsCodeGenerator<'a> {
    unsafe_policy: &'a UnsafePolicy,
    include_list: &'a [String],
    bindgen_mod: ItemMod,
    original_name_map: CppNameMap,
    config: &'a IncludeCppConfig,
    header_name: Option<String>,
    names_duplicated_by_bindgen: &'a HashSet<QualifiedName>,
    bridge_type_names: &'a BridgeTypeNames,
    derive_requests: &'a DeriveRequests,
}

impl<'a> RsCodeGenerator<'a> {
    /// Generate code for a set of APIs that was discovered during parsing.
    pub(crate) fn generate_rs_code(
        all_apis: ApiVec<FnPhase>,
        unsafe_policy: &'a UnsafePolicy,
        include_list: &'a [String],
        bindgen_mod: ItemMod,
        config: &'a IncludeCppConfig,
        header_name: Option<String>,
        inputs: &'a RsCodegenInputs<'a>,
    ) -> Vec<Item> {
        let c = Self {
            unsafe_policy,
            include_list,
            bindgen_mod,
            original_name_map: CppNameMap::new_from_apis(
                &all_apis,
                &inputs.parse_observations.shadowed_types,
            ),
            config,
            header_name,
            names_duplicated_by_bindgen: &inputs.parse_observations.names_duplicated_by_bindgen,
            bridge_type_names: inputs.bridge_type_names,
            derive_requests: inputs.derive_requests,
        };
        c.rs_codegen(all_apis)
    }

    fn rs_codegen(mut self, all_apis: ApiVec<FnPhase>) -> Vec<Item> {
        // ... and now let's start to generate the output code.
        // First off, when we generate structs we may need to add some methods
        // if they're superclasses.
        let methods_by_superclass = self.accumulate_superclass_methods(&all_apis);
        let peer_constructors = decide_peer_constructors(&all_apis, self.unsafe_policy);
        let non_pod_types = find_non_pod_types(&all_apis);
        let concrete_typedefs = find_concrete_typedefs(&all_apis);
        let types_with_no_rust_storage = find_types_with_no_rust_storage(&all_apis);
        let array_element_witnesses = array_element_witnesses(&all_apis);
        // Now let's generate the Rust code.
        let (rs_codegen_results_and_namespaces, additional_cpp_needs): (Vec<_>, Vec<_>) = all_apis
            .into_iter()
            .map(|api| {
                let more_cpp_needed = api.needs_cpp_codegen();
                let name = api.name().clone();
                let gen = self.generate_rs_for_api(
                    api,
                    &methods_by_superclass,
                    &peer_constructors,
                    &non_pod_types,
                    &concrete_typedefs,
                    &types_with_no_rust_storage,
                );
                ((name, gen), more_cpp_needed)
            })
            .unzip();
        // First, the hierarchy of mods containing lots of 'use' statements
        // and other items which are the final API exposed as 'ffi'.
        let mut output_mod_items = Self::generate_final_output_namespace(
            &rs_codegen_results_and_namespaces,
            !self.config.exclude_utilities(),
        );
        // Both of the above ('use' hierarchy and bindgen mod) are organized into
        // sub-mods by namespace. From here on, things are flat.
        let (_, rs_codegen_results): (Vec<_>, Vec<_>) =
            rs_codegen_results_and_namespaces.into_iter().unzip();
        let (extern_c_mod_items, extern_rust_mod_items, all_items, bridge_items): (
            Vec<_>,
            Vec<_>,
            Vec<_>,
            Vec<_>,
        ) = rs_codegen_results
            .into_iter()
            .map(|api| {
                (
                    api.extern_c_mod_items,
                    api.extern_rust_mod_items,
                    api.global_items,
                    api.bridge_items,
                )
            })
            .multiunzip();
        // Items for the [cxx::bridge] mod...
        let mut bridge_items: Vec<Item> = bridge_items.into_iter().flatten().collect();
        // Things to include in the "extern "C"" mod passed within the cxx::bridge
        let mut extern_c_mod_items: Vec<ForeignItem> =
            extern_c_mod_items.into_iter().flatten().collect();
        // The same for extern "Rust"
        let mut extern_rust_mod_items = extern_rust_mod_items.into_iter().flatten().collect();
        // And a list of global items to include at the top level.
        let mut all_items: Vec<Item> = all_items.into_iter().flatten().collect();
        // And finally any C++ we need to generate. And by "we" I mean autocxx not cxx.
        // A witness is C++ autocxx writes, so the bridge has to include the
        // header it is written into even where nothing else needed one.
        let has_additional_cpp_needs = additional_cpp_needs.into_iter().any(std::convert::identity)
            || !array_element_witnesses.is_empty();
        extern_c_mod_items.extend(self.build_include_foreign_items(has_additional_cpp_needs));
        // The by-value use which tells cxx an array element is trivially
        // movable. No Rust caller can reach one: the bridge mod is private and
        // no `use` re-exports them, so they appear in neither the API nor the
        // documentation. The C++ definition is an ordinary function at global
        // scope, and callable as one; it does nothing. See
        // `array_element_witnesses`.
        extern_c_mod_items.extend(array_element_witnesses.iter().map(|name| {
            let witness = make_ident(witness_name(self.config, name));
            let id = self.bridge_type_names.get(name);
            parse_quote! {
                #[doc(hidden)]
                fn #witness(_: #id);
            }
        }));
        // We will always create an extern "C" mod even if bindgen
        // didn't generate one, e.g. because it only generated types.
        // We still want cxx to know about those types.
        let mut extern_c_mod: ItemForeignMod = parse_quote!(
            extern "C++" {}
        );
        extern_c_mod.items.append(&mut extern_c_mod_items);
        bridge_items.push(Self::make_foreign_mod_unsafe(extern_c_mod));
        let mut extern_rust_mod: ItemForeignMod = parse_quote!(
            extern "Rust" {}
        );
        extern_rust_mod.items.append(&mut extern_rust_mod_items);
        bridge_items.push(Item::ForeignMod(extern_rust_mod));
        // The extensive use of parse_quote here could end up
        // being a performance bottleneck. If so, we might want
        // to set the 'contents' field of the ItemMod
        // structures directly.
        bindgen_sanitizer::collapse_colliding_type_names(
            &mut self.bindgen_mod,
            self.names_duplicated_by_bindgen,
        );
        bindgen_sanitizer::remove_unbound_type_aliases(&mut self.bindgen_mod);
        bindgen_sanitizer::remove_unwanted_defaults(&mut self.bindgen_mod);
        bindgen_sanitizer::simplify_bitfield_transmutes(&mut self.bindgen_mod);
        bindgen_sanitizer::add_requested_derives(&mut self.bindgen_mod, self.derive_requests);
        self.bindgen_mod.vis = parse_quote! {};
        // bindgen writes bare unsafe calls into the bodies of the `unsafe fn`s
        // it generates, which RFC 2585 forbids. `bindgen::Builder::
        // wrap_unsafe_ops` is the switch for that, but it cannot be used here:
        // it makes bindgen emit a wrapper around each extern fn, and our
        // `item_name` parse callback has already appended `_bindgen_original`
        // to the name that wrapper is derived from, so the wrapper arrives as
        // `create_bindgen_original_bindgen_original`.
        // `strip_bindgen_original_suffix` removes one suffix, the name still
        // does not match the method it belongs to, and static methods silently
        // become free functions (`test_conflicting_static_functions` catches
        // it). Until autocxx-bindgen stops double-applying the rename, allow
        // the lint here - over bindgen's output alone, not over the code
        // autocxx writes itself.
        self.bindgen_mod.attrs.push(parse_quote! {
            #[allow(unsafe_op_in_unsafe_fn)]
        });
        self.bindgen_mod.attrs.push(parse_quote! {
            #[doc = "A private mod containing the bindings generated by `bindgen`. Do not use the contents directly - the useful parts will be re-exported into the main FFI mod."]
        });
        all_items.push(Item::Mod(self.bindgen_mod));
        all_items.push(Item::Mod(parse_quote! {
            /// A private mod containing the bindings generated by [`cxx`]. Do not use the contents directly - the useful parts will be re-exported into the main FFI mod.
            #[cxx::bridge]
            mod cxxbridge {
                #(#bridge_items)*
            }
        }));

        all_items.push(Item::Use(parse_quote! {
            #[allow(unused_imports)]
            use bindgen::root;
        }));
        let ffi_mod_name = self.config.get_mod_name();
        all_items.push(Item::Use(parse_quote! {
            #[allow(unused_imports)]
            use super::#ffi_mod_name as output;
        }));
        all_items.append(&mut output_mod_items);
        all_items
    }

    fn accumulate_superclass_methods(
        &self,
        apis: &ApiVec<FnPhase>,
    ) -> HashMap<QualifiedName, SuperclassTraitContents> {
        let mut results: HashMap<QualifiedName, SuperclassTraitContents> = HashMap::new();
        results.extend(
            self.config
                .superclasses()
                .map(|sc| (QualifiedName::new_from_cpp_name(sc), Default::default())),
        );
        for api in apis.iter() {
            if let Api::SubclassTraitItem { details, .. } = api {
                if let Some(contents) = results.get_mut(&details.receiver) {
                    contents.methods.push(details.clone());
                }
            }
        }
        // The superclass can only implement its own traits by forwarding each
        // method to the binding it got for that method - so it needs one for
        // every method in the trait. Ask the functions which survived analysis
        // rather than guessing from an earlier phase: a protected virtual
        // method, for instance, is analyzed successfully and only then
        // withheld from codegen, and an impl calling a binding which isn't
        // there resolves back to the trait and recurses for ever.
        let emitted = emitted_method_names(apis, |ty| results.contains_key(ty));
        for (superclass, contents) in results.iter_mut() {
            let emitted = emitted.get(superclass);
            contents.superclass_implements_traits = contents.methods.iter().all(|method| {
                emitted.is_some_and(|emitted| emitted.contains(method.name.to_string().as_str()))
            });
        }
        results
    }

    fn make_foreign_mod_unsafe(ifm: ItemForeignMod) -> Item {
        // At the moment syn does not support outputting 'unsafe extern "C"' except in verbatim
        // items. See https://github.com/dtolnay/syn/pull/938
        Item::Verbatim(quote! {
            unsafe #ifm
        })
    }

    fn build_include_foreign_items(&self, has_additional_cpp_needs: bool) -> Vec<ForeignItem> {
        let extra_inclusion = if has_additional_cpp_needs {
            Some(self.header_name.clone().unwrap())
        } else {
            None
        };
        let chained = self.include_list.iter().chain(extra_inclusion.iter());
        chained
            .map(|inc| {
                ForeignItem::Macro(parse_quote! {
                    include!(#inc);
                })
            })
            .collect()
    }

    /// Generate the final output mod hierarchy which the user will actually
    /// interact with. This is mostly lots of 'use' statements.
    fn generate_final_output_namespace(
        input_items: &[(QualifiedName, RsCodegenResult)],
        include_string_trait: bool,
    ) -> Vec<Item> {
        let mut output_items = Vec::new();
        let ns_entries = NamespaceEntries::new(input_items);
        Self::append_child_output_namespace(&ns_entries, &mut output_items, include_string_trait);
        output_items
    }

    fn append_child_output_namespace(
        ns_entries: &NamespaceEntries<(QualifiedName, RsCodegenResult)>,
        output_items: &mut Vec<Item>,
        include_string_trait: bool,
    ) {
        for (_name, codegen) in ns_entries.entries() {
            output_items.extend(codegen.output_mod_items.iter().cloned());
        }

        let mut impl_entries_by_type: HashMap<_, Vec<_>> = HashMap::new();
        let mut trait_impl_entries_by_trait_and_ty: HashMap<_, Vec<_>> = HashMap::new();
        for item in ns_entries.entries() {
            if let Some(impl_entry) = &item.1.impl_entry {
                impl_entries_by_type
                    .entry(impl_entry.ty.clone())
                    .or_default()
                    .push(&impl_entry.item);
            }
            if let Some(trait_impl_entry) = &item.1.trait_impl_entry {
                trait_impl_entries_by_trait_and_ty
                    .entry(trait_impl_entry.key.clone())
                    .or_default()
                    .push(&trait_impl_entry.item);
            }
        }
        for (ty, entries) in impl_entries_by_type.into_iter() {
            output_items.push(Item::Impl(parse_quote! {
                impl #ty {
                    #(#entries)*
                }
            }))
        }
        for (key, entries) in trait_impl_entries_by_trait_and_ty.into_iter() {
            let unsafety = key.unsafety;
            let ty = key.ty;
            let trt = key.trait_signature;
            output_items.push(Item::Impl(parse_quote! {
                #unsafety impl #trt for #ty {
                    #(#entries)*
                }
            }))
        }

        for (child_name, child_ns_entries) in ns_entries.children() {
            if child_ns_entries.is_empty() {
                continue;
            }
            let child_id = make_ident(child_name);
            let mut new_mod: ItemMod = if include_string_trait {
                parse_quote!(
                    pub mod #child_id {
                        #[allow(unused_imports)]
                        use super::{cxxbridge, output, bindgen, ToCppString};
                    }
                )
            } else {
                parse_quote!(
                    pub mod #child_id {
                        #[allow(unused_imports)]
                        use super::{cxxbridge, output, bindgen};
                    }
                )
            };
            Self::append_child_output_namespace(
                child_ns_entries,
                &mut new_mod.content.as_mut().unwrap().1,
                include_string_trait,
            );
            output_items.push(Item::Mod(new_mod));
        }
    }

    fn id_to_expr(id: &Ident) -> Expr {
        parse_quote! { #id }
    }

    fn generate_rs_for_api(
        &self,
        api: Api<FnPhase>,
        associated_methods: &HashMap<QualifiedName, SuperclassTraitContents>,
        peer_constructors: &HashMap<QualifiedName, PeerConstructorImpl>,
        non_pod_types: &HashSet<QualifiedName>,
        concrete_typedefs: &HashMap<QualifiedName, QualifiedName>,
        types_with_no_rust_storage: &HashSet<QualifiedName>,
    ) -> RsCodegenResult {
        let name = api.name().clone();
        let id = name.get_final_ident();
        // What this type is called inside the bridge mod, which may differ
        // from its own name if another namespace holds a type of that name.
        let bridge_id = self.bridge_type_names.get(&name);
        match api {
            Api::StringConstructor { .. } => {
                let make_string_name = make_ident(self.config.get_makestring_name());
                RsCodegenResult {
                    extern_c_mod_items: vec![ForeignItem::Fn(parse_quote!(
                        /// Make a C++ [`cxx::UniquePtr`] to a [`cxx::CxxString`]
                        /// from a Rust `&str`.
                        fn #make_string_name(str_: &str) -> UniquePtr<CxxString>;
                    ))],
                    global_items: get_string_items(),
                    output_mod_items: vec![generate_cxx_use_stmt(
                        &name,
                        Some(&make_ident("make_string").0),
                    )],
                    ..Default::default()
                }
            }
            Api::Function { fun, analysis, .. } => gen_function(
                &name,
                *fun,
                analysis,
                non_pod_types,
                types_with_no_rust_storage,
                self.bridge_type_names,
            ),
            Api::Const { .. } => RsCodegenResult {
                output_mod_items: vec![Self::generate_bindgen_use_stmt(&name)],
                ..Default::default()
            },
            Api::Typedef { .. } => RsCodegenResult {
                output_mod_items: vec![match concrete_typedefs.get(&name) {
                    Some(target) => Self::generate_concrete_typedef(&name, target),
                    None => Self::generate_bindgen_use_stmt(&name),
                }],
                ..Default::default()
            },
            Api::Static { .. } => RsCodegenResult {
                output_mod_items: vec![Self::generate_static_use_stmt(&name)],
                ..Default::default()
            },
            Api::Struct {
                details,
                analysis:
                    PodAndDepAnalysis {
                        pod:
                            PodAnalysis {
                                num_generics, kind, ..
                            },
                        constructors,
                        ..
                    },
                ..
            } => {
                let mut doc_attrs = get_doc_attrs(&details.item.attrs);
                if constructors.abstract_without_virtual_destructor {
                    // Each container asks cxx for C++ which destroys the
                    // payload through a `T*`, and each in its own way.
                    // `std::unique_ptr<T>`'s deleter does `delete`.
                    // `std::shared_ptr<T>` is safe when it is handed ownership
                    // of a derived object, but cxx's `$raw` shim builds one
                    // from a `T*`, which installs a deleter doing the same
                    // `delete`. `std::vector<T>` destroys its elements, which
                    // for an abstract `T` can never exist - instantiating the
                    // destructor is enough to emit the call. `WeakPtr`
                    // destroys nothing itself and goes only because it is of
                    // no use without `SharedPtr`.
                    //
                    // Where the destructor really is non-virtual, the two
                    // smart-pointer paths perform the invalid deletion named
                    // at
                    // [`PublicConstructors::abstract_without_virtual_destructor`],
                    // and the vector, which can hold nothing to delete, still
                    // instantiates the code which would; the compiler
                    // diagnoses all three. Where the destructor is virtual
                    // through a base we could not see, none of it was wrong
                    // and nothing would have been said - this withdrawal is
                    // then a false alarm, which is the price of the safe
                    // direction.
                    //
                    // What comes back is only what autocxx adds of its own
                    // accord. cxx instantiates the same glue for any
                    // `std::unique_ptr<T>` a bound C++ signature names itself,
                    // and refusing those signatures - which would need a
                    // diagnostic of our own - is not done here.
                    doc_attrs.extend(make_doc_attrs(
                        "autocxx has not added its usual `UniquePtr`, `SharedPtr`, `WeakPtr` \
                         and `CxxVector` support for this type, because it is abstract and no \
                         virtual destructor was found for it. Every pointer to an abstract \
                         class points at an object of some derived class, so if the destructor \
                         is not virtual, destroying one through such a pointer runs the wrong \
                         one. Give the class a virtual destructor if C++ is meant to own one \
                         this way - or, if it has one through a base class autocxx was not \
                         asked to generate, add that base to your `generate!` list so that \
                         autocxx can see it. You can still call this type's methods via a \
                         reference or pointer obtained from C++. Note that a C++ function \
                         which itself returns or takes a `std::unique_ptr` of this type is \
                         still bound, and cxx still generates the deleting C++ for it, which \
                         your compiler may then refuse."
                            .to_string(),
                    ));
                }
                if constructors.destructor_inaccessible {
                    // Say so on the type itself, because for a type with no
                    // public constructor there'd otherwise be nothing at all
                    // in the output to explain why it can only be borrowed.
                    // See google/autocxx#829.
                    doc_attrs.extend(make_doc_attrs(
                        "autocxx has not generated any way for Rust to own one of these, \
                         because this type's C++ destructor is inaccessible (private, \
                         protected or deleted) and so Rust could never destroy one. \
                         You can still call its methods via a reference or pointer \
                         obtained from C++."
                            .to_string(),
                    ));
                } else {
                    // Otherwise, explain any constructor C++'s rules withheld,
                    // because a type which turns up with no `new()` is
                    // otherwise a mystery. See google/autocxx#1034. An
                    // inaccessible destructor withholds all of them at once,
                    // which is what the note above already says.
                    let why = &constructors.why_no_constructors;
                    for (member, why) in [
                        (
                            SpecialMemberKind::DefaultConstructor,
                            &why.default_constructor,
                        ),
                        (SpecialMemberKind::CopyConstructor, &why.copy_constructor),
                        (SpecialMemberKind::MoveConstructor, &why.move_constructor),
                    ] {
                        if let Some(why) = why {
                            doc_attrs.extend(make_doc_attrs(why.describe(member)));
                        }
                    }
                }
                // `destroyable` is the smart-pointer trio and `movable` the
                // vector; the note above says why both go.
                let deletable = !constructors.abstract_without_virtual_destructor;
                self.generate_type(
                    &name,
                    bridge_id,
                    kind,
                    constructors.move_constructor && deletable,
                    constructors.destructor && deletable,
                    || Some(Item::Struct(details.item.into())),
                    doc_attrs,
                    associated_methods,
                    num_generics,
                )
            }
            Api::Enum { item, .. } => {
                let doc_attrs = get_doc_attrs(&item.attrs);
                self.generate_type(
                    &name,
                    bridge_id,
                    TypeKind::Pod,
                    true,
                    true,
                    || Some(Item::Enum(item.into())),
                    doc_attrs,
                    associated_methods,
                    0,
                )
            }
            Api::ConcreteType {
                holder_surface,
                incomplete_argument,
                ..
            } => {
                // A template instantiation built on a type this header only
                // declares is a complete type - C++ will name one and pass
                // references to it - but destroying one needs whatever the
                // template did with that argument, which may be a
                // `std::unique_ptr` of it. The smart-pointer trio is where
                // cxx writes C++ which destroys the payload, so it is withheld
                // and said so; every other position which would destroy one is
                // refused during analysis. See
                // `ConvertErrorFromCpp::InstantiationOnIncompleteType`.
                let mut doc_attrs = Vec::new();
                if let Some(argument) = &incomplete_argument {
                    doc_attrs.extend(make_doc_attrs(format!(
                        "autocxx has not added its usual `UniquePtr`, `SharedPtr` and `WeakPtr` \
                         support for this type, because it is a template instantiation whose \
                         argument `{}` is a type this header only declares. Each of those asks \
                         C++ to destroy one, and destroying a template instantiation can need \
                         its argument to be complete. You can still reach one through a \
                         reference or a pointer obtained from C++. Define `{}` where autocxx can \
                         see it if you need to own one.",
                        argument.to_cpp_name(),
                        argument.to_cpp_name(),
                    )));
                }
                let mut result = self.generate_type(
                    &name,
                    bridge_id.clone(),
                    TypeKind::Abstract,
                    false, // assume for now that these types can't be kept in a Vector
                    incomplete_argument.is_none(),
                    || None,
                    doc_attrs,
                    associated_methods,
                    0,
                );
                match holder_surface {
                    Some(HolderSurface::SharedPtr { payload, .. }) => {
                        self.generate_shared_ptr_surface(&name, &bridge_id, &payload, &mut result)
                    }
                    Some(HolderSurface::UniquePtr { payload, .. }) => {
                        self.generate_unique_ptr_surface(&name, &bridge_id, &payload, &mut result)
                    }
                    Some(HolderSurface::WeakPtr { shared_holder, .. }) => self
                        .generate_weak_ptr_surface(&name, &bridge_id, &shared_holder, &mut result),
                    Some(HolderSurface::VectorOfPointers { element, .. }) => {
                        self.generate_vector_surface(&name, &bridge_id, &element, &mut result)
                    }
                    Some(HolderSurface::CustomPtr {
                        payload,
                        payload_is_const,
                        ..
                    }) => self.generate_custom_ptr_surface(
                        &name,
                        &bridge_id,
                        &payload,
                        payload_is_const,
                        &mut result,
                    ),
                    Some(HolderSurface::ConstRef { payload, .. }) => {
                        self.generate_const_ref_surface(&name, &bridge_id, &payload, &mut result)
                    }
                    None => {}
                }
                result
            }
            Api::ForwardDeclaration { .. } | Api::OpaqueTypedef { .. } => self.generate_type(
                &name,
                bridge_id,
                TypeKind::Abstract,
                false, // these types can't be kept in a Vector
                false, // these types can't be put in a smart pointer
                || None,
                Vec::new(),
                associated_methods,
                0,
            ),
            Api::CType { .. } => RsCodegenResult {
                extern_c_mod_items: vec![ForeignItem::Verbatim(quote! {
                    type #id = autocxx::#id;
                })],
                ..Default::default()
            },
            Api::RustType { path, .. } => {
                let id = path.get_final_ident();
                RsCodegenResult {
                    global_items: vec![parse_quote! {
                        use super::#path;
                    }],
                    extern_rust_mod_items: vec![parse_quote! {
                        type #id;
                    }],
                    ..Default::default()
                }
            }
            Api::RustFn {
                details:
                    RustFun {
                        path,
                        mut sig,
                        has_receiver,
                        ..
                    },
                ..
            } => {
                sig.inputs = unqualify_params(sig.inputs, self.bridge_type_names);
                sig.output = unqualify_ret_type(sig.output, self.bridge_type_names);
                RsCodegenResult {
                    global_items: if !has_receiver {
                        vec![parse_quote! {
                            use super::#path;
                        }]
                    } else {
                        Vec::new()
                    },
                    extern_rust_mod_items: vec![parse_quote! {
                        #sig;
                    }],
                    ..Default::default()
                }
            }
            Api::RustSubclassFn {
                details, subclass, ..
            } => self.generate_subclass_fn(id.into(), *details, subclass),
            Api::Subclass {
                name,
                superclass,
                analysis:
                    SubclassAnalysis {
                        superclass_destructor_visibility,
                        superclass_destructor_virtual,
                    },
            } => {
                let methods = associated_methods.get(&superclass).map(|c| &c.methods);
                // A subclass with no synthesized constructor at all - nothing
                // reached `decide_peer_constructors` for it - has no generated
                // impl either.
                let peer_constructor = peer_constructors
                    .get(&name.0.name)
                    .copied()
                    .unwrap_or(PeerConstructorImpl::LeftToAuthor);
                self.generate_subclass(
                    name,
                    &superclass,
                    superclass_destructor_visibility,
                    superclass_destructor_virtual,
                    methods,
                    peer_constructor,
                )
            }
            Api::ExternCppType {
                details: ExternCppType { rust_path, .. },
                ..
            } => self.generate_extern_cpp_type(&name, rust_path),
            Api::IgnoredItem {
                err,
                ctx: Some(ctx),
                ..
            } => Self::generate_error_entry(err, ctx),
            Api::IgnoredItem { .. } | Api::SubclassTraitItem { .. } => RsCodegenResult::default(),
        }
    }

    fn generate_subclass(
        &self,
        sub: SubclassName,
        superclass: &QualifiedName,
        superclass_destructor_visibility: Option<CppVisibility>,
        superclass_destructor_virtual: bool,
        methods: Option<&Vec<SuperclassMethod>>,
        peer_constructor: PeerConstructorImpl,
    ) -> RsCodegenResult {
        let super_name = superclass.get_final_item();
        let super_path = superclass.to_type_path();
        // The superclass is declared in the bridge under whatever name it was
        // allocated there, which is not its own if something else - another
        // namespace's class of the same name, or cxx's own vocabulary - got
        // there first. Everything below refers to the type, so it has to use
        // that name; the accessors named after it are function names and keep
        // the C++ spelling, which is what the C++ side generates.
        let super_cxxxbridge_id = self.bridge_type_names.get(superclass);
        let id = sub.id();
        let holder = sub.holder();
        let full_cpp = sub.cpp();
        let cpp_path = full_cpp.to_type_path();
        let cpp_id = full_cpp.get_final_ident();
        let mut global_items = Vec::new();
        let relinquish_ownership_call = sub.cpp_remove_ownership();
        // Said about the peer type, because it is the peer's constructor which
        // the impl would have had to call.
        let peer_type_docs: Vec<Attribute> = match peer_constructor {
            PeerConstructorImpl::LeftToAuthorBecauseFallible => {
                let note = format!(
                    "autocxx has not written a `CppPeerConstructor` implementation for this \
                     subclass, because a `throws!` directive names `{cpp_id}`'s constructor: it \
                     hands back a `Result`, which is not what `make_peer` returns. Write the \
                     implementation, with `try_make_peer` calling `{cpp_id}::new` and `make_peer` \
                     deciding what to do about an exception."
                );
                vec![parse_quote! { #[doc = #note] }]
            }
            PeerConstructorImpl::Generated | PeerConstructorImpl::LeftToAuthor => Vec::new(),
        };
        let mut output_mod_items: Vec<Item> = vec![
            parse_quote! {
                #(#peer_type_docs)*
                pub use cxxbridge::#cpp_id;
            },
            parse_quote! {
                pub struct #holder(pub autocxx::subclass::CppSubclassRustPeerHolder<super::#id>);
            },
            parse_quote! {
                impl autocxx::subclass::CppSubclassCppPeer for #cpp_id {
                    fn relinquish_ownership(&self) {
                        self.#relinquish_ownership_call();
                    }
                }
            },
        ];
        let mut extern_c_mod_items = vec![
            self.generate_cxxbridge_type(&full_cpp, false, Vec::new()),
            parse_quote! {
                fn #relinquish_ownership_call(self: &#cpp_id);
            },
        ];
        if let Some(methods) = methods {
            let supers = SubclassName::get_supers_trait_name(superclass).to_type_path();
            let methods_impls: Vec<ImplItem> = methods
                .iter()
                .zip(super_fn_names(methods))
                .filter(|(m, _)| m.has_super_helper)
                .map(|(m, trait_super_method_name)| {
                    let peer_super_method_name =
                        SubclassName::get_super_fn_name(&Namespace::new(), &m.name.to_string())
                            .get_final_ident();
                    // The same shape as the trait item this implements, so
                    // that whatever the trait says a parameter is, the peer's
                    // own `_super` method is handed exactly that - and
                    // whatever the trait says the result is, this produces.
                    let (params, param_names, ret) = Self::superclass_trait_method_signature(m);
                    let peer_fn = make_ident(match m.receiver_mutability {
                        ReceiverMutability::Const => "peer",
                        ReceiverMutability::Mutable => "peer_mut",
                    });
                    let unsafe_token = m.requires_unsafe.wrapper_token();
                    // Under the policy which wraps references the peer's
                    // methods take their receiver as a wrapper, like any other
                    // C++ reference.
                    //
                    // SAFETY: `Pin::into_inner_unchecked` asks that the peer not
                    // be moved out of. It isn't: the pointer goes straight to
                    // C++, which holds the peer as `*this`.
                    let receiver: Expr = match (self.unsafe_policy, m.receiver_mutability) {
                        (
                            UnsafePolicy::ReferencesWrappedAllFunctionsSafe,
                            ReceiverMutability::Const,
                        ) => {
                            parse_quote!(autocxx::CppRef::from_ptr(self.#peer_fn()))
                        }
                        (
                            UnsafePolicy::ReferencesWrappedAllFunctionsSafe,
                            ReceiverMutability::Mutable,
                        ) => {
                            parse_quote!(autocxx::CppMutRef::from_ptr(unsafe {
                                ::core::pin::Pin::into_inner_unchecked(self.#peer_fn())
                            } as *mut #cpp_path))
                        }
                        _ => parse_quote!(self.#peer_fn()),
                    };
                    let call = Self::binding_call_as_trait_return(
                        m,
                        parse_quote!( #receiver.#peer_super_method_name(#param_names) ),
                    );
                    parse_quote! {
                        #unsafe_token fn #trait_super_method_name(#params) #ret {
                            use autocxx::subclass::CppSubclass;
                            #call
                        }
                    }
                })
                .collect();
            if !methods_impls.is_empty() {
                output_mod_items.push(parse_quote! {
                    #[allow(non_snake_case)]
                    impl #supers for super::#id {
                        #(#methods_impls)*
                    }
                });
            }
        }
        if matches!(peer_constructor, PeerConstructorImpl::Generated) {
            // The peer's `new` allocates in C++ and hands back the pointer, so
            // this is the whole body - see `find_types_with_no_rust_storage`.
            output_mod_items.push(parse_quote! {
                impl autocxx::subclass::CppPeerConstructor<#cpp_id> for super::#id {
                    fn make_peer(&mut self, peer_holder: autocxx::subclass::CppSubclassRustPeerHolder<Self>) -> cxx::UniquePtr<#cpp_path> {
                        #cpp_id :: new(peer_holder)
                    }
                }
            })
        };

        // Once for each superclass, in future...
        let as_id = make_ident(format!("As_{super_name}"));
        extern_c_mod_items.push(parse_quote! {
            fn #as_id(self: &#cpp_id) -> &#super_cxxxbridge_id;
        });
        let as_mut_id = make_ident(format!("As_{super_name}_mut"));
        extern_c_mod_items.push(parse_quote! {
            fn #as_mut_id(self: Pin<&mut #cpp_id>) -> Pin<&mut #super_cxxxbridge_id>;
        });
        output_mod_items.push(parse_quote! {
            impl AsRef<#super_path> for super::#id {
                fn as_ref(&self) -> &cxxbridge::#super_cxxxbridge_id {
                    use autocxx::subclass::CppSubclass;
                    self.peer().#as_id()
                }
            }
        });
        // TODO it would be nice to impl AsMut here but pin prevents us
        output_mod_items.push(parse_quote! {
            impl super::#id {
                pub fn pin_mut(&mut self) -> ::core::pin::Pin<&mut cxxbridge::#super_cxxxbridge_id> {
                    use autocxx::subclass::CppSubclass;
                    self.peer_mut().#as_mut_id()
                }
            }
        });
        // Only a superclass with a public destructor can live in a
        // `std::unique_ptr`, so this conversion exists only for those, and the
        // destructor has to be virtual besides: what the `std::unique_ptr`
        // this hands out owns is a peer, never a plain superclass, so
        // `delete`ing it through a superclass pointer whose destructor is not
        // virtual runs the wrong one. The C++ side agrees; see
        // `CppCodeGenerator::generate_subclass`.
        if superclass_destructor_virtual
            && matches!(
                superclass_destructor_visibility,
                Some(CppVisibility::Public)
            )
        {
            let as_unique_ptr_id = make_ident(format!("{cpp_id}_As_{super_name}_UniquePtr"));
            extern_c_mod_items.push(parse_quote! {
                fn #as_unique_ptr_id(u: UniquePtr<#cpp_id>) -> UniquePtr<#super_cxxxbridge_id>;
            });
            let rs_as_unique_ptr_id = make_ident(format!("as_{super_name}_unique_ptr"));
            output_mod_items.push(parse_quote! {
                impl super::#id {
                    pub fn #rs_as_unique_ptr_id(u: cxx::UniquePtr<#cpp_id>) -> cxx::UniquePtr<cxxbridge::#super_cxxxbridge_id> {
                        cxxbridge::#as_unique_ptr_id(u)
                    }
                }
            });
        }
        let remove_ownership = sub.remove_ownership();
        global_items.push(parse_quote! {
            #[allow(non_snake_case)]
            pub fn #remove_ownership(me: Box<#holder>) -> Box<#holder> {
                Box::new(#holder(me.0.relinquish_ownership()))
            }
        });
        RsCodegenResult {
            extern_c_mod_items,
            // For now we just assume we can't keep subclasses in vectors, but we can put them in
            // smart pointers.
            // That's the reason for the 'false' and 'true'
            bridge_items: create_impl_items(&cpp_id, false, true, self.config),
            output_mod_items,
            global_items,
            extern_rust_mod_items: vec![
                parse_quote! {
                    pub type #holder;
                },
                parse_quote! {
                    fn #remove_ownership(me: Box<#holder>) -> Box<#holder>;
                },
            ],
            ..Default::default()
        }
    }

    fn generate_subclass_fn(
        &self,
        api_name: Ident,
        details: RustSubclassFnDetails,
        subclass: SubclassName,
    ) -> RsCodegenResult {
        let params = details.params;
        let ret = details.ret;
        // cxx refuses an `extern "Rust"` function with a raw pointer parameter
        // unless it is declared unsafe, and under
        // `ReferencesWrappedAllFunctionsSafe` every C++ reference this method
        // takes is one. The Rust definition has to match the declaration, so
        // both read from here; the `_methods` trait item this forwards to is
        // still safe, because by then each pointer is back inside a `CppRef`.
        let unsafe_token = details.requires_unsafe.wrapper_token().or_else(|| {
            params
                .iter()
                .any(|param| matches!(&param.0, FnArg::Typed(pt) if matches!(*pt.ty, Type::Ptr(_))))
                .then(|| parse_quote! { unsafe })
        });
        let global_def = quote! { #unsafe_token fn #api_name(#params) #ret };
        let params = unqualify_params(minisynize_punctuated(params), self.bridge_type_names);
        let ret = unqualify_ret_type(ret.into(), self.bridge_type_names);
        let method_name = details.method_name;
        let cxxbridge_decl: ForeignItemFn =
            parse_quote! { #unsafe_token fn #api_name(#params) #ret; };
        // The trait implemented by the Rust subclass sees each parameter the
        // way Rust would rather have it, so undo whatever the Rust-calls-C++
        // direction did to it on the way past.
        let args: Punctuated<Expr, Comma> = Self::args_from_sig(&cxxbridge_decl.sig.inputs)
            .zip(details.cpp_impl.argument_conversion.iter())
            .map(
                |(arg, conversion)| match conversion.inverse_rust_conversion() {
                    Some((_, wrapper)) => parse_quote! { #wrapper::from_ptr(#arg) },
                    None => arg,
                },
            )
            .collect();
        // And the same for what it hands back. The bridge returns the pointer
        // cxx requires; the trait item speaks in wrappers, so unwrap what it
        // gives us. `as_ptr`/`as_mut_ptr` are safe: no reference is created,
        // and the pointer's onward journey is into C++, where the returned
        // reference has to outlive the call by C++'s own rules for a virtual
        // method - which this wrapper neither strengthens nor weakens.
        let return_unwrap = details
            .cpp_impl
            .return_conversion
            .as_ref()
            .and_then(|conversion| conversion.inverse_rust_return_conversion())
            .map(|(_, unwrap)| unwrap);
        let superclass_id = details.superclass.get_final_ident();
        let methods_trait = SubclassName::get_methods_trait_name(&details.superclass);
        let methods_trait = methods_trait.to_type_path();
        let (deref_ty, deref_call, borrow, mut_token) = match details.receiver_mutability {
            ReceiverMutability::Const => ("Deref", "deref", "try_borrow", None),
            ReceiverMutability::Mutable => (
                "DerefMut",
                "deref_mut",
                "try_borrow_mut",
                Some(syn::token::Mut(Span::call_site())),
            ),
        };
        let deref_ty = make_ident(deref_ty);
        let deref_call = make_ident(deref_call);
        let borrow = make_ident(borrow);
        let destroy_panic_msg = format!("Rust subclass API (method {} of subclass {} of superclass {}) called after subclass destroyed", method_name, subclass.0.name, superclass_id);
        let reentrancy_panic_msg = format!("Rust subclass API (method {} of subclass {} of superclass {}) called whilst subclass already borrowed - likely a re-entrant call",  method_name, subclass.0.name, superclass_id);
        let call: Expr = parse_quote! {
            #methods_trait :: #method_name (r, #args)
        };
        let call: Expr = match return_unwrap {
            Some(unwrap) => parse_quote!( #call.#unwrap() ),
            None => call,
        };
        RsCodegenResult {
            global_items: vec![parse_quote! {
                #global_def {
                    let rc = me.0
                        .get()
                        .expect(#destroy_panic_msg);
                    let #mut_token b = rc
                        .as_ref()
                        .#borrow()
                        .expect(#reentrancy_panic_msg);
                    let r = ::core::ops::#deref_ty::#deref_call(& #mut_token b);
                    #call
                }
            }],
            extern_rust_mod_items: vec![ForeignItem::Fn(cxxbridge_decl)],
            ..Default::default()
        }
    }

    fn args_from_sig(params: &Punctuated<FnArg, Comma>) -> impl Iterator<Item = Expr> + '_ {
        params.iter().skip(1).filter_map(|fnarg| match fnarg {
            syn::FnArg::Receiver(_) => None,
            syn::FnArg::Typed(fnarg) => match &*fnarg.pat {
                syn::Pat::Ident(id) => Some(Self::id_to_expr(&id.ident)),
                _ => None,
            },
        })
    }

    /// Declare the three C++ helpers of a `std::shared_ptr<const T>` holder in
    /// the bridge, and put a method for each on the holder itself.
    ///
    /// The holder is opaque, so these are what make it usable at all: without
    /// them a caller could receive one and hand it back to C++, and nothing
    /// else. They are written here rather than synthesized as `Api::Function`s
    /// because the holder is manufactured during function analysis, which is
    /// over by the time such a function could be analysed - see
    /// `FnAnalyzer::analyze_functions`, whose extra APIs are added with
    /// `add_analysis` and so can only be types.
    ///
    /// See google/autocxx#799.
    fn generate_shared_ptr_surface(
        &self,
        name: &QualifiedName,
        bridge_id: &crate::minisyn::Ident,
        payload: &Type,
        result: &mut RsCodegenResult,
    ) {
        // The bridge mod has a flat namespace and spells cxx's own types
        // unqualified, so every type in a declaration there needs the same
        // treatment a function signature gets. The output mod, where the
        // methods go, uses the qualified spellings instead.
        let holder = name.get_final_ident();
        let bridge_payload = unqualify_type(payload.clone(), self.bridge_type_names);
        let wrapped = matches!(
            self.unsafe_policy,
            UnsafePolicy::ReferencesWrappedAllFunctionsSafe
        );
        let mut methods: Vec<ImplItem> = Vec::new();
        for shim in SharedPtrShim::ALL {
            let shim_id = make_ident(shim.cpp_name(name));
            let method_id = make_ident(shim.rust_name());
            // A `CppRef` is the one return here which safe code may go on to
            // *dereference*: under this policy it is what a C++ `const T&`
            // parameter takes, and the generated C++ wrapper turns it back
            // into a reference with `(*p)` - no `unsafe` anywhere in the
            // caller. Every other function which hands one out got it from a
            // C++ function returning a real reference; `std::shared_ptr::get`
            // is documented to return null, and with the aliasing constructor
            // may return a pointer this holder does not keep alive. So this
            // one method is `unsafe`, and its safety comment is where the
            // caller vouches for what the C++ header would have promised.
            let unsafety: Option<syn::token::Unsafe> =
                matches!(shim, SharedPtrShim::Get if wrapped).then(|| parse_quote! { unsafe });
            let (bridge_ret, method_ret, body): (Type, Type, Expr) = match shim {
                SharedPtrShim::Get if wrapped => (
                    parse_quote! { *const #bridge_payload },
                    parse_quote! { autocxx::CppRef<#payload> },
                    parse_quote! { autocxx::CppRef::from_ptr(cxxbridge::#shim_id(self)) },
                ),
                SharedPtrShim::Get => (
                    parse_quote! { *const #bridge_payload },
                    parse_quote! { *const #payload },
                    parse_quote! { cxxbridge::#shim_id(self) },
                ),
                SharedPtrShim::Clone => (
                    parse_quote! { UniquePtr<#bridge_id> },
                    parse_quote! { cxx::UniquePtr<#holder> },
                    parse_quote! { cxxbridge::#shim_id(self) },
                ),
                SharedPtrShim::UseCount => (
                    parse_quote! { i64 },
                    parse_quote! { i64 },
                    parse_quote! { cxxbridge::#shim_id(self) },
                ),
            };
            result.extern_c_mod_items.push(parse_quote! {
                fn #shim_id(self_: &#bridge_id) -> #bridge_ret;
            });
            let doc = shared_ptr_method_doc(shim, wrapped);
            methods.push(parse_quote! {
                #[doc = #doc]
                pub #unsafety fn #method_id(&self) -> #method_ret {
                    #body
                }
            });
        }
        let doc = shared_ptr_holder_doc();
        result.output_mod_items.push(parse_quote! {
            #[doc = #doc]
            impl #holder {
                #(#methods)*
            }
        });
    }

    /// Declare the two C++ helpers of a `std::unique_ptr<const T>` holder in
    /// the bridge, and put a method for each on the holder itself.
    ///
    /// Written here rather than as synthesized `Api::Function`s for the reason
    /// [`Self::generate_shared_ptr_surface`] gives, and `get` is that surface's
    /// `get` in every respect - the same pointer, the same `unsafe fn` under
    /// the wrapped-references policy, and the same reason for it. What it does
    /// not have is `clone` or `use_count`: a `std::unique_ptr` is the one
    /// owner, so there is nothing to copy and no count to read. See
    /// google/autocxx#799.
    fn generate_unique_ptr_surface(
        &self,
        name: &QualifiedName,
        bridge_id: &crate::minisyn::Ident,
        payload: &Type,
        result: &mut RsCodegenResult,
    ) {
        // As in `generate_shared_ptr_surface`: the bridge mod has a flat
        // namespace, and the output mod, where the methods go, uses the
        // qualified spellings.
        let holder = name.get_final_ident();
        let bridge_payload = unqualify_type(payload.clone(), self.bridge_type_names);
        let wrapped = matches!(
            self.unsafe_policy,
            UnsafePolicy::ReferencesWrappedAllFunctionsSafe
        );
        let mut methods: Vec<ImplItem> = Vec::new();
        for shim in UniquePtrShim::ALL {
            let shim_id = make_ident(shim.cpp_name(name));
            let method_id = make_ident(shim.rust_name());
            // `std::shared_ptr::get`'s reasoning, unchanged: under this policy
            // a `CppRef` is what safe code may go on to dereference, and the
            // stored pointer may be null.
            let unsafety: Option<syn::token::Unsafe> =
                matches!(shim, UniquePtrShim::Get if wrapped).then(|| parse_quote! { unsafe });
            let (bridge_ret, method_ret, body): (Type, Type, Expr) = match shim {
                UniquePtrShim::Get if wrapped => (
                    parse_quote! { *const #bridge_payload },
                    parse_quote! { autocxx::CppRef<#payload> },
                    parse_quote! { autocxx::CppRef::from_ptr(cxxbridge::#shim_id(self)) },
                ),
                UniquePtrShim::Get => (
                    parse_quote! { *const #bridge_payload },
                    parse_quote! { *const #payload },
                    parse_quote! { cxxbridge::#shim_id(self) },
                ),
                UniquePtrShim::PayloadIsNull => (
                    parse_quote! { bool },
                    parse_quote! { bool },
                    parse_quote! { cxxbridge::#shim_id(self) },
                ),
            };
            result.extern_c_mod_items.push(parse_quote! {
                fn #shim_id(self_: &#bridge_id) -> #bridge_ret;
            });
            let doc = unique_ptr_method_doc(shim, wrapped);
            methods.push(parse_quote! {
                #[doc = #doc]
                pub #unsafety fn #method_id(&self) -> #method_ret {
                    #body
                }
            });
        }
        let doc = unique_ptr_holder_doc();
        result.output_mod_items.push(parse_quote! {
            #[doc = #doc]
            impl #holder {
                #(#methods)*
            }
        });
    }

    /// Declare the one C++ helper of a `const`-reference holder in the bridge,
    /// and put a method for it on the holder itself.
    ///
    /// Written here rather than as a synthesized `Api::Function` for the reason
    /// [`Self::generate_shared_ptr_surface`] gives, and `get` is that
    /// surface's `get` in every respect, `unsafe` under the wrapped-references
    /// policy included. Not being null is not the whole of what a `CppRef`
    /// promises: the referent has to be alive, and a variable of static
    /// storage duration is not alive before its dynamic initialization or
    /// after static destruction - both of which C++ can call into Rust from.
    /// The holder is a `std::reference_wrapper` like any other, at that, so a
    /// header which returns one of its own shares this type and can refer to
    /// whatever it likes. See google/autocxx#94.
    fn generate_const_ref_surface(
        &self,
        name: &QualifiedName,
        bridge_id: &crate::minisyn::Ident,
        payload: &Type,
        result: &mut RsCodegenResult,
    ) {
        // As in `generate_shared_ptr_surface`: the bridge mod has a flat
        // namespace, and the output mod, where the methods go, uses the
        // qualified spellings.
        let holder = name.get_final_ident();
        let bridge_payload = unqualify_type(payload.clone(), self.bridge_type_names);
        let wrapped = matches!(
            self.unsafe_policy,
            UnsafePolicy::ReferencesWrappedAllFunctionsSafe
        );
        let mut methods: Vec<ImplItem> = Vec::new();
        for shim in ConstRefShim::ALL {
            let shim_id = make_ident(shim.cpp_name(name));
            let method_id = make_ident(shim.rust_name());
            // `std::shared_ptr::get`'s reasoning, less the nullness half:
            // under this policy a `CppRef` is what safe code may go on to
            // dereference, and this one promises a live referent rather than a
            // non-null pointer.
            let unsafety: Option<syn::token::Unsafe> =
                matches!(shim, ConstRefShim::Get if wrapped).then(|| parse_quote! { unsafe });
            let (bridge_ret, method_ret, body): (Type, Type, Expr) = match shim {
                ConstRefShim::Get if wrapped => (
                    parse_quote! { *const #bridge_payload },
                    parse_quote! { autocxx::CppRef<#payload> },
                    parse_quote! { autocxx::CppRef::from_ptr(cxxbridge::#shim_id(self)) },
                ),
                ConstRefShim::Get => (
                    parse_quote! { *const #bridge_payload },
                    parse_quote! { *const #payload },
                    parse_quote! { cxxbridge::#shim_id(self) },
                ),
            };
            result.extern_c_mod_items.push(parse_quote! {
                fn #shim_id(self_: &#bridge_id) -> #bridge_ret;
            });
            let doc = const_ref_method_doc(shim, wrapped);
            methods.push(parse_quote! {
                #[doc = #doc]
                pub #unsafety fn #method_id(&self) -> #method_ret {
                    #body
                }
            });
        }
        let doc = const_ref_holder_doc();
        result.output_mod_items.push(parse_quote! {
            #[doc = #doc]
            impl #holder {
                #(#methods)*
            }
        });
    }

    /// Declare the one C++ helper of a user smart pointer's holder in the
    /// bridge, and put a method for it on the holder itself.
    ///
    /// Written here rather than as a synthesized `Api::Function` for the reason
    /// [`Self::generate_shared_ptr_surface`] gives, and `get` is that surface's
    /// `get` in every respect: the same raw pointer, the same `unsafe fn` under
    /// the wrapped-references policy, and the same reason for it - a smart
    /// pointer may hold nothing, and what it does hold is kept alive by C++ on
    /// terms nothing here knows. The pointer is `*mut` where C++ wrote a
    /// mutable argument, because that is what the template's `get` hands back;
    /// mutating through one is `unsafe` for the ordinary reason, and it is a
    /// raw pointer, so it makes no promise about aliasing to break. See
    /// google/autocxx#670.
    fn generate_custom_ptr_surface(
        &self,
        name: &QualifiedName,
        bridge_id: &crate::minisyn::Ident,
        payload: &Type,
        payload_is_const: bool,
        result: &mut RsCodegenResult,
    ) {
        // As in `generate_shared_ptr_surface`: the bridge mod has a flat
        // namespace, and the output mod, where the methods go, uses the
        // qualified spellings.
        let holder = name.get_final_ident();
        let bridge_payload = unqualify_type(payload.clone(), self.bridge_type_names);
        let wrapped = matches!(
            self.unsafe_policy,
            UnsafePolicy::ReferencesWrappedAllFunctionsSafe
        );
        let mut methods: Vec<ImplItem> = Vec::new();
        for shim in CustomPtrShim::ALL {
            let shim_id = make_ident(shim.cpp_name(name));
            let method_id = make_ident(shim.rust_name());
            let unsafety: Option<syn::token::Unsafe> =
                matches!(shim, CustomPtrShim::Get if wrapped).then(|| parse_quote! { unsafe });
            // What the bridge declares is what the C++ shim returns, and that
            // is the template's own `get`: a `const T*` for a `MyPtr<const T>`
            // and a `T*` otherwise. cxx typechecks the declaration against the
            // real C++ signature through a function pointer, so the two have to
            // agree exactly.
            let bridge_ret: Type = if payload_is_const {
                parse_quote! { *const #bridge_payload }
            } else {
                parse_quote! { *mut #bridge_payload }
            };
            let (method_ret, body): (Type, Expr) = match shim {
                // A `CppRef` is what safe code may dereference under this
                // policy, and this pointer promises neither to be non-null nor
                // to outlive the call, so the method is `unsafe` and its safety
                // comment is where the caller vouches for both - exactly as for
                // `std::shared_ptr::get`. A mutable payload is handed over as a
                // shared `CppRef` all the same, which is the surface every
                // other holder's `get` has; `CppRef::const_cast` is how a
                // caller asks for the other one.
                CustomPtrShim::Get if wrapped => (
                    parse_quote! { autocxx::CppRef<#payload> },
                    parse_quote! { autocxx::CppRef::from_ptr(cxxbridge::#shim_id(self)) },
                ),
                CustomPtrShim::Get if payload_is_const => (
                    parse_quote! { *const #payload },
                    parse_quote! { cxxbridge::#shim_id(self) },
                ),
                CustomPtrShim::Get => (
                    parse_quote! { *mut #payload },
                    parse_quote! { cxxbridge::#shim_id(self) },
                ),
            };
            result.extern_c_mod_items.push(parse_quote! {
                fn #shim_id(self_: &#bridge_id) -> #bridge_ret;
            });
            let doc = custom_ptr_method_doc(shim, wrapped);
            methods.push(parse_quote! {
                #[doc = #doc]
                pub #unsafety fn #method_id(&self) -> #method_ret {
                    #body
                }
            });
        }
        let doc = custom_ptr_holder_doc();
        result.output_mod_items.push(parse_quote! {
            #[doc = #doc]
            impl #holder {
                #(#methods)*
            }
        });
    }

    /// Declare the two C++ helpers of a `std::weak_ptr<const T>` holder in the
    /// bridge, and put the surface built from them on the holder itself.
    ///
    /// `lock` hands back the `std::shared_ptr<const T>` holder, which is where
    /// the payload becomes readable: a `std::weak_ptr` gives no access to it
    /// at all, so nothing here names the payload and everything a caller can
    /// do with it is on the other holder. `expired` is written in Rust as the
    /// `use_count() == 0` C++ defines it to be, rather than costing a third
    /// shim. See google/autocxx#799.
    fn generate_weak_ptr_surface(
        &self,
        name: &QualifiedName,
        bridge_id: &crate::minisyn::Ident,
        shared_holder: &QualifiedName,
        result: &mut RsCodegenResult,
    ) {
        let holder = name.get_final_ident();
        // Both holders live in the root namespace, so within the output mod
        // the sibling is reached by its own name; the bridge mod, being flat,
        // needs the name settled for it there.
        let shared_id = shared_holder.get_final_ident();
        let shared_bridge_id = self.bridge_type_names.get(shared_holder);
        for shim in WeakPtrShim::ALL {
            let shim_id = make_ident(shim.cpp_name(name));
            result.extern_c_mod_items.push(match shim {
                WeakPtrShim::Lock => parse_quote! {
                    fn #shim_id(self_: &#bridge_id) -> UniquePtr<#shared_bridge_id>;
                },
                WeakPtrShim::UseCount => parse_quote! {
                    fn #shim_id(self_: &#bridge_id) -> i64;
                },
            });
        }
        let lock_id = make_ident(WeakPtrShim::Lock.cpp_name(name));
        let use_count_id = make_ident(WeakPtrShim::UseCount.cpp_name(name));
        let holder_doc = weak_ptr_holder_doc();
        let lock_doc = weak_ptr_lock_doc();
        let use_count_doc = weak_ptr_use_count_doc();
        let expired_doc = weak_ptr_expired_doc();
        result.output_mod_items.push(parse_quote! {
            #[doc = #holder_doc]
            impl #holder {
                #[doc = #lock_doc]
                pub fn lock(&self) -> cxx::UniquePtr<#shared_id> {
                    cxxbridge::#lock_id(self)
                }

                #[doc = #use_count_doc]
                pub fn use_count(&self) -> i64 {
                    cxxbridge::#use_count_id(self)
                }

                #[doc = #expired_doc]
                pub fn expired(&self) -> bool {
                    self.use_count() == 0
                }
            }
        });
    }

    /// Declare the two C++ helpers of a `std::vector<T*>` holder in the
    /// bridge, and put the read-only surface built from them on the holder
    /// itself.
    ///
    /// Written here rather than as synthesized `Api::Function`s for the reason
    /// [`Self::generate_shared_ptr_surface`] gives, and shaped after
    /// `cxx::CxxVector`, which is what a caller reaching for a `std::vector`
    /// will already know: `len`, `is_empty`, a checked `get`, an unchecked
    /// one, and `iter`.
    ///
    /// Every one of them hands back the element by value - a copy of the
    /// stored `T*`. That is the whole safety story of this type. The vector
    /// owns its pointers and not their pointees, so it makes no promise about
    /// what one points at and this makes none either: the element is a raw
    /// pointer under every unsafe policy, may be null, and needs `unsafe` to
    /// dereference. It follows that a later mutation of the vector cannot
    /// change which address a caller is holding - nothing handed out is a
    /// borrow of an element's slot - though it says nothing about what that
    /// address points at, which the vector never spoke for.
    ///
    /// The mode is read-only. A mutating surface would need a `Pin<&mut>`
    /// receiver and a way to build a holder from Rust, and neither is needed
    /// to bind a header which passes these around. See google/autocxx#330.
    fn generate_vector_surface(
        &self,
        name: &QualifiedName,
        bridge_id: &crate::minisyn::Ident,
        element: &Type,
        result: &mut RsCodegenResult,
    ) {
        // As in `generate_shared_ptr_surface`: the bridge mod has a flat
        // namespace, and the output mod, where the methods go, uses the
        // qualified spellings.
        let holder = name.get_final_ident();
        let bridge_element = unqualify_type(element.clone(), self.bridge_type_names);
        for shim in VectorShim::ALL {
            let shim_id = make_ident(shim.cpp_name(name));
            result.extern_c_mod_items.push(match shim {
                VectorShim::Len => parse_quote! {
                    fn #shim_id(self_: &#bridge_id) -> usize;
                },
                // cxx would insist this be an `unsafe fn` if a raw pointer
                // were a *parameter*; returning one is safe, here as in every
                // other binding autocxx writes for a C++ function returning
                // `T*`. What is unsafe is the index, and the mod this is
                // declared in is private to the generated `ffi` mod, so the
                // only way to reach it is the `unsafe fn` below.
                VectorShim::GetUnchecked => parse_quote! {
                    fn #shim_id(self_: &#bridge_id, pos: usize) -> #bridge_element;
                },
            });
        }
        let len_id = make_ident(VectorShim::Len.cpp_name(name));
        let get_unchecked_id = make_ident(VectorShim::GetUnchecked.cpp_name(name));
        let holder_doc = vector_holder_doc();
        let len_doc = vector_len_doc();
        let is_empty_doc = vector_is_empty_doc();
        let get_doc = vector_get_doc();
        let get_unchecked_doc = vector_get_unchecked_doc();
        let iter_doc = vector_iter_doc();
        result.output_mod_items.push(parse_quote! {
            #[doc = #holder_doc]
            impl #holder {
                #[doc = #len_doc]
                pub fn len(&self) -> usize {
                    cxxbridge::#len_id(self)
                }

                #[doc = #is_empty_doc]
                pub fn is_empty(&self) -> bool {
                    self.len() == 0
                }

                #[doc = #get_unchecked_doc]
                pub unsafe fn get_unchecked(&self, pos: usize) -> #element {
                    cxxbridge::#get_unchecked_id(self, pos)
                }

                #[doc = #get_doc]
                pub fn get(&self, pos: usize) -> Option<#element> {
                    if pos < self.len() {
                        // Safe: the bound was just read from the vector, and
                        // nothing between there and here can have changed it -
                        // this thread makes no other call into C++, and any
                        // other thread mutating a vector Rust holds a
                        // reference to is already a data race the caller owes
                        // us against.
                        Some(unsafe { self.get_unchecked(pos) })
                    } else {
                        None
                    }
                }

                #[doc = #iter_doc]
                pub fn iter(&self) -> impl Iterator<Item = #element> + '_ {
                    (0usize..).map_while(move |pos| self.get(pos))
                }
            }
        });
    }

    #[allow(clippy::too_many_arguments)] // currently the least unclear way
    fn generate_type<F>(
        &self,
        name: &QualifiedName,
        id: crate::minisyn::Ident,
        type_kind: TypeKind,
        movable: bool,
        destroyable: bool,
        item_creator: F,
        doc_attrs: Vec<Attribute>,
        associated_methods: &HashMap<QualifiedName, SuperclassTraitContents>,
        num_generics: usize,
    ) -> RsCodegenResult
    where
        F: FnOnce() -> Option<Item>,
    {
        let mut output_mod_items = Vec::new();
        Self::add_superclass_stuff_to_type(
            name,
            &mut output_mod_items,
            associated_methods.get(name),
            self.unsafe_policy,
        );
        // The generic parameters bindgen declared, carried verbatim into any
        // wrapper this generates: a parameter can have a bound, which a fresh
        // parameter of our own invention could not reproduce.
        let bindgen_generics = match item_creator() {
            Some(Item::Struct(s)) => s.generics.clone(),
            _ => Generics::default(),
        };
        // We have a choice here to either:
        // a) tell cxx to generate an opaque type using 'type A;'
        // b) generate a concrete type definition, e.g. by using bindgen's
        //    or doing our own, and then telling cxx 'type A = bindgen::A';'
        match type_kind {
            TypeKind::Pod | TypeKind::NonPod | TypeKind::Opaque => {
                // Feed cxx "type T = root::bindgen::T"
                // For non-POD types, there might be the option of simply giving
                // cxx a "type T;" as we do for abstract types below. There's
                // two reasons we don't:
                // a) we want to specify size and alignment for the sake of
                //    moveit;
                // b) for nested types such as 'A::B', there is no combination
                //    of cxx-acceptable attributes which will inform cxx that
                //    A is a class rather than a namespace.
                output_mod_items.push(match type_kind {
                    TypeKind::Pod => Self::generate_bindgen_use_stmt(name),
                    _ => non_pod_struct::generate_opaque_type(name, &bindgen_generics, &doc_attrs),
                });
                if num_generics > 0 {
                    // Still generate the type as emitted by bindgen,
                    // but don't attempt to tell cxx about it
                    RsCodegenResult {
                        output_mod_items,
                        ..Default::default()
                    }
                } else {
                    output_mod_items.append(&mut self.generate_extern_type_impl(type_kind, name));
                    RsCodegenResult {
                        bridge_items: create_impl_items(&id, movable, destroyable, self.config),
                        extern_c_mod_items: vec![
                            self.generate_cxxbridge_type(name, true, doc_attrs)
                        ],
                        output_mod_items,
                        ..Default::default()
                    }
                }
            }
            TypeKind::Abstract => {
                if num_generics > 0 {
                    RsCodegenResult::default()
                } else if self.is_nested_in_class(name) {
                    // For types nested within a class (e.g. Foo::CallbackType),
                    // a bare "type T;" makes cxx emit a forward declaration
                    // 'namespace Foo { using CallbackType = ...; }' which
                    // treats the enclosing class as a namespace and fails to
                    // compile. Alias the bindgen definition instead; cxx does
                    // not forward-declare aliases.
                    output_mod_items.push(non_pod_struct::generate_opaque_type(
                        name,
                        &bindgen_generics,
                        &doc_attrs,
                    ));
                    output_mod_items.append(&mut self.generate_extern_type_impl(type_kind, name));
                    RsCodegenResult {
                        extern_c_mod_items: vec![
                            self.generate_cxxbridge_type(name, true, doc_attrs)
                        ],
                        bridge_items: create_impl_items(&id, movable, destroyable, self.config),
                        output_mod_items,
                        ..Default::default()
                    }
                } else {
                    // Feed cxx "type T;"
                    // We MUST do this because otherwise cxx assumes this can be
                    // instantiated using UniquePtr etc.
                    let rust_id = name.get_final_ident();
                    output_mod_items.push(generate_cxx_use_stmt_for_id(
                        name,
                        &id,
                        (id != rust_id).then_some(&rust_id.0),
                    ));
                    RsCodegenResult {
                        extern_c_mod_items: vec![
                            self.generate_cxxbridge_type(name, false, doc_attrs)
                        ],
                        bridge_items: create_impl_items(&id, movable, destroyable, self.config),
                        output_mod_items,
                        ..Default::default()
                    }
                }
            }
        }
    }

    fn add_superclass_stuff_to_type(
        name: &QualifiedName,
        output_mod_items: &mut Vec<Item>,
        contents: Option<&SuperclassTraitContents>,
        unsafe_policy: &UnsafePolicy,
    ) {
        if let Some(contents) = contents {
            let methods = &contents.methods;
            let supers_name = SubclassName::get_supers_trait_name(name).get_final_ident();
            let (supers, mains): (Vec<_>, Vec<_>) = methods
                .iter()
                .zip(super_fn_names(methods))
                .map(|(method, super_id)| {
                    let id = &method.name;
                    let (params, param_names, ret_type) =
                        Self::superclass_trait_method_signature(method);
                    let unsafe_token = method.requires_unsafe.wrapper_token();
                    if !method.has_super_helper {
                        // No superclass implementation this subclass could
                        // call, so no default body: every Rust subclass has
                        // to provide one.
                        (
                            None,
                            parse_quote!(
                                #unsafe_token fn #id(#params) #ret_type;
                            ),
                        )
                    } else {
                        let a: Option<TraitItem> = Some(parse_quote!(
                            #unsafe_token fn #super_id(#params) #ret_type;
                        ));
                        // Spell out which trait's item this is. The `_methods`
                        // trait inherits from the `_supers` one, so if the
                        // superclass happens to have a method of its own by
                        // this name, both would be in scope on `self` here.
                        let b: TraitItem = parse_quote!(
                            #unsafe_token fn #id(#params) #ret_type {
                                #supers_name::#super_id(self, #param_names)
                            }
                        );
                        (a, b)
                    }
                })
                .unzip();
            let supers: Vec<_> = supers.into_iter().flatten().collect();
            let methods_name = SubclassName::get_methods_trait_name(name).get_final_ident();
            if !supers.is_empty() {
                output_mod_items.push(parse_quote! {
                    #[allow(non_snake_case)]
                    pub trait #supers_name {
                        #(#supers)*
                    }
                });
                output_mod_items.push(parse_quote! {
                    #[allow(non_snake_case)]
                    pub trait #methods_name : #supers_name {
                        #(#mains)*
                    }
                });
            } else {
                output_mod_items.push(parse_quote! {
                    #[allow(non_snake_case)]
                    pub trait #methods_name {
                        #(#mains)*
                    }
                });
            }
            if contents.superclass_implements_traits {
                Self::implement_superclass_traits_for_superclass(
                    name,
                    output_mod_items,
                    methods,
                    !supers.is_empty(),
                    unsafe_policy,
                );
            }
        }
    }

    /// The signature a `_methods`/`_supers` trait item gets for one superclass
    /// method - the C++ receiver swapped for a plain `self`, because
    /// implementers are Rust types - plus the names by which to pass the rest
    /// of the parameters on to whoever really does the work.
    ///
    /// Parameters and return value alike are the ones the bridge carries,
    /// undoing any conversion which only makes sense in the Rust-calls-C++
    /// direction: a trait a Rust subclass implements is called the other way
    /// about, so the override receives what C++ passes and produces what C++
    /// will receive.
    fn superclass_trait_method_signature(
        method: &SuperclassMethod,
    ) -> (
        Punctuated<crate::minisyn::FnArg, Comma>,
        Punctuated<Expr, Comma>,
        crate::minisyn::ReturnType,
    ) {
        let param_names = Self::args_from_sig(&minisynize_punctuated(method.params.clone()))
            .collect::<Punctuated<Expr, Comma>>();
        let mut params = method.params.clone();
        for (param, conversion) in params
            .iter_mut()
            .zip(method.param_conversions.iter())
            .skip(1)
        {
            if let (syn::FnArg::Typed(pt), Some((ty, _))) =
                (&mut param.0, conversion.inverse_rust_conversion())
            {
                *pt.ty = ty;
            }
        }
        let ret_type = match Self::superclass_trait_return_conversion(method) {
            Some((ty, _)) => parse_quote! { -> #ty },
            None => method.ret_type.clone(),
        };
        *(params
            .iter_mut()
            .next()
            .expect("Superclass method had no receiver")) = match method.receiver_mutability {
            ReceiverMutability::Const => parse_quote!(&self),
            ReceiverMutability::Mutable => parse_quote!(&mut self),
        };
        (params, param_names, ret_type)
    }

    /// How the return value of one `_methods`/`_supers` trait item differs from
    /// the one the bridge carries, or `None` when it doesn't.
    ///
    /// Under `ReferencesWrappedAllFunctionsSafe` a virtual method returning a
    /// C++ reference reaches the bridge as a raw pointer, and the trait says
    /// `CppRef`/`CppMutRef` instead, just as it does for such a method's
    /// parameters. The second element is the method which gets the pointer
    /// back out of the wrapper, for whoever has to hand one to the bridge.
    fn superclass_trait_return_conversion(method: &SuperclassMethod) -> Option<(Type, Ident)> {
        method
            .ret_conversion
            .as_ref()
            .and_then(|conversion| conversion.inverse_rust_return_conversion())
    }

    /// Adapts a call to one of autocxx's own Rust-calls-C++ bindings for a
    /// superclass method so that it yields what the matching
    /// `_methods`/`_supers` trait item promises.
    ///
    /// The two differ only under `ReferencesWrappedAllFunctionsSafe`, and only
    /// for a method returning a C++ reference: such a binding hands back a
    /// `CppLtRef`/`CppMutLtRef` whose lifetime parameter it invented - the
    /// lifetime appears nowhere among the binding's own arguments, so its
    /// caller already picks it freely - whereas the trait says the
    /// lifetime-free `CppRef`/`CppMutRef` that a Rust subclass's override
    /// returns. `lifetime_cast` is the documented way across, and is safe for
    /// the reason those wrappers exist at all: a `CppRef` is never
    /// dereferenced in Rust, so how long the referent must live is C++'s
    /// business, exactly as it is for the C++ method being wrapped.
    fn binding_call_as_trait_return(method: &SuperclassMethod, call: Expr) -> Expr {
        match Self::superclass_trait_return_conversion(method) {
            Some(_) => parse_quote!( #call.lifetime_cast() ),
            None => call,
        }
    }

    /// Implements a superclass's own `_methods` (and `_supers`) trait for the
    /// superclass itself, so that code generic over the trait accepts the C++
    /// type as readily as any of its Rust subclasses.
    /// See <https://github.com/google/autocxx/issues/609>.
    ///
    /// Each method simply calls the superclass's own binding for it, so only
    /// call this once every one of them is known to have such a binding -
    /// `SuperclassTraitContents::superclass_implements_traits` is that
    /// question.
    fn implement_superclass_traits_for_superclass(
        name: &QualifiedName,
        output_mod_items: &mut Vec<Item>,
        methods: &[SuperclassMethod],
        has_supers_trait: bool,
        unsafe_policy: &UnsafePolicy,
    ) {
        let ty = name.get_final_ident();
        let (supers, mains): (Vec<_>, Vec<_>) = methods
            .iter()
            .zip(super_fn_names(methods))
            .map(|(method, super_id)| {
                let id = &method.name;
                let (params, param_names, ret_type) =
                    Self::superclass_trait_method_signature(method);
                let unsafe_token = method.requires_unsafe.wrapper_token();
                let wraps_references = matches!(
                    unsafe_policy,
                    UnsafePolicy::ReferencesWrappedAllFunctionsSafe
                );
                let receiver: Expr = match method.receiver_mutability {
                    // Under the policy which wraps references, the superclass's
                    // own binding takes its receiver as a `CppRef` like any
                    // other C++ reference, so the trait's `&self` has to become
                    // one. `from_ptr` is safe: a `CppRef` is never dereferenced
                    // in Rust.
                    ReceiverMutability::Const if wraps_references => {
                        parse_quote!(autocxx::CppRef::from_ptr(self))
                    }
                    ReceiverMutability::Mutable if wraps_references => {
                        parse_quote!(autocxx::CppMutRef::from_ptr(self))
                    }
                    ReceiverMutability::Const => parse_quote!(self),
                    // The trait's mutable methods take `&mut self` - that's
                    // what a Rust subclass wants - whereas the superclass's own
                    // binding takes the `Pin<&mut Self>` every C++ object is
                    // held behind.
                    //
                    // SAFETY: the emitted `Pin::new_unchecked` asserts that
                    // this object will not be moved. It won't: the C++ type is
                    // `!Unpin` and has private fields, safe Rust can neither
                    // construct one nor obtain a `&mut` to one (autocxx hands
                    // out only `Pin<&mut T>`, and `Pin::get_mut` wants
                    // `Unpin`), so the `&mut self` we were passed can only have
                    // come from unsafe code which already took on exactly this
                    // obligation - `Pin::into_inner_unchecked` and
                    // `Pin::get_unchecked_mut` both demand it of their callers.
                    ReceiverMutability::Mutable => {
                        parse_quote!(unsafe { ::core::pin::Pin::new_unchecked(self) })
                    }
                };
                let call: Expr = parse_quote!( #ty::#id(#receiver, #param_names) );
                // A method returning a non-POD value by value hands the
                // superclass's caller an `impl New`, whereas the trait
                // promises a `UniquePtr`. That can't also be a returned
                // reference, so the two adaptations never both apply.
                let body: Expr = if method.superclass_binding.returns_new {
                    parse_quote!({
                        use autocxx::moveit::Emplace;
                        cxx::UniquePtr::emplace(#call)
                    })
                } else {
                    Self::binding_call_as_trait_return(method, call)
                };
                if !method.has_super_helper {
                    // There's no `_super` item for a pure virtual method, so
                    // this has to implement the `_methods` item itself; for
                    // the others the trait's own default body forwards to
                    // `_supers` for us. The other reason for a method to have
                    // no `_super` item - being `private` - can't reach here,
                    // because such a method has no binding of the superclass's
                    // own for this impl to call, and our caller only runs when
                    // every method has one.
                    let item: ImplItem = parse_quote!(
                        #unsafe_token fn #id(#params) #ret_type { #body }
                    );
                    (None, Some(item))
                } else {
                    let item: ImplItem = parse_quote!(
                        #unsafe_token fn #super_id(#params) #ret_type { #body }
                    );
                    (Some(item), None)
                }
            })
            .unzip();
        let supers: Vec<_> = supers.into_iter().flatten().collect();
        let mains: Vec<_> = mains.into_iter().flatten().collect();
        if has_supers_trait {
            let supers_name = SubclassName::get_supers_trait_name(name).get_final_ident();
            output_mod_items.push(parse_quote! {
                #[allow(non_snake_case)]
                impl #supers_name for #ty {
                    #(#supers)*
                }
            });
        }
        let methods_name = SubclassName::get_methods_trait_name(name).get_final_ident();
        output_mod_items.push(parse_quote! {
            #[allow(non_snake_case)]
            impl #methods_name for #ty {
                #(#mains)*
            }
        });
    }

    fn generate_extern_cpp_type(
        &self,
        name: &QualifiedName,
        rust_path: TypePath,
    ) -> RsCodegenResult {
        let name_final = name.get_final_ident();
        RsCodegenResult {
            extern_c_mod_items: vec![self.generate_cxxbridge_type(name, true, Vec::new())],
            output_mod_items: vec![parse_quote! { pub use #rust_path as #name_final; }],
            ..Default::default()
        }
    }

    /// Generates something in the output mod that will carry a docstring
    /// explaining why a given type or function couldn't have bindings
    /// generated.
    fn generate_error_entry(err: ConvertErrorFromCpp, ctx: ErrorContext) -> RsCodegenResult {
        let ctx = ctx.into_type();
        if !ctx.is_declarable() {
            // No name Rust would let us hang the docstring on - see
            // `ErrorContextType::is_declarable`.
            return RsCodegenResult::default();
        }
        let err = format!(" autocxx bindings couldn't be generated: {err}");
        let (impl_entry, output_mod_items) = match ctx {
            ErrorContextType::Item(id) | ErrorContextType::SanitizedItem { display: id, .. } => (
                None,
                vec![parse_quote! {
                    #[doc = #err]
                    pub struct #id;
                }],
            ),
            ErrorContextType::Method { self_ty, method } => (
                Some(Box::new(ImplBlockDetails {
                    item: parse_quote! {
                        #[doc = #err]
                        fn #method(_uhoh: autocxx::BindingGenerationFailure) {
                        }
                    },
                    ty: parse_quote! { #self_ty },
                })),
                vec![],
            ),
        };
        RsCodegenResult {
            impl_entry,
            output_mod_items,
            ..Default::default()
        }
    }

    fn generate_bindgen_use_stmt(name: &QualifiedName) -> Item {
        let segs = find_output_mod_root(name.get_namespace()).chain(name.get_bindgen_path_idents());
        Item::Use(parse_quote! {
            #[allow(unused_imports)]
            pub use #(#segs)::*;
        })
    }

    /// Declare a typedef which finally names a concrete template
    /// instantiation as an alias for the type autocxx made for that
    /// instantiation.
    ///
    /// The alternative - re-exporting the typedef as bindgen wrote it - names
    /// bindgen's rendering of the class template with its arguments filled in,
    /// `root::A<u32>`, which is a Rust type cxx has never heard of: it can be
    /// neither constructed nor passed anywhere. See google/autocxx#723.
    fn generate_concrete_typedef(name: &QualifiedName, target: &QualifiedName) -> Item {
        let id = name.get_final_ident();
        let segs = find_output_mod_root(name.get_namespace())
            .chain(target.get_namespace().iter().map(make_ident))
            .chain(std::iter::once(target.get_final_ident()));
        Item::Type(parse_quote! {
            pub type #id = #(#segs)::*;
        })
    }

    /// Re-export a C++ variable, spelling out the assumption Rust makes about
    /// anything it can take a reference to.
    fn generate_static_use_stmt(name: &QualifiedName) -> Item {
        let segs = find_output_mod_root(name.get_namespace()).chain(name.get_bindgen_path_idents());
        Item::Use(parse_quote! {
            #[doc = "A variable defined in C++."]
            #[doc = ""]
            #[doc = "Reading it is `unsafe`. Rust further requires that the object does not change while Rust holds a reference to it, which autocxx cannot enforce: C++ mutating it behind Rust's back - through a `mutable` member of an otherwise `const` object, say - is undefined behaviour and is not supported."]
            #[allow(unused_imports)]
            pub use #(#segs)::*;
        })
    }

    /// Whether this type is nested within a C++ class (as opposed to a
    /// namespace), e.g. a class-scoped typedef or nested class. Such types
    /// have an original C++ name with extra path segments relative to their
    /// enclosing namespace.
    fn is_nested_in_class(&self, name: &QualifiedName) -> bool {
        self.original_name_map
            .get(name)
            .map(|cpp_name| {
                cpp_name
                    .to_qualified_name()
                    .ns_segment_iter()
                    .next()
                    .is_some()
            })
            .unwrap_or(false)
    }

    fn generate_extern_type_impl(&self, type_kind: TypeKind, tyname: &QualifiedName) -> Vec<Item> {
        let tynamestring = self.original_name_map.map(tyname);
        let ty_ident = tyname.get_final_ident();
        let kind_item = match type_kind {
            TypeKind::Pod => "Trivial",
            _ => "Opaque",
        };
        let kind_item = make_ident(kind_item);
        vec![Item::Impl(parse_quote! {
            unsafe impl cxx::ExternType for #ty_ident {
                type Id = cxx::type_id!(#tynamestring);
                type Kind = cxx::kind::#kind_item;
            }
        })]
    }

    fn generate_cxxbridge_type(
        &self,
        name: &QualifiedName,
        references_bindgen: bool,
        doc_attrs: Vec<Attribute>,
    ) -> ForeignItem {
        let ns = name.get_namespace();
        let rust_id = name.get_final_ident();
        let id = self.bridge_type_names.get(name);
        // The following lines actually Tell A Lie.
        // If we have a nested class, B::C, within namespace A,
        // we actually have to tell cxx that we have nested class C
        // within namespace A.
        let mut ns_components: Vec<_> = ns.iter().map(|s| s.to_string()).collect();
        let mut cxx_name = None;
        // If the type's own name is hidden by a variable of the same name, we
        // generate a typedef for it in its own namespace, and that's the name
        // cxx has to use - see `UnshadowingAlias`. It lives at namespace scope
        // even for a type nested in a class, so the lie told below doesn't
        // apply and the namespace is left as it is.
        if let Some(alias) = self.original_name_map.unshadowing_alias(name) {
            cxx_name = Some(alias.to_string());
        } else if let Some(cpp_name) = self.original_name_map.get(name) {
            let cpp_name = cpp_name.to_qualified_name();
            cxx_name = Some(cpp_name.get_final_item().to_string());
            ns_components.extend(cpp_name.ns_segment_iter().map(|s| s.to_string()));
        } else if id != rust_id {
            // We had to rename the type to keep the bridge mod's flat
            // namespace unambiguous, so tell cxx what it's really called.
            cxx_name = Some(rust_id.to_string());
        };

        let mut for_extern_c_ts = if !ns_components.is_empty() {
            let ns_string = ns_components.join("::");
            quote! {
                #[namespace = #ns_string]
            }
        } else {
            TokenStream::new()
        };

        if let Some(n) = cxx_name {
            for_extern_c_ts.extend(quote! {
                #[cxx_name = #n]
            });
        }

        for_extern_c_ts.extend(quote! {
            #(#doc_attrs)*
        });

        if references_bindgen {
            for_extern_c_ts.extend(quote! {
                type #id = super::
            });
            for_extern_c_ts.extend(ns.iter().map(make_ident).map(|id| {
                quote! {
                    #id::
                }
            }));
            for_extern_c_ts.extend(quote! {
                #rust_id;
            });
        } else {
            for_extern_c_ts.extend(quote! {
                type #id;
            });
        }
        ForeignItem::Verbatim(for_extern_c_ts)
    }
}

/// Whether autocxx writes a subclass's `CppPeerConstructor` implementation.
#[derive(Clone, Copy)]
enum PeerConstructorImpl {
    /// autocxx writes it.
    Generated,
    /// The author writes it, for a reason `CppPeerConstructor`'s own
    /// documentation covers: the superclass has several constructors, or one
    /// which takes arguments, or the unsafety policy makes the peer's
    /// constructor an unsafe call.
    LeftToAuthor,
    /// The author writes it, and the generated code says why, because nothing
    /// else would: the peer's constructor is designated by `throws!`, so it
    /// hands back a `Result` and no `make_peer` body could be written by
    /// calling it.
    LeftToAuthorBecauseFallible,
}

/// Whether autocxx writes each subclass's `CppPeerConstructor` implementation.
///
/// autocxx writes one only for a subclass whose superclass offers a single
/// constructor taking no arguments, because that is the only case in which
/// there is no choice to make about which constructor to call and what to pass
/// it. The peer constructor being fallible takes even that case away: a
/// designated constructor hands back a `Result` where the trait promises a
/// `UniquePtr`, so the generated body would not compile - and, being generated
/// anyway, would also collide with the one the author wrote to do the job
/// properly, leaving them no way to have such a subclass at all.
///
/// Designating the *superclass's* constructor marks a different function; a
/// designation which reaches the peer's constructor names the peer.
fn decide_peer_constructors(
    apis: &ApiVec<FnPhase>,
    unsafe_policy: &UnsafePolicy,
) -> HashMap<QualifiedName, PeerConstructorImpl> {
    // Per subclass: whether every constructor synthesized for it takes no
    // arguments, and whether any of them is designated as throwing.
    let mut constructors: HashMap<QualifiedName, (bool, bool)> = HashMap::new();
    for (subclass, is_trivial, may_throw) in apis.iter().filter_map(|api| match api {
        Api::Function { fun, analysis, .. } => match &fun.provenance {
            Provenance::SynthesizedSubclassConstructor(details) => Some((
                details.subclass.0.name.clone(),
                details.is_trivial,
                analysis.may_throw,
            )),
            _ => None,
        },
        _ => None,
    }) {
        let entry = constructors.entry(subclass).or_insert((true, false));
        entry.0 &= is_trivial;
        entry.1 |= may_throw;
    }
    constructors
        .into_iter()
        .map(|(subclass, (all_trivial, any_fallible))| {
            let decision = if !all_trivial {
                PeerConstructorImpl::LeftToAuthor
            } else if any_fallible {
                PeerConstructorImpl::LeftToAuthorBecauseFallible
            } else if matches!(unsafe_policy, UnsafePolicy::AllFunctionsUnsafe) {
                // `CppPeerConstructor::make_peer` is a safe method, so it can
                // only call the generated constructor under a policy which
                // makes that constructor safe. Both of the policies which do
                // are named for it.
                //
                // What this withholds under `AllFunctionsUnsafe` is the
                // automatic impl, not the ability to have subclasses: the user
                // writes `make_peer` themselves with the unsafe call inside it,
                // which is what `test_subclass_no_safety` exercises and what the
                // book's "Callbacks into Rust" chapter shows.
                // The alternative the old note wondered about, a parallel
                // unsafe trait, does not stop at one trait: `CppSubclass`
                // requires `CppPeerConstructor`, and `CppSubclassSelfOwned`,
                // `CppSubclassDefault` and `CppSubclassSelfOwnedDefault` each
                // build on `CppSubclass`, so an unsafe peer constructor drags an
                // unsafe twin of the ownership constructors behind it. Other
                // shapes are available - making those ownership constructors
                // unsafe for everybody, say - but each of them charges the whole
                // subclass API for something the user can write once, in one
                // impl, when they need it.
                //
                // Decision: no unsafe trait; the manual impl is the supported
                // route.
                PeerConstructorImpl::LeftToAuthor
            } else {
                PeerConstructorImpl::Generated
            };
            (subclass, decision)
        })
        .collect()
}

/// The types Rust can only hold behind a pointer, which is what
/// [`lifetime::add_explicit_lifetime_if_necessary`] wants to know for the
/// cxx#1024 case.
///
/// A `concrete!` template instantiation belongs here as much as a non-POD
/// struct does - codegen gives it [`TypeKind::Abstract`] - even though no shape
/// was found in which its membership changes what gets built. It can only flip
/// the cxx#1024 answer, and that answer only ever decides whether a lifetime is
/// written out or elided: a function may return a reference only if it takes
/// exactly one (see `MultipleInputReferences` and `NoInputReference` in
/// `analysis::fun`), so elision always reaches the same conclusion.
/// `test_concrete_template_reference_return` and
/// `test_concrete_template_reference_parameter` are the two shapes, one for
/// each direction the answer flips.
fn find_non_pod_types(apis: &ApiVec<FnPhase>) -> HashSet<QualifiedName> {
    apis.iter()
        .filter_map(|api| match api {
            Api::Struct {
                name,
                analysis:
                    PodAndDepAnalysis {
                        pod:
                            PodAnalysis {
                                kind: TypeKind::NonPod,
                                ..
                            },
                        ..
                    },
                ..
            }
            | Api::ConcreteType { name, .. } => Some(name.name.clone()),
            _ => None,
        })
        .collect()
}

/// The constructor-bearing types autocxx declares to cxx as a plain opaque
/// `type T;`.
///
/// Read for one thing only: the Rust side of such a type is cxx's opaque
/// stand-in, which is zero-sized, so no Rust storage of that type can hold the
/// C++ object and nothing may build one there. See
/// `generate_constructor_impl`.
///
/// Two kinds of type are in it, and autocxx knows the size of neither:
///
/// * a concrete template instantiation, because bindgen reports the template
///   and never the specialization;
/// * a subclass's C++ peer class, because autocxx writes that class itself
///   *after* bindgen has run, so nothing ever measures it. Its superclass is
///   opaque to autocxx too.
///
/// The rest of the opaque `type T;` population - an abstract class, a forward
/// declaration, an opaque typedef - reaches no constructor, so leaving them
/// out changes nothing here: `mark_types_abstract` deletes an abstract class's
/// constructors along with its `CopyNew` and `MoveNew`, and the other two are
/// types whose members autocxx never learns. What an abstract class can still
/// reach is the by-value *return* path; see `build_correctly_sized_type_set`.
fn find_types_with_no_rust_storage(apis: &ApiVec<FnPhase>) -> HashSet<QualifiedName> {
    apis.iter()
        .filter_map(|api| match api {
            Api::ConcreteType { name, .. } => Some(name.name.clone()),
            Api::Subclass { name, .. } => Some(name.cpp()),
            _ => None,
        })
        .collect()
}

/// Each typedef which finally names a concrete template instantiation, and
/// the instantiation it names.
///
/// Follows chains, because C++ allows them: `typedef A<uint32_t> B; typedef B
/// C;` - and it is the last hop which knows whether a concrete type is what
/// this alias is for. See google/autocxx#723.
fn find_concrete_typedefs(apis: &ApiVec<FnPhase>) -> HashMap<QualifiedName, QualifiedName> {
    let concrete_types: HashSet<QualifiedName> = apis
        .iter()
        .filter_map(|api| match api {
            Api::ConcreteType { name, .. } => Some(name.name.clone()),
            _ => None,
        })
        .collect();
    if concrete_types.is_empty() {
        return HashMap::new();
    }
    let targets = typedef_targets(apis);
    apis.iter()
        .filter_map(|api| match api {
            Api::Typedef { name, .. } => {
                let target = resolve_typedefs(&targets, &name.name);
                concrete_types
                    .contains(&target)
                    .then(|| (name.name.clone(), target))
            }
            _ => None,
        })
        .collect()
}

impl HasNs for (QualifiedName, RsCodegenResult) {
    fn get_namespace(&self) -> &Namespace {
        self.0.get_namespace()
    }
}

impl<T: AnalysisPhase> HasNs for Api<T> {
    fn get_namespace(&self) -> &Namespace {
        self.name().get_namespace()
    }
}

/// What the generated docs say about a `std::shared_ptr<const T>` holder, on
/// the impl block carrying its three methods.
///
/// The point of saying it in the generated code is that the type's name gives
/// no clue: a caller who expected `cxx::SharedPtr` needs to know both that this
/// is a real `std::shared_ptr` and why it isn't spelt as one. See
/// google/autocxx#799.
fn shared_ptr_holder_doc() -> String {
    "This type is a C++ `std::shared_ptr<const T>`, held opaquely.\n\n\
     `cxx::SharedPtr<T>` cannot stand for it. cxx spells that specialization \
     `std::shared_ptr<T>`, dropping the `const`, because Rust has no `const T` \
     to put in the `T`; the C++ which cxx then generates does not compile \
     against the real signature. autocxx therefore declares this instantiation \
     to cxx as an opaque extern type whose C++ definition is exactly \
     `std::shared_ptr<const T>`, and gives it the methods below.\n\n\
     Ownership works as it does in C++, which is to say it is the C++ object's \
     and not this wrapper's: dropping the `UniquePtr` holding one of these \
     destroys a `shared_ptr`, releasing a reference to whatever ownership \
     group it belonged to, and `clone` copy-constructs one into the same \
     group. A `shared_ptr` need not belong to a group at all - an empty one \
     does not, and neither does one built with the aliasing constructor - and \
     for those there is no count to move. What this type does not have is \
     cxx's `SharedPtr` API - `null`, `Deref`, and the rest - because it is not \
     one.\n\n\
     The `const` is C++'s, and describes the access path this type gives you \
     rather than the payload. C++ may hold a `std::shared_ptr<T>` to the same \
     object and write through it, so treat what `get` returns as you would any \
     other pointer into C++.\n\n\
     Nothing here makes the holder `Send` or `Sync`: the reference count is \
     atomic, which says nothing about whether the payload may be touched from \
     another thread."
        .to_string()
}

/// What the generated docs say about each of the holder's three methods.
fn shared_ptr_method_doc(shim: SharedPtrShim, wrapped: bool) -> String {
    // The two `Get` arms describe the same C++ call and differ only in what
    // Rust receives, so they say the same things about it.
    let get_caveats = "It may be null, and this does not check. Holding the \
         holder does not by itself establish that the pointer refers to a live \
         object either: `std::shared_ptr` has an aliasing constructor, and one \
         built with it stores a pointer whose lifetime is not tied to the \
         ownership group it shares.";
    match shim {
        SharedPtrShim::Get if wrapped => format!(
            "The stored pointer, as a `CppRef` - `std::shared_ptr::get`.\n\n\
             {get_caveats}\n\n\
             # Safety\n\n\
             Under this policy a `CppRef` is what a C++ `const T&` parameter \
             takes, and the generated C++ dereferences it without any further \
             `unsafe` on your part - so producing one is where the promise has \
             to be made. The caller must establish what the C++ header would \
             otherwise have promised: that the stored pointer is non-null, \
             aligned, and refers to a live object for as long as the `CppRef` \
             is used.\n\n\
             The payload's C++ type is `const`, so no method here yields \
             anything mutable - though `CppRef::const_cast` will hand you a \
             `CppMutRef` if you ask, exactly as C++'s `const_cast` would."
        ),
        SharedPtrShim::Get => format!(
            "The stored pointer - `std::shared_ptr::get`.\n\n\
             {get_caveats} Both are why dereferencing it is `unsafe`.\n\n\
             The payload's C++ type is `const`, so this is a `*const` and no \
             method here yields a `*mut` - though Rust will let you cast one, \
             exactly as C++'s `const_cast` would."
        ),
        SharedPtrShim::Clone => "A copy of this `shared_ptr`, sharing whatever it owns.\n\n\
             This is C++ copy-construction, not a copy of the payload: the two \
             join the same ownership group and the group's count rises by one. \
             A `shared_ptr` which owns nothing - an empty one, or one built \
             with the aliasing constructor - has no group and no count, and \
             copying it produces another of the same."
            .to_string(),
        SharedPtrShim::UseCount => {
            "`std::shared_ptr::use_count` - the number of `shared_ptr`s sharing \
             ownership with this one, itself included.\n\n\
             Zero where this one owns nothing, which does not mean `get` is \
             null: the aliasing constructor produces exactly that pair. As in \
             C++, the answer is for diagnostics - in the presence of other \
             threads it may already be stale."
                .to_string()
        }
    }
}

/// What the generated docs say about a `std::unique_ptr<const T>` holder, on
/// the impl block carrying its two methods. See google/autocxx#799.
/// What the generated docs say about the holder standing for a `const`
/// reference to a C++ variable.
fn const_ref_holder_doc() -> String {
    "This type stands for a `const` reference to a C++ variable, held \
     opaquely.\n\n\
     A C++ variable whose type is one Rust may hold by value is re-exported \
     as itself. This one is not: its type reaches Rust as an opaque wrapper \
     rather than as the layout C++ gave it, so there is nothing for Rust to \
     hold. autocxx generates a getter instead, and what it hands back is \
     this - a `std::reference_wrapper<const T>` declared to cxx as an opaque \
     extern type, with the method below.\n\n\
     Nothing is copied. The holder is a pointer's worth of C++ vocabulary \
     type referring to the variable itself, so reading through it sees \
     whatever C++ has most recently written there, and the variable's type \
     need not be copy-constructible.\n\n\
     The `const` is C++'s, and describes the access path this type gives you \
     rather than the variable; C++ may write to it. Nothing here makes the \
     holder `Send` or `Sync`."
        .to_string()
}

/// What the generated docs say about that holder's one method.
fn const_ref_method_doc(shim: ConstRefShim, wrapped: bool) -> String {
    let lifetime = "It is never null - a `std::reference_wrapper` always \
         refers to something - but that is not the same as alive. A variable \
         of static storage duration is alive from its initialization until \
         static destruction runs, and C++ can call into Rust from either side \
         of that window. Nothing here checks, and a holder which came from \
         somewhere other than a variable's getter need not refer to a variable \
         at all.";
    match shim {
        ConstRefShim::Get if wrapped => format!(
            "The variable this refers to, as a `CppRef` - \
             `std::reference_wrapper::get`.\n\n\
             {lifetime}\n\n\
             # Safety\n\n\
             Under this policy a `CppRef` is what a C++ `const T&` parameter \
             takes, and the generated C++ dereferences it without any further \
             `unsafe` on your part - so producing one is where the promise has \
             to be made. The caller must establish that the referent is alive \
             for as long as the `CppRef` is used.\n\n\
             The referent's C++ type is `const`, so this yields nothing \
             mutable - though `CppRef::const_cast` will hand you a \
             `CppMutRef` if you ask, exactly as C++'s `const_cast` would."
        ),
        ConstRefShim::Get => format!(
            "The address of the variable this refers to - \
             `std::reference_wrapper::get`.\n\n\
             {lifetime} That is why dereferencing it is `unsafe`.\n\n\
             The referent's C++ type is `const`, so this is a `*const` - \
             though Rust will let you cast one, exactly as C++'s \
             `const_cast` would."
        ),
    }
}

/// What the generated docs say about the holder of an instantiation of a user's
/// own smart pointer template.
fn custom_ptr_holder_doc() -> String {
    "This type is an instantiation of a C++ class template a `smart_pointer!` \
     directive declared to be a smart pointer, held opaquely.\n\n\
     autocxx knows nothing about such a template beyond that declaration, so \
     the instantiation is declared to cxx as an opaque extern type whose C++ \
     definition is exactly that specialization. C++ can hand one to Rust and \
     take it back, and destroys it when Rust drops it; the method below is what \
     the directive adds, and is the whole of what Rust can do with one \
     besides.\n\n\
     The method below is the whole of what the directive adds: there is no way \
     to make one of these from Rust and no way to copy one, so a \
     reference-counted pointer can be moved from Rust but not shared from it. \
     Nothing here makes the holder `Send` or `Sync`."
        .to_string()
}

/// What the generated docs say about that holder's one method.
fn custom_ptr_method_doc(shim: CustomPtrShim, wrapped: bool) -> String {
    let claim = "What this points at is whatever the template's own `get` \
         hands back, and the `smart_pointer!` directive is the only thing which \
         says there is such a member: autocxx cannot inspect a specialization, \
         so the C++ compiler is the arbiter of the claim.";
    let lifetime = "A smart pointer may hold nothing, in which case this is \
         null. What it does hold is kept alive by C++ on terms autocxx does not \
         know - a reference count this holder owns a share of, or something \
         else entirely - and dropping the holder may be what ends that.";
    match shim {
        CustomPtrShim::Get if wrapped => format!(
            "What this smart pointer points at, as a `CppRef` - the \
             template's `get`.\n\n\
             {claim}\n\n\
             {lifetime}\n\n\
             # Safety\n\n\
             Under this policy a `CppRef` is what a C++ `const T&` parameter \
             takes, and the generated C++ dereferences it without any further \
             `unsafe` on your part - so producing one is where the promise has \
             to be made. The caller must establish that this smart pointer is \
             not empty and that what it points at outlives the `CppRef`.\n\n\
             A mutable payload reaches Rust as a shared `CppRef` all the same, \
             which is the surface every other holder's `get` has. \
             `CppRef::const_cast` will hand you a `CppMutRef` if you ask, \
             exactly as C++'s `const_cast` would."
        ),
        CustomPtrShim::Get => format!(
            "What this smart pointer points at - the template's `get`.\n\n\
             {claim}\n\n\
             {lifetime} That is why dereferencing this pointer is `unsafe`: \
             nothing here promises it is non-null, and nothing here ties what \
             it points at to the life of the holder."
        ),
    }
}

fn unique_ptr_holder_doc() -> String {
    "This type is a C++ `std::unique_ptr<const T>`, held opaquely.\n\n\
     `cxx::UniquePtr<T>` cannot stand for it. cxx spells that specialization \
     `std::unique_ptr<T>`, dropping the `const`, because Rust has no `const T` \
     to put in the `T`; the C++ which cxx then generates does not compile \
     against the real signature. autocxx therefore declares this instantiation \
     to cxx as an opaque extern type whose C++ definition is exactly \
     `std::unique_ptr<const T>`, and gives it the methods below.\n\n\
     Ownership is the C++ object's, as it is in C++: dropping the \
     `cxx::UniquePtr` holding one of these runs `~unique_ptr`, which destroys \
     the payload. Note the two levels - the outer `cxx::UniquePtr` is how any \
     opaque C++ object reaches Rust, and the inner one is the C++ type this \
     is.\n\n\
     The surface is read-only: `release`, `reset` and `swap` would need a \
     `Pin<&mut>` receiver and a way to build one of these from Rust, and \
     neither is needed to bind a header which passes these around. Nor is \
     there any way to move the payload out.\n\n\
     The `const` is C++'s, and describes the access path this type gives you \
     rather than the payload; C++ may hold a mutable pointer to the same \
     object. Nothing here makes the holder `Send` or `Sync`."
        .to_string()
}

/// What the generated docs say about each of the `std::unique_ptr<const T>`
/// holder's two methods.
fn unique_ptr_method_doc(shim: UniquePtrShim, wrapped: bool) -> String {
    let get_caveats = "It may be null - a `std::unique_ptr` need not hold \
         anything, and `payload_is_null` is how to find out; note that the \
         `cxx::UniquePtr` this arrives in has an `is_null` of its own, which \
         answers about that outer pointer instead. Beyond null, the pointer is \
         only as good as the holder: the payload dies with the `unique_ptr`.";
    match shim {
        UniquePtrShim::Get if wrapped => format!(
            "The stored pointer, as a `CppRef` - `std::unique_ptr::get`.\n\n\
             {get_caveats}\n\n\
             # Safety\n\n\
             Under this policy a `CppRef` is what a C++ `const T&` parameter \
             takes, and the generated C++ dereferences it without any further \
             `unsafe` on your part - so producing one is where the promise has \
             to be made. The caller must establish what the C++ header would \
             otherwise have promised: that the stored pointer is non-null - \
             [`Self::payload_is_null`] answers that - aligned, and refers to a \
             live object for as long as the `CppRef` is used.\n\n\
             The payload's C++ type is `const`, so no method here yields \
             anything mutable - though `CppRef::const_cast` will hand you a \
             `CppMutRef` if you ask, exactly as C++'s `const_cast` would."
        ),
        UniquePtrShim::Get => format!(
            "The stored pointer - `std::unique_ptr::get`.\n\n\
             {get_caveats} Both are why dereferencing it is `unsafe`.\n\n\
             The payload's C++ type is `const`, so this is a `*const` and no \
             method here yields a `*mut` - though Rust will let you cast one, \
             exactly as C++'s `const_cast` would."
        ),
        UniquePtrShim::PayloadIsNull => "Whether this `unique_ptr` holds nothing - \
             C++'s `operator bool`, negated.\n\n\
             The question [`Self::get`] does not answer, and the one to settle \
             before dereferencing what it returns."
            .to_string(),
    }
}

/// What the generated docs say about a `std::weak_ptr<const T>` holder, on the
/// impl block carrying its methods. See google/autocxx#799.
fn weak_ptr_holder_doc() -> String {
    "This type is a C++ `std::weak_ptr<const T>`, held opaquely.\n\n\
     `cxx::WeakPtr<T>` cannot stand for it. cxx spells that specialization \
     `std::weak_ptr<T>`, dropping the `const`, because Rust has no `const T` \
     to put in the `T`; the C++ which cxx then generates does not compile \
     against the real signature. autocxx therefore declares this instantiation \
     to cxx as an opaque extern type whose C++ definition is exactly \
     `std::weak_ptr<const T>`, and gives it the methods below.\n\n\
     A `std::weak_ptr` observes an ownership group without joining it, so it \
     gives no access to the payload at all: `lock` is the only way to read \
     one, and it answers with a `std::shared_ptr` which does own a share. That \
     is why the payload is nowhere in this type's own methods - everything you \
     can do with it is on what `lock` returns.\n\n\
     Nothing here makes the holder `Send` or `Sync`."
        .to_string()
}

fn weak_ptr_lock_doc() -> String {
    "A `std::shared_ptr` sharing ownership of the payload, if it is still \
     there - `std::weak_ptr::lock`.\n\n\
     The result is always a holder, never nothing: C++ answers an expired \
     `weak_ptr` with an *empty* `shared_ptr`, and so does this. `get` on it is \
     then null and `use_count` is zero, which is how to tell the two apart. \
     Taking the lock is the only way to read the payload safely - checking \
     [`Self::expired`] first and reading afterwards would be checking \
     something another thread may since have changed."
        .to_string()
}

fn weak_ptr_use_count_doc() -> String {
    "`std::weak_ptr::use_count` - the number of `shared_ptr`s owning the \
     payload this observes. This `weak_ptr` is not one of them, so it does not \
     count itself.\n\n\
     As in C++, the answer is for diagnostics - in the presence of other \
     threads it may already be stale."
        .to_string()
}

fn weak_ptr_expired_doc() -> String {
    "Whether the payload is gone - `std::weak_ptr::expired`, which C++ defines \
     as `use_count() == 0` and this computes the same way.\n\n\
     Also as in C++, a `false` answer may already be stale by the time you \
     read it. [`Self::lock`] is the answer which cannot be, because what it \
     hands back keeps the payload alive."
        .to_string()
}

/// What the generated docs say about a `std::vector<T*>` holder, on the impl
/// block carrying its methods.
///
/// As with the smart-pointer holder, the type's name gives no clue, so this is
/// where a caller who expected `cxx::CxxVector` learns what they have. It is
/// also the only place the ownership contract can be stated once rather than
/// per method. See google/autocxx#330.
fn vector_holder_doc() -> String {
    "This type is a C++ `std::vector<T*>`, held opaquely.\n\n\
     `cxx::CxxVector<T>` cannot stand for it: cxx implements its `VectorElement` \
     for its own types and for opaque `ExternType`s, and a raw pointer is \
     neither. autocxx therefore declares this instantiation to cxx as an opaque \
     extern type whose C++ definition is exactly `std::vector<T*>`, and gives it \
     the methods below.\n\n\
     **The vector owns the pointers, not what they point at.** Dropping the \
     `UniquePtr` holding one of these runs `~vector`, which frees the array of \
     pointers and touches no pointee. So an element is exactly as good as C++ \
     made it: it may be null, it may already dangle, and nothing about holding \
     this vector keeps it alive. Every accessor hands the element back as a raw \
     pointer for that reason, under every unsafe policy, so that reading through \
     one stays `unsafe` and the promise stays yours to make.\n\n\
     What an accessor gives you is the stored pointer's *value*, copied out, \
     and not a reference to the slot it sits in. So `push_back`, `erase` and \
     the reallocation they cause cannot change which address you are holding, \
     the way they would leave a borrow of the element itself dangling. They can \
     still end the life of what it points at, if that happens to live in \
     storage those operations own - an element made to point into the vector's \
     own buffer is invalidated by a reallocation like anything else in there. \
     The pointee's lifetime was never this vector's promise, and it is not this \
     type's either.\n\n\
     The surface is read-only. There is no way to change the vector from Rust \
     here; C++ still can, and `len` and `get` ask it afresh every time."
        .to_string()
}

fn vector_len_doc() -> String {
    "The number of elements - `std::vector::size`.\n\n\
     Read from C++ on every call, so it reflects any mutation C++ has made \
     since the last one."
        .to_string()
}

fn vector_is_empty_doc() -> String {
    "Whether the vector has no elements. As `len() == 0`, and read afresh in \
     the same way."
        .to_string()
}

fn vector_get_doc() -> String {
    "The element at `pos`, or `None` if there is no such element.\n\n\
     The bound is read from C++ immediately before the element is, so this \
     cannot read past the end.\n\n\
     `Some` carries a raw pointer which may itself be null: an element of a \
     `std::vector<T*>` is whatever C++ put there, and a present element and a \
     non-null one are different questions. See the type's own documentation for \
     what the pointer does and does not promise."
        .to_string()
}

fn vector_get_unchecked_doc() -> String {
    "The element at `pos`, without checking that there is one - \
     `std::vector::operator[]`.\n\n\
     [`Self::get`] is this with the bound checked, and costs one extra call \
     into C++ to read the length.\n\n\
     # Safety\n\n\
     `pos` must be less than the length of the vector at the moment of the \
     call. Indexing a `std::vector` out of range is undefined behaviour in C++, \
     and nothing here or in the generated C++ checks. Note that C++ may have \
     shortened the vector since any length you read earlier."
        .to_string()
}

fn vector_iter_doc() -> String {
    "The elements, in order.\n\n\
     Each step asks the vector for its length and then for one element, so a \
     C++ mutation part-way through changes what the iterator goes on to yield \
     rather than taking it past the end. That is also why this is not an \
     `ExactSizeIterator`: the length is not fixed when iteration starts.\n\n\
     The items are raw pointers, with everything the type's own documentation \
     says about them."
        .to_string()
}

/// Snippets of code generated from a particular API.
/// These are then concatenated together into the final generated code.
#[derive(Default)]
struct RsCodegenResult {
    extern_c_mod_items: Vec<ForeignItem>,
    extern_rust_mod_items: Vec<ForeignItem>,
    bridge_items: Vec<Item>,
    /// Items that go in the top level.
    global_items: Vec<Item>,
    impl_entry: Option<Box<ImplBlockDetails>>,
    trait_impl_entry: Option<Box<TraitImplBlockDetails>>,
    /// Items that go into a per-namespace mod exposed to the user.
    output_mod_items: Vec<Item>,
}

/// An [`Item`] that always needs to be in an unsafe block.
#[derive(Clone)]
enum MaybeUnsafeStmt {
    // This could almost be a syn::Stmt, but that doesn't quite work
    // because the last stmt in a function is actually an expression
    // thus lacking a semicolon.
    Normal(TokenStream),
    NeedsUnsafe(TokenStream),
    Binary {
        in_safe_context: TokenStream,
        in_unsafe_context: TokenStream,
    },
}

impl MaybeUnsafeStmt {
    fn new(stmt: TokenStream) -> Self {
        Self::Normal(stmt)
    }

    fn needs_unsafe(stmt: TokenStream) -> Self {
        Self::NeedsUnsafe(stmt)
    }

    fn maybe_unsafe(stmt: TokenStream, needs_unsafe: bool) -> Self {
        if needs_unsafe {
            Self::NeedsUnsafe(stmt)
        } else {
            Self::Normal(stmt)
        }
    }

    fn binary(in_safe_context: TokenStream, in_unsafe_context: TokenStream) -> Self {
        Self::Binary {
            in_safe_context,
            in_unsafe_context,
        }
    }
}

fn maybe_unsafes_to_tokens(
    items: Vec<MaybeUnsafeStmt>,
    context_is_already_unsafe: bool,
) -> TokenStream {
    if context_is_already_unsafe {
        let items = items.into_iter().map(|item| match item {
            MaybeUnsafeStmt::Normal(stmt)
            | MaybeUnsafeStmt::NeedsUnsafe(stmt)
            | MaybeUnsafeStmt::Binary {
                in_unsafe_context: stmt,
                ..
            } => stmt,
        });
        quote! {
            #(#items)*
        }
    } else {
        let mut currently_unsafe_list = None;
        let mut output = Vec::new();
        for item in items {
            match item {
                MaybeUnsafeStmt::NeedsUnsafe(stmt) => {
                    if currently_unsafe_list.is_none() {
                        currently_unsafe_list = Some(Vec::new());
                    }
                    currently_unsafe_list.as_mut().unwrap().push(stmt);
                }
                MaybeUnsafeStmt::Normal(stmt)
                | MaybeUnsafeStmt::Binary {
                    in_safe_context: stmt,
                    ..
                } => {
                    if let Some(currently_unsafe_list) = currently_unsafe_list.take() {
                        output.push(quote! {
                            unsafe {
                                #(#currently_unsafe_list)*
                            }
                        })
                    }
                    output.push(stmt);
                }
            }
        }
        if let Some(currently_unsafe_list) = currently_unsafe_list.take() {
            output.push(quote! {
                unsafe {
                    #(#currently_unsafe_list)*
                }
            })
        }
        quote! {
            #(#output)*
        }
    }
}

#[test]
fn test_maybe_unsafes_to_tokens() {
    let items = vec![
        MaybeUnsafeStmt::new(quote! { use A; }),
        MaybeUnsafeStmt::new(quote! { use B; }),
        MaybeUnsafeStmt::needs_unsafe(quote! { use C; }),
        MaybeUnsafeStmt::needs_unsafe(quote! { use D; }),
        MaybeUnsafeStmt::new(quote! { use E; }),
        MaybeUnsafeStmt::needs_unsafe(quote! { use F; }),
    ];
    assert_eq!(
        maybe_unsafes_to_tokens(items.clone(), false).to_string(),
        quote! {
            use A;
            use B;
            unsafe {
                use C;
                use D;
            }
            use E;
            unsafe {
                use F;
            }
        }
        .to_string()
    );
    assert_eq!(
        maybe_unsafes_to_tokens(items, true).to_string(),
        quote! {
            use A;
            use B;
            use C;
            use D;
            use E;
            use F;
        }
        .to_string()
    );
}
