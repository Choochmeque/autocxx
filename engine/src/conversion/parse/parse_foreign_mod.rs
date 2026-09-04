// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::conversion::api::{ApiName, NullPhase, Provenance};
use crate::conversion::apivec::ApiVec;
use crate::conversion::doc_attr::get_doc_attrs;
use crate::conversion::error_reporter::report_any_error;
use crate::conversion::{
    api::{FuncToConvert, UnanalyzedApi},
    convert_error::ConvertErrorWithContext,
    convert_error::ErrorContext,
};
use crate::minisyn::{minisynize_punctuated, minisynize_vec};
use crate::types::strip_bindgen_original_suffix_from_ident;
use crate::ParseCallbackResults;
use crate::{
    conversion::ConvertErrorFromCpp,
    types::{Namespace, QualifiedName},
};
use std::collections::HashMap;
use syn::{
    Attribute, Block, Expr, ExprCall, ExprLit, ForeignItem, Ident, ImplItem, ItemImpl, Lit, Meta,
    MetaNameValue, Stmt, Type,
};

use super::linkage::{linkage_from_link_name, CppLinkage};
use super::parse_bindgen::api_name;
use super::ref_qualifier::{ref_qualifier_from_mangled_name, CppRefQualifier};

/// Parses a given bindgen-generated 'mod' into suitable
/// [Api]s. In bindgen output, a given mod concerns
/// a specific C++ namespace.
pub(crate) struct ParseForeignMod<'a> {
    ns: Namespace,
    // We mostly act upon the functions we see within the 'extern "C"'
    // block of bindgen output, but we can't actually do this until
    // we've seen the (possibly subsequent) 'impl' blocks so we can
    // deduce which functions are actually static methods. Hence
    // store them.
    funcs_to_convert: Vec<FuncToConvert>,
    // Evidence from 'impl' blocks about which of these items
    // may actually be methods (static or otherwise). Mapping from
    // function name to type name.
    method_receivers: HashMap<Ident, QualifiedName>,
    // Variables with static storage duration which we'll re-export.
    statics: ApiVec<NullPhase>,
    ignored_apis: ApiVec<NullPhase>,
    parse_callback_results: &'a ParseCallbackResults,
}

impl<'a> ParseForeignMod<'a> {
    pub(crate) fn new(ns: Namespace, parse_callback_results: &'a ParseCallbackResults) -> Self {
        Self {
            ns,
            funcs_to_convert: Vec::new(),
            method_receivers: HashMap::new(),
            statics: ApiVec::new(),
            ignored_apis: ApiVec::new(),
            parse_callback_results,
        }
    }

    /// Record information from foreign mod items encountered
    /// in bindgen output.
    pub(crate) fn convert_foreign_mod_items(&mut self, foreign_mod_items: &Vec<ForeignItem>) {
        let mut extra_apis = ApiVec::new();
        for i in foreign_mod_items {
            report_any_error(&self.ns.clone(), &mut extra_apis, || {
                self.parse_foreign_item(i)
            });
        }
        self.ignored_apis.append(&mut extra_apis);
    }

