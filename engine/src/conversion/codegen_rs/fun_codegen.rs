// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::set::IndexSet as HashSet;
use std::borrow::Cow;

use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use syn::{
    parse::Parser,
    parse_quote,
    punctuated::Punctuated,
    token::{Comma, Unsafe},
    Attribute, ForeignItem, Ident, ImplItem, Item, ReturnType,
};

use super::{
    function_wrapper_rs::RustParamConversion,
    maybe_unsafes_to_tokens,
    unqualify::{unqualify_params_minisyn, unqualify_ret_type},
    utils::generate_cxx_use_stmt,
    ImplBlockDetails, MaybeUnsafeStmt, RsCodegenResult, TraitImplBlockDetails,
};
use crate::{
    conversion::{
        analysis::bridge_type_names::BridgeTypeNames,
        analysis::fun::{
            function_wrapper::TypeConversionPolicy, ArgumentAnalysis, FnAnalysis, FnKind,
            MethodKind, RustRenameStrategy, TraitMethodDetails,
        },
        api::UnsafetyNeeded,
    },
    minisyn::{minisynize_vec, FnArg},
    types::QualifiedName,
};
use crate::{
    conversion::{api::FuncToConvert, codegen_rs::lifetime::add_explicit_lifetime_if_necessary},
    types::make_ident,
};

impl UnsafetyNeeded {
    pub(crate) fn bridge_token(&self) -> Option<Unsafe> {
        match self {
            UnsafetyNeeded::None => None,
            _ => Some(parse_quote! { unsafe }),
        }
    }

    pub(crate) fn wrapper_token(&self) -> Option<Unsafe> {
        match self {
            UnsafetyNeeded::Always => Some(parse_quote! { unsafe }),
            _ => None,
        }
    }

    pub(crate) fn from_param_details(params: &[ArgumentAnalysis], ignore_placements: bool) -> Self {
        params.iter().fold(UnsafetyNeeded::None, |accumulator, pd| {
            if matches!(accumulator, UnsafetyNeeded::Always) {
                UnsafetyNeeded::Always
            } else if (pd.self_type.is_some() || pd.is_placement_return_destination)
                && ignore_placements
            {
                if matches!(
                    pd.requires_unsafe,
                    UnsafetyNeeded::Always | UnsafetyNeeded::JustBridge
                ) {
                    UnsafetyNeeded::JustBridge
                } else {
                    accumulator
                }
            } else if matches!(pd.requires_unsafe, UnsafetyNeeded::Always) {
                UnsafetyNeeded::Always
            } else if matches!(accumulator, UnsafetyNeeded::JustBridge)
                || matches!(pd.requires_unsafe, UnsafetyNeeded::JustBridge)
            {
                UnsafetyNeeded::JustBridge
            } else {
                UnsafetyNeeded::None
            }
        })
    }
}