    fn parse_foreign_item(&mut self, i: &ForeignItem) -> Result<(), ConvertErrorWithContext> {
        match i {
            ForeignItem::Fn(item) => {
                let doc_attrs = get_doc_attrs(&item.attrs);
                let unsuffixed_name = strip_bindgen_original_suffix_from_ident(&item.sig.ident);
                let qn = QualifiedName::new(&self.ns, unsuffixed_name.clone().into());
                self.funcs_to_convert.push(FuncToConvert {
                    provenance: Provenance::Bindgen,
                    self_ty: None,
                    ident: unsuffixed_name.clone().into(),
                    doc_attrs: minisynize_vec(doc_attrs),
                    inputs: minisynize_punctuated(item.sig.inputs.clone()),
                    output: item.sig.output.clone().into(),
                    vis: item.vis.clone().into(),
                    virtualness: self.parse_callback_results.get_virtualness(&qn),
                    cpp_vis: self.parse_callback_results.get_cpp_visibility(&qn),
                    special_member: self.parse_callback_results.special_member_kind(&qn),
                    original_name: self.parse_callback_results.get_fn_original_name(&qn),
                    synthesized_this_type: None,
                    add_to_trait: None,
                    is_deleted: self.parse_callback_results.get_deleted_or_defaulted(&qn),
                    synthetic_cpp: None,
                    variadic: item.sig.variadic.is_some(),
                    ref_qualifier: ref_qualifier_from_attrs(&item.attrs),
                });
                Ok(())
            }
            ForeignItem::Static(item) => {
                // A C++ variable with static storage duration. `bindgen` has
                // already declared it for us within the mod which we emit
                // verbatim, so all we need to do is note it as an API so that
                // we re-export it - see google/autocxx#93.
                let cpp_ty = analyze_static(&item.attrs, &item.ty, &item.ident).map_err(|e| {
                    ConvertErrorWithContext(
                        e,
                        Some(ErrorContext::new_for_item(item.ident.clone().into())),
                    )
                })?;
                self.statics.push(UnanalyzedApi::Static {
                    name: api_name(&self.ns, item.ident.clone(), self.parse_callback_results),
                    cpp_ty,
                });
                Ok(())
            }
            _ => Err(ConvertErrorWithContext(
                ConvertErrorFromCpp::UnexpectedForeignItem,
                None,
            )),
        }
    }

    /// Record information from impl blocks encountered in bindgen
    /// output.
    pub(crate) fn convert_impl_items(&mut self, imp: ItemImpl) {
        let ty_id = match *imp.self_ty {
            Type::Path(typ) => typ.path.segments.last().unwrap().ident.clone(),
            _ => return,
        };
        for i in imp.items {
            if let ImplItem::Fn(itm) = i {
                let effective_fun_name = match get_called_function(&itm.block) {
                    Some(id) => id.clone(),
                    None => itm.sig.ident,
                };
                let effective_fun_name =
                    strip_bindgen_original_suffix_from_ident(&effective_fun_name);
                self.method_receivers.insert(
                    effective_fun_name,
                    QualifiedName::new(&self.ns, ty_id.clone().into()),
                );
            }
        }
    }

    /// Indicate that all foreign mods and all impl blocks have been
    /// fed into us, and we should process that information to generate
    /// the resulting APIs.
    pub(crate) fn finished(mut self, apis: &mut ApiVec<NullPhase>) {
        apis.append(&mut self.ignored_apis);
        apis.append(&mut self.statics);
        while !self.funcs_to_convert.is_empty() {
            let mut fun = self.funcs_to_convert.remove(0);
            fun.self_ty = self.method_receivers.get(&fun.ident).cloned();
            apis.push(UnanalyzedApi::Function {
                name: ApiName::new_with_cpp_name(
                    &self.ns,
                    fun.ident.clone(),
                    fun.original_name.clone(),
                ),
                fun: Box::new(fun),
                analysis: (),
            })
        }
    }
}

/// Decide whether we can re-export a variable with static storage duration,
/// and if so what its C++ type is.
///
/// We can only do it if there will be a symbol to link against, and if
/// `bindgen`'s declaration of the variable names a type which our own output
/// mod exposes unchanged. The type is a path either rooted at `root` (a C++
/// type, for which `autocxx` generates its own representation) or a plain Rust
/// type such as `::std::os::raw::c_int`. In the former case we return its
/// [`QualifiedName`], so that we can later check it is POD and record a
/// dependency upon it; in the latter we return `None`, because `bindgen`'s
/// declaration is directly usable.
///
/// Any other sort of type - a pointer, a reference, an array, a function
/// pointer - is rejected, because re-exporting it would expose `bindgen`'s raw
/// view of the world rather than the types `autocxx` generates.
fn analyze_static(
    attrs: &[Attribute],
    ty: &Type,
    ident: &Ident,
) -> Result<Option<QualifiedName>, ConvertErrorFromCpp> {
    if linkage_from_link_name(link_name_from_attrs(attrs).as_deref()) == CppLinkage::Internal {
        return Err(ConvertErrorFromCpp::StaticDataWithInternalLinkage(
            ident.to_string(),
        ));
    }
    match ty {
        Type::Path(typ) => {
            let is_cpp_type = typ
                .path
                .segments
                .first()
                .is_some_and(|seg| seg.ident == "root");
            Ok(if is_cpp_type {
                Some(QualifiedName::from_type_path(typ))
            } else {
                None
            })
        }
        _ => Err(ConvertErrorFromCpp::StaticDataOfUnsupportedType),
    }
}