pub(super) fn gen_function(
    name: &QualifiedName,
    fun: FuncToConvert,
    analysis: FnAnalysis,
    non_pod_types: &HashSet<QualifiedName>,
    bridge_type_names: &BridgeTypeNames,
) -> RsCodegenResult {
    if analysis.ignore_reason.is_err() || !analysis.externally_callable {
        return RsCodegenResult::default();
    }
    let cxxbridge_name = analysis.cxxbridge_name;
    let rust_name = &analysis.rust_name;
    let cpp_call_name = &analysis.cpp_call_name;
    let ret_type = analysis.ret_type;
    let ret_conversion = analysis.ret_conversion;
    let param_details = analysis.param_details;
    let wrapper_function_needed = analysis.cpp_wrapper.is_some();
    let params = analysis.params;
    let vis = analysis.vis;
    let kind = analysis.kind;
    let may_throw = analysis.may_throw;
    let doc_attrs = minisynize_vec(fun.doc_attrs);
    let deprecation = fun
        .deprecation
        .as_ref()
        .map(|deprecation| match deprecation.message() {
            Some(note) => parse_quote! { #[deprecated(note = #note)] },
            None => parse_quote! { #[deprecated] },
        });

    let mut cpp_name_attr = Vec::new();
    let mut impl_entry = None;
    let mut trait_impl_entry = None;
    let fn_generator = FnGenerator {
        param_details: &param_details,
        cxxbridge_name: &cxxbridge_name,
        rust_name,
        unsafety: &analysis.requires_unsafe,
        doc_attrs: &doc_attrs,
        deprecation: &deprecation,
        non_pod_types,
        ret_type: &ret_type,
        ret_conversion: &ret_conversion,
        may_throw,
    };
    // In rare occasions, we might need to give an explicit lifetime.
    let (lifetime_tokens, params, ret_type) = add_explicit_lifetime_if_necessary(
        &param_details,
        params,
        Cow::Borrowed(&ret_type),
        non_pod_types,
        &ret_conversion,
    );

    let mut output_mod_items = Vec::new();

    if analysis.rust_wrapper_needed {
        match kind {
            FnKind::Method {
                ref impl_for,
                method_kind: MethodKind::Constructor { .. },
                ..
            } => {
                // Constructor.
                impl_entry = Some(fn_generator.generate_constructor_impl(impl_for));
            }
            FnKind::Method {
                ref impl_for,
                ref method_kind,
                ..
            } => {
                // Method, or static method.
                impl_entry = Some(fn_generator.generate_method_impl(
                    matches!(method_kind, MethodKind::Constructor { .. }),
                    impl_for,
                ));
            }
            FnKind::TraitMethod { ref details, .. } => {
                trait_impl_entry = Some(fn_generator.generate_trait_impl(details));
            }
            _ => {
                // Generate plain old function
                output_mod_items.push(fn_generator.generate_function_impl());
            }
        }
    } else if matches!(kind, FnKind::Function) {
        let alias = match analysis.rust_rename_strategy {
            RustRenameStrategy::RenameInOutputMod(ref alias) => Some(&alias.0),
            _ => None,
        };
        output_mod_items.push(generate_cxx_use_stmt(name, alias));
    }

    if let Some(cpp_call_name) = cpp_call_name {
        if cpp_call_name.does_not_match_cxxbridge_name(&cxxbridge_name) && !wrapper_function_needed
        {
            cpp_name_attr = Attribute::parse_outer
                .parse2(cpp_call_name.generate_cxxbridge_name_attribute())
                .unwrap();
        }
    }

    // Finally - namespace support. All the Types in everything
    // above this point are fully qualified. We need to unqualify them.
    // We need to do that _after_ the above wrapper_function_needed
    // work, because it relies upon spotting fully qualified names like
    // std::unique_ptr. However, after it's done its job, all such
    // well-known types should be unqualified already (e.g. just UniquePtr)
    // and the following code will act to unqualify only those types
    // which the user has declared.
    let params = unqualify_params_minisyn(params, bridge_type_names);
    let ret_type = unqualify_ret_type(ret_type.into_owned(), bridge_type_names);

    // For functions marked as throwing, wrap the return type in Result for the cxx bridge.
    // cxx will catch C++ exceptions and convert them to cxx::Exception.
    let bridge_ret_type = if may_throw {
        match &ret_type {
            ReturnType::Default => parse_quote! { -> Result<()> },
            ReturnType::Type(arrow, ty) => parse_quote! { #arrow Result<#ty> },
        }
    } else {
        ret_type.clone()
    };

    // And we need to make an attribute for the namespace that the function
    // itself is in.
    let namespace_attr = if name.get_namespace().is_empty() || wrapper_function_needed {
        Vec::new()
    } else {
        let namespace_string = name.get_namespace().to_string();
        Attribute::parse_outer
            .parse2(quote!(
                #[namespace = #namespace_string]
            ))
            .unwrap()
    };
    // At last, actually generate the cxx::bridge entry.
    let bridge_unsafety = analysis.requires_unsafe.bridge_token();
    let extern_c_mod_item = ForeignItem::Fn(parse_quote!(
        #(#namespace_attr)*
        #(#cpp_name_attr)*
        #(#doc_attrs)*
        #vis #bridge_unsafety fn #cxxbridge_name #lifetime_tokens ( #params ) #bridge_ret_type;
    ));
    RsCodegenResult {
        extern_c_mod_items: vec![extern_c_mod_item],
        impl_entry,
        trait_impl_entry,
        output_mod_items,
        ..Default::default()
    }
}

/// `#[must_use]` for a function which hands back an `impl New` or an
/// `impl TryNew`, or `None` if it hands back anything else.
///
/// Either is a recipe for constructing a C++ object, not the object:
/// dropping one runs no constructor, allocates nothing and reports nothing, so
/// a caller who writes `Goat::new();` and moves on gets no goat and no
/// complaint. That silence is what the attribute buys back, and the message
/// names the ways to cash the recipe in - which differ between the two, because
/// a fallible recipe is finished by the `try_` spelling of each.
fn must_use_attr_if_impl_new(ret_type: &ReturnType) -> Option<Attribute> {
    match returns_impl_new(ret_type) {
        None => None,
        Some(Fallibility::Infallible) => Some(parse_quote! {
            #[must_use = "this is a recipe for constructing a C++ object, and constructs nothing until it is stored somewhere: finish it with .within_unique_ptr(), .within_box(), or the moveit! macro"]
        }),
        Some(Fallibility::Fallible) => Some(parse_quote! {
            #[must_use = "this is a recipe for constructing a C++ object, and constructs nothing until it is stored somewhere: finish it with .try_within_unique_ptr(), .try_within_box(), or a stack_slot! and .try_emplace()"]
        }),
    }
}

/// Whether the constructor behind an `impl New`/`impl TryNew` can fail.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Fallibility {
    Infallible,
    Fallible,
}

/// Whether this is one of the `-> impl autocxx::moveit::new::New<Output = T>`
/// return types we synthesize - either for a constructor or for a function
/// which returns a non-POD type by value - or the
/// `-> impl ...TryNew<Output = T, Error = cxx::Exception>` a `throws!` one gets
/// instead; and which of the two.
///
/// This inspects the return type we ended up with rather than tracking how we
/// got there, because several later steps rewrite it: an explicit lifetime may
/// be added to the bound, and a return type conversion may replace it outright.
fn returns_impl_new(ret_type: &ReturnType) -> Option<Fallibility> {
    let ty = match ret_type {
        ReturnType::Default => return None,
        ReturnType::Type(_, ty) => &**ty,
    };
    impl_new_fallibility(ty)
}

/// Whether this type is `impl ...New<..>` or `impl ...TryNew<..>`, whatever
/// else it is bounded by.
fn impl_new_fallibility(ty: &syn::Type) -> Option<Fallibility> {
    let bounds = match ty {
        syn::Type::ImplTrait(imp) => &imp.bounds,
        _ => return None,
    };
    bounds.iter().find_map(|bound| match bound {
        syn::TypeParamBound::Trait(t) => match t.path.segments.last() {
            Some(seg) if seg.ident == "New" => Some(Fallibility::Infallible),
            Some(seg) if seg.ident == "TryNew" => Some(Fallibility::Fallible),
            _ => None,
        },
        _ => None,
    })
}

/// The return type for a function which builds its result into a place the
/// caller supplies - a constructor, or a function returning a non-POD type by
/// value.
///
/// A constructor which cannot fail hands back a [`moveit::new::New`], whose
/// `new` returns nothing and must leave the place initialized. One which can -
/// a `throws!` one - hands back a [`moveit::new::TryNew`] instead, whose
/// `try_new` returns a `Result` and leaves the place untouched when it is
/// `Err`. There is no way to express the same thing as `Result<impl New, _>`:
/// that would decide whether construction fails before it has been attempted,
/// whereas a C++ constructor only throws once it is running.
fn placement_return_type(output: &syn::Type, may_throw: bool) -> ReturnType {
    if may_throw {
        parse_quote! {
            -> impl autocxx::moveit::new::TryNew<Output = #output, Error = cxx::Exception>
        }
    } else {
        parse_quote! {
            -> impl autocxx::moveit::new::New<Output = #output>
        }
    }
}

/// Knows how to generate a given function.
#[derive(Clone)]
struct FnGenerator<'a> {
    param_details: &'a [ArgumentAnalysis],
    ret_conversion: &'a Option<TypeConversionPolicy>,
    ret_type: &'a ReturnType,
    cxxbridge_name: &'a Ident,
    rust_name: &'a str,
    unsafety: &'a UnsafetyNeeded,
    doc_attrs: &'a Vec<Attribute>,
    /// `#[deprecated]` carrying the message C++ wrote, where C++ marked the
    /// function deprecated. It goes on the Rust item a caller names, and
    /// deliberately not on the `cxx::bridge` declaration: the wrapper here
    /// calls that declaration, so the attribute there would warn inside
    /// autocxx's own generated code rather than at the call the user wrote.
    deprecation: &'a Option<Attribute>,
    non_pod_types: &'a HashSet<QualifiedName>,
    may_throw: bool,
}

impl<'a> FnGenerator<'a> {
    fn common_parts<'b>(
        &'b self,
        avoid_self: bool,
        parameter_reordering: &Option<Vec<usize>>,
        ret_type: Option<ReturnType>,
    ) -> (
        Option<TokenStream>,
        Punctuated<FnArg, Comma>,
        std::borrow::Cow<'b, ReturnType>,
        TokenStream,
    ) {
        let mut wrapper_params: Punctuated<FnArg, Comma> = Punctuated::new();
        let mut local_variables = Vec::new();
        let mut arg_list = Vec::new();
        let mut ptr_arg_name = None;
        let mut ret_type: Cow<'a, _> = ret_type
            .map(Cow::Owned)
            .unwrap_or_else(|| Cow::Borrowed(self.ret_type));
        let mut any_conversion_requires_unsafe = false;
        let mut variable_counter = 0usize;
        for pd in self.param_details {
            let wrapper_arg_name: syn::Pat = if pd.self_type.is_some() && !avoid_self {
                parse_quote!(self)
            } else {
                pd.name.clone().into()
            };
            let rust_for_param = pd
                .conversion
                .rust_conversion(parse_quote! { #wrapper_arg_name }, &mut variable_counter);
            match rust_for_param {
                RustParamConversion::Param {
                    ty,
                    conversion,
                    local_variables: mut these_local_variables,
                    conversion_requires_unsafe,
                } => {
                    arg_list.push(conversion.clone());
                    local_variables.append(&mut these_local_variables);
                    if pd.is_placement_return_destination {
                        ptr_arg_name = Some(conversion);
                    } else {
                        let param_mutability = pd.conversion.requires_mutability();
                        wrapper_params.push(parse_quote!(
                            #param_mutability #wrapper_arg_name: #ty
                        ));
                    }
                    any_conversion_requires_unsafe =
                        conversion_requires_unsafe || any_conversion_requires_unsafe;
                }
                RustParamConversion::ReturnValue { ty } => {
                    ptr_arg_name = Some(pd.name.to_token_stream());
                    ret_type = Cow::Owned(placement_return_type(&ty, self.may_throw));
                    arg_list.push(pd.name.to_token_stream());
                }
            }
        }
        if let Some(parameter_reordering) = &parameter_reordering {
            wrapper_params = Self::reorder_parameters(wrapper_params, parameter_reordering);
        }
        let (lifetime_tokens, wrapper_params, ret_type) = add_explicit_lifetime_if_necessary(
            self.param_details,
            wrapper_params,
            ret_type,
            self.non_pod_types,
            self.ret_conversion,
        );

        let cxxbridge_name = self.cxxbridge_name;
        // Whether this function builds its result into a place the caller
        // supplies, rather than returning it: a constructor, or a function
        // returning a non-POD type by value. Such a function's wrapper hands
        // back a recipe (a `New` or a `TryNew`) rather than a value, which
        // changes where a throwing call's `Result` has to end up.
        let is_placement_return = ptr_arg_name.is_some();
        // If the function may throw, the bridge returns Result<T>.
        // Use ? to propagate errors when there's additional work to do - except
        // for a placement return, where the closure below is itself required to
        // return `Result<(), cxx::Exception>` and so hands the bridge's own
        // `Result` straight back.
        let bridge_call = if self.may_throw && !is_placement_return {
            quote! {
                cxxbridge::#cxxbridge_name ( #(#arg_list),* )?
            }
        } else {
            quote! {
                cxxbridge::#cxxbridge_name ( #(#arg_list),* )
            }
        };
        let call_body = MaybeUnsafeStmt::maybe_unsafe(
            bridge_call,
            any_conversion_requires_unsafe
                || matches!(
                    self.unsafety,
                    UnsafetyNeeded::JustBridge | UnsafetyNeeded::Always
                ),
        );
        // RFC 2585: an unsafe operation needs its own `unsafe` block even
        // inside an `unsafe fn`, so the enclosing function's unsafety never
        // counts as permission here and the surrounding context is always
        // treated as safe.
        let context_is_unsafe = false;
        let (call_body, ret_type) = match self.ret_conversion {
            Some(ret_conversion) if ret_conversion.rust_work_needed() => {
                // There's a potential lurking bug below. If the return type conversion requires
                // unsafe, then we'll end up doing something like
                //   unsafe { do_return_conversion( unsafe { call_body() })}
                // and the generated code will get warnings about nested unsafe blocks.
                // That's because we convert the call body to tokens in the following
                // line without considering the fact it's embedded in another expression.
                // At the moment this is OK because no return type conversions require
                // unsafe, but if this happens in future, we should do:
                //   let temp_ret_val = unsafe { call_body() };
                //   do_return_conversion(temp_ret_val)
                // by returning a vector of MaybeUnsafes within call_body.
                let expr = maybe_unsafes_to_tokens(vec![call_body], context_is_unsafe);
                let conv =
                    ret_conversion.rust_conversion(parse_quote! { #expr }, &mut variable_counter);
                let (conversion, requires_unsafe, ty) = match conv {
                    RustParamConversion::Param {
                        local_variables, ..
                    } if !local_variables.is_empty() => panic!("return type required variables"),
                    RustParamConversion::Param {
                        conversion,
                        conversion_requires_unsafe,
                        ty,
                        ..
                    } => (conversion, conversion_requires_unsafe, ty),
                    _ => panic!(
                        "Unexpected - return type is supposed to be converted to a return type"
                    ),
                };
                (
                    if requires_unsafe {
                        MaybeUnsafeStmt::NeedsUnsafe(conversion)
                    } else {
                        MaybeUnsafeStmt::Normal(conversion)
                    },
                    Cow::Owned(parse_quote! { -> #ty }),
                )
            }
            _ => (call_body, ret_type),
        };

        // A placement return goes inside a closure which `by_raw` (or, when the
        // constructor may throw, `try_by_raw`) turns into the `New`/`TryNew`
        // this function hands back. `by_raw`'s closure returns `()`;
        // `try_by_raw`'s returns `Result<(), cxx::Exception>`, which is exactly
        // what the bridge call already produces, so the two differ only in the
        // factory named here.
        let call_stmts = if let Some(ptr_arg_name) = ptr_arg_name {
            let mut closure_stmts = local_variables;
            closure_stmts.push(MaybeUnsafeStmt::binary(
                quote! { let #ptr_arg_name = unsafe { #ptr_arg_name.get_unchecked_mut().as_mut_ptr() };},
                quote! { let #ptr_arg_name = #ptr_arg_name.get_unchecked_mut().as_mut_ptr();},
            ));
            closure_stmts.push(call_body);
            let closure_stmts = maybe_unsafes_to_tokens(closure_stmts, true);
            let factory = if self.may_throw {
                quote! { autocxx::moveit::new::try_by_raw }
            } else {
                quote! { autocxx::moveit::new::by_raw }
            };
            vec![MaybeUnsafeStmt::needs_unsafe(parse_quote! {
                #factory(move |#ptr_arg_name| {
                    #closure_stmts
                })
            })]
        } else {
            let mut call_stmts = local_variables;
            call_stmts.push(call_body);
            call_stmts
        };
        let call_body = maybe_unsafes_to_tokens(call_stmts, context_is_unsafe);

        // If the function may throw, wrap the call body in Ok() and the return
        // type in Result. A placement return is exempt: its fallibility is
        // already carried by the `TryNew` the branch above produced, and
        // wrapping that in a `Result` would claim construction had failed
        // before it was attempted.
        let (call_body, ret_type) = if self.may_throw && !is_placement_return {
            let wrapped_body = quote! { Ok(#call_body) };
            let wrapped_ret_type = match ret_type.as_ref() {
                ReturnType::Default => Cow::Owned(parse_quote! { -> Result<(), cxx::Exception> }),
                ReturnType::Type(arrow, ty) => {
                    Cow::Owned(parse_quote! { #arrow Result<#ty, cxx::Exception> })
                }
            };
            (wrapped_body, wrapped_ret_type)
        } else {
            (call_body, ret_type)
        };

        (lifetime_tokens, wrapper_params, ret_type, call_body)
    }

    /// Generate an 'impl Type { methods-go-here }' item
    fn generate_method_impl(
        &self,
        avoid_self: bool,
        impl_block_type_name: &QualifiedName,
    ) -> Box<ImplBlockDetails> {
        let (lifetime_tokens, wrapper_params, ret_type, call_body) =
            self.common_parts(avoid_self, &None, None);
        let rust_name = make_ident(self.rust_name);
        let unsafety = self.unsafety.wrapper_token();
        let doc_attrs = self.doc_attrs;
        let deprecation = self.deprecation;
        let must_use = must_use_attr_if_impl_new(&ret_type);
        let ty = impl_block_type_name.get_final_ident();
        Box::new(ImplBlockDetails {
            item: ImplItem::Fn(parse_quote! {
                #(#doc_attrs)*
                #deprecation
                #must_use
                pub #unsafety fn #rust_name #lifetime_tokens ( #wrapper_params ) #ret_type {
                    #call_body
                }
            }),
            ty: parse_quote! { # ty },
        })
    }

    /// Generate an 'impl Trait for Type { methods-go-here }' in its entrety.
    ///
    /// Unlike the other generators here this one adds no `#[must_use]`, even
    /// when the method hands back an `impl New`: rustc rejects the attribute on
    /// a trait method in an impl block (`unused_attributes`, on its way to
    /// becoming a hard error). Today no such method returns one - the moveit
    /// traits construct into a placement parameter and return `()` - so nothing
    /// is lost, but if one ever does, its callers will not be warned.
    ///
    /// It adds no `#[deprecated]` either, for the same reason: rustc ignores
    /// the attribute on a trait impl item and says so through
    /// `useless_deprecated`, also on its way to becoming a hard error. A
    /// deprecated special member - a copy or move constructor, a destructor -
    /// therefore reaches Rust unmarked. The generated C++ still gets the
    /// pragma which keeps its own build clean.
    fn generate_trait_impl(&self, details: &TraitMethodDetails) -> Box<TraitImplBlockDetails> {
        let (lifetime_tokens, wrapper_params, ret_type, call_body) =
            self.common_parts(details.avoid_self, &details.parameter_reordering, None);
        let doc_attrs = self.doc_attrs;
        let unsafety = self.unsafety.wrapper_token();
        let key = details.trt.clone();
        let method_name = &details.method_name;
        let item = parse_quote! {
            #(#doc_attrs)*
            #unsafety fn #method_name #lifetime_tokens ( #wrapper_params ) #ret_type {
                #call_body
            }
        };
        Box::new(TraitImplBlockDetails { item, key })
    }

    /// Generate a 'impl Type { methods-go-here }' item which is a constructor
    /// for use with moveit traits.
    fn generate_constructor_impl(
        &self,
        impl_block_type_name: &QualifiedName,
    ) -> Box<ImplBlockDetails> {
        let ret_type = placement_return_type(&parse_quote! { Self }, self.may_throw);
        let (lifetime_tokens, wrapper_params, ret_type, call_body) =
            self.common_parts(true, &None, Some(ret_type));
        let rust_name = make_ident(self.rust_name);
        let doc_attrs = self.doc_attrs;
        let deprecation = self.deprecation;
        let unsafety = self.unsafety.wrapper_token();
        let must_use = must_use_attr_if_impl_new(&ret_type);
        let ty = impl_block_type_name.get_final_ident();
        let ty = parse_quote! { #ty };
        let stuff = quote! {
                #(#doc_attrs)*
                #deprecation
                #must_use
                pub #unsafety fn #rust_name #lifetime_tokens ( #wrapper_params ) #ret_type {
                    #call_body
                }
        };
        Box::new(ImplBlockDetails {
            item: ImplItem::Fn(parse_quote! { #stuff }),
            ty,
        })
    }

    /// Generate a function call wrapper
    fn generate_function_impl(&self) -> Item {
        let (lifetime_tokens, wrapper_params, ret_type, call_body) =
            self.common_parts(false, &None, None);
        let rust_name = make_ident(self.rust_name);
        let doc_attrs = self.doc_attrs;
        let deprecation = self.deprecation;
        let unsafety = self.unsafety.wrapper_token();
        let must_use = must_use_attr_if_impl_new(&ret_type);
        Item::Fn(parse_quote! {
            #(#doc_attrs)*
            #deprecation
            #must_use
            pub #unsafety fn #rust_name #lifetime_tokens ( #wrapper_params ) #ret_type {
                #call_body
            }
        })
    }

    fn reorder_parameters(
        params: Punctuated<FnArg, Comma>,
        parameter_ordering: &[usize],
    ) -> Punctuated<FnArg, Comma> {
        let old_params = params.into_iter().collect::<Vec<_>>();
        parameter_ordering
            .iter()
            .map(|n| old_params.get(*n).unwrap().clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{must_use_attr_if_impl_new, placement_return_type};
    use quote::ToTokens;
    use syn::{parse_quote, ReturnType};

    fn must_use_message(ret_type: ReturnType) -> Option<String> {
        must_use_attr_if_impl_new(&ret_type).map(|attr| attr.to_token_stream().to_string())
    }

    fn is_must_use(ret_type: ReturnType) -> bool {
        must_use_message(ret_type).is_some()
    }

    #[test]
    fn impl_new_returns_are_marked() {
        // What a constructor gets.
        assert!(is_must_use(
            parse_quote! { -> impl autocxx::moveit::new::New<Output=Self> }
        ));
        // What a function returning a non-POD type by value gets, after
        // `add_explicit_lifetime_if_necessary` has been at it.
        assert!(is_must_use(
            parse_quote! { -> impl autocxx::moveit::new::New<Output = Bob> + 'a }
        ));
    }

    /// A throwing constructor's `TryNew` is finished with different methods
    /// from a `New`, so it gets a message naming those instead.
    #[test]
    fn impl_try_new_returns_are_marked_with_the_fallible_finishers() {
        let message = must_use_message(
            parse_quote! { -> impl autocxx::moveit::new::TryNew<Output=Self, Error = cxx::Exception> },
        )
        .expect("a TryNew return should be must_use");
        assert!(
            message.contains("try_within_unique_ptr"),
            "expected the fallible finishers to be named, got: {message}"
        );
        // And the lifetime-annotated form a non-POD by-value return gets.
        assert!(is_must_use(
            parse_quote! { -> impl autocxx::moveit::new::TryNew<Output = Bob, Error = cxx::Exception> + 'a }
        ));
    }

    #[test]
    fn ordinary_returns_are_left_alone() {
        assert!(!is_must_use(ReturnType::Default));
        assert!(!is_must_use(parse_quote! { -> u32 }));
        assert!(!is_must_use(parse_quote! { -> cxx::UniquePtr<Bob> }));
        // A `Result` of something else is already `#[must_use]` by virtue of
        // being a `Result`, and has no object to emplace.
        assert!(!is_must_use(
            parse_quote! { -> Result<u32, cxx::Exception> }
        ));
        // Not every `impl Trait` we could ever return is a `New`.
        assert!(!is_must_use(
            parse_quote! { -> impl std::iter::Iterator<Item = u32> }
        ));
    }

    #[test]
    fn placement_returns_track_fallibility() {
        let infallible = placement_return_type(&parse_quote! { Self }, false);
        assert_eq!(
            infallible.to_token_stream().to_string(),
            quote::quote! { -> impl autocxx::moveit::new::New<Output = Self> }.to_string()
        );
        let fallible = placement_return_type(&parse_quote! { Self }, true);
        assert_eq!(
            fallible.to_token_stream().to_string(),
            quote::quote! {
                -> impl autocxx::moveit::new::TryNew<Output = Self, Error = cxx::Exception>
            }
            .to_string()
        );
    }
}