/// The mangled symbol name which bindgen recorded for an item, if it differed
/// from the item's Rust name.
fn link_name_from_attrs(attrs: &[Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| match &attr.meta {
        Meta::NameValue(MetaNameValue {
            path,
            value:
                Expr::Lit(ExprLit {
                    lit: Lit::Str(link_name),
                    ..
                }),
            ..
        }) if path.is_ident("link_name") => Some(link_name.value()),
        _ => None,
    })
}

/// Work out whether a function is ref-qualified (`void foo() &` /
/// `void foo() &&`).
///
/// bindgen has no idea about ref-qualifiers, so its Rust output for a
/// ref-qualified method is identical to that for an unqualified one. The
/// mangled name in the `#[link_name]` attribute is the only place the
/// information survives, so that's where we look. See
/// [`super::ref_qualifier`] for the gory details.
fn ref_qualifier_from_attrs(attrs: &[Attribute]) -> CppRefQualifier {
    link_name_from_attrs(attrs)
        .map(|link_name| ref_qualifier_from_mangled_name(&link_name))
        .unwrap_or_default()
}

/// Find the function which the body of a method in one of bindgen's `impl`
/// blocks calls, so that we can attribute that function to the type the
/// `impl` block is for. The name of the `impl` fn won't do: bindgen sometimes
/// generates an `impl` fn called `a` which calls a function called `a1()`, if
/// it's dealing with conflicting names, and it's `a1` we care about.
///
/// Most of these bodies are a single call, but the ones bindgen writes for
/// constructors first have to make somewhere to construct into:
///
/// ```ignore
/// let mut __bindgen_tmp = ::std::mem::MaybeUninit::uninit();
/// Type_Type_bindgen_original(__bindgen_tmp.as_mut_ptr());
/// __bindgen_tmp.assume_init()
/// ```
///
/// so we consider every statement, not just the first. Only an unqualified
/// call can be one of these functions, since they live in the same mod, and
/// insisting on that is what stops us mistaking `MaybeUninit::uninit()` for
/// one of them.
fn get_called_function(block: &Block) -> Option<&Ident> {
    block.stmts.iter().find_map(|stmt| match stmt {
        Stmt::Expr(Expr::Call(ExprCall { func, .. }), _) => match **func {
            Expr::Path(ref exp)
                if exp.qself.is_none()
                    && exp.path.leading_colon.is_none()
                    && exp.path.segments.len() == 1 =>
            {
                exp.path.segments.first().map(|ps| &ps.ident)
            }
            _ => None,
        },
        _ => None,
    })
}

#[cfg(test)]
mod test {
    use super::get_called_function;
    use syn::parse_quote;
    use syn::Block;

    #[test]
    fn test_get_called_function() {
        let b: Block = parse_quote! {
            {
                call_foo()
            }
        };
        assert_eq!(get_called_function(&b).unwrap().to_string(), "call_foo");
    }

    /// The shape `bindgen` gives the body of a constructor, where the call
    /// we're after is neither the first statement nor the last expression.
    #[test]
    fn test_get_called_function_in_constructor() {
        let b: Block = parse_quote! {
            {
                let mut __bindgen_tmp = ::std::mem::MaybeUninit::uninit();
                Type_Type_bindgen_original(__bindgen_tmp.as_mut_ptr());
                __bindgen_tmp.assume_init()
            }
        };
        assert_eq!(
            get_called_function(&b).unwrap().to_string(),
            "Type_Type_bindgen_original"
        );
    }

    /// A qualified call is never one of `bindgen`'s thunks, which all live in
    /// the mod the `impl` block is in.
    #[test]
    fn test_get_called_function_ignores_qualified_calls() {
        let b: Block = parse_quote! {
            {
                ::std::mem::drop(());
            }
        };
        assert!(get_called_function(&b).is_none());
    }
}
