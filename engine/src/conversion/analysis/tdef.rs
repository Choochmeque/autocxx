// Copyright 2021 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;

use autocxx_parser::IncludeCppConfig;
use syn::{ItemType, Type};

use crate::{
    conversion::{
        analysis::type_converter::{
            add_analysis, Annotated, TypeConversionContext, TypeConverter, TypeKind,
        },
        api::{
            AnalysisPhase, Api, ApiName, NestedCppNames, NullPhase, OpaqueTypedefReason,
            TypedefKind,
        },
        apivec::ApiVec,
        check_for_fatal_attrs,
        convert_error::{ConvertErrorWithContext, ErrorContext},
        error_reporter::convert_apis,
        type_helpers::unwrap_function_pointer,
        ConvertErrorFromCpp,
    },
    types::QualifiedName,
    ParseCallbackResults,
};

#[derive(std::fmt::Debug)]
pub(crate) struct TypedefAnalysis {
    pub(crate) kind: TypedefKind,
    /// What sort of thing the converted target turned out to be. Kept because
    /// converting throws the distinction away in one case: a C++ rvalue
    /// reference and a C++ pointer both end up as a Rust pointer, and whoever
    /// later uses this alias has no other way to tell which it was. See
    /// google/autocxx#1363.
    pub(crate) target_kind: TypeKind,
    /// Whether the target was `const` in its own right - `typedef const int
    /// ci`. Kept for the same reason as `target_kind`: converting throws it
    /// away, because Rust has no way to spell it, and a field or return which
    /// later names this alias would otherwise never learn that C++ made it
    /// unassignable.
    pub(crate) target_is_const: bool,
    pub(crate) deps: HashSet<QualifiedName>,
}

/// Analysis phase where typedef analysis has been performed but no other
/// analyses just yet.
#[derive(std::fmt::Debug)]
pub(crate) struct TypedefPhase;

impl AnalysisPhase for TypedefPhase {
    type TypedefAnalysis = TypedefAnalysis;
    type StructAnalysis = ();
    type FunAnalysis = ();
    type SubclassAnalysis = ();
}

#[allow(clippy::needless_collect)] // we need the extra collect because the closure borrows extra_apis
pub(crate) fn convert_typedef_targets(
    config: &IncludeCppConfig,
    apis: ApiVec<NullPhase>,
    parse_callback_results: &ParseCallbackResults,
) -> ApiVec<TypedefPhase> {
    let mut type_converter = TypeConverter::new(config, &apis);
    let mut extra_apis = ApiVec::new();
    let mut results = ApiVec::new();
    convert_apis(
        apis,
        &mut results,
        Api::fun_unchanged,
        Api::struct_unchanged,
        Api::enum_unchanged,
        |name, item, old_tyname, _| {
            Ok(Box::new(std::iter::once(match item {
                TypedefKind::Type(ity) => get_replacement_typedef(
                    config,
                    name,
                    (*ity).into(),
                    old_tyname,
                    &mut type_converter,
                    &mut extra_apis,
                    parse_callback_results,
                )?,
                TypedefKind::Use { .. } => Api::Typedef {
                    name,
                    item: item.clone(),
                    old_tyname,
                    analysis: TypedefAnalysis {
                        kind: item,
                        // A `use` names a type and is passed through
                        // unconverted, so there is nothing here that a caller
                        // couldn't read off the type itself.
                        target_kind: TypeKind::Regular,
                        target_is_const: false,
                        deps: HashSet::new(),
                    },
                },
            })))
        },
        Api::subclass_unchanged,
    );
    results.extend(extra_apis.into_iter().map(add_analysis));
    ignore_typedefs_to_alias_templates(results)
}

/// An alias template reaches us as a plain typedef, because bindgen discards
/// its template parameters, and we ignore it above on either of the two things
/// bindgen tells us: that it dropped a type parameter as unused, or that the
/// declaration had parameters which are not types at all. Where bindgen left a
/// further typedef pointing at such an alias template - rather than resolving
/// through it to a concrete type - that typedef names a template which still
/// wants arguments, so we must ignore it as well: cxx would otherwise emit a
/// forward declaration naming the alias template without its template
/// arguments, which isn't valid C++. `test_alias_template_typedef_ignored` is
/// what this exists for: an ordinary `typedef f<int> g;` over an alias template
/// `f`. A chain of alias templates is now refused hop by hop instead.
/// See google/autocxx#1094 and google/autocxx#1501.
///
/// A `TypedefKind::Use` - which is what bindgen emits for an alias to an enum -
/// never reaches the check above, so such an alias template is not refused.
/// Probing `enum class E` aliased by `template <int N> using A = E;`, used as a
/// field and not, produces valid output either way, so it is left alone.
/// This must happen here, rather than later, because at this point the only
/// items ignored for this reason are typedefs.
fn ignore_typedefs_to_alias_templates(mut apis: ApiVec<TypedefPhase>) -> ApiVec<TypedefPhase> {
    // Propagate to a fixed point: each newly ignored typedef can in
    // turn invalidate typedefs which point at *it* bare (chains of
    // erased alias templates wrap each other). Terminates because
    // every round strictly shrinks the set of Typedef apis.
    loop {
        let alias_templates: HashSet<QualifiedName> = apis
            .iter()
            .filter_map(|api| match api {
                Api::IgnoredItem {
                    err:
                        ConvertErrorFromCpp::UnusedTemplateParam
                        | ConvertErrorFromCpp::AliasTemplate { .. },
                    ..
                } => Some(api.name()),
                _ => None,
            })
            .cloned()
            .collect();
        let mut changed = false;
        apis = apis
            .into_iter()
            .map(|api| match api {
                Api::Typedef {
                    ref name,
                    analysis: TypedefAnalysis { ref deps, .. },
                    ..
                } if !alias_templates.is_disjoint(deps) => {
                    changed = true;
                    Api::IgnoredItem {
                        name: api.name_info().clone(),
                        err: ConvertErrorFromCpp::UnusedTemplateParam,
                        ctx: Some(ErrorContext::new_for_item(name.name.get_final_ident())),
                    }
                }
                _ => api,
            })
            .collect();
        if !changed {
            return apis;
        }
    }
}

fn get_replacement_typedef(
    config: &IncludeCppConfig,
    name: ApiName,
    ity: ItemType,
    old_tyname: Option<QualifiedName>,
    type_converter: &mut TypeConverter,
    extra_apis: &mut ApiVec<NullPhase>,
    parse_callback_results: &ParseCallbackResults,
) -> Result<Api<TypedefPhase>, ConvertErrorWithContext> {
    if !ity.generics.params.is_empty() {
        return Err(ConvertErrorWithContext(
            ConvertErrorFromCpp::TypedefTakesGenericParameters,
            Some(ErrorContext::new_for_item(name.name.get_final_ident())),
        ));
    }
    let mut converted_type = ity.clone();
    check_for_fatal_attrs(parse_callback_results, &name.name)?;
    // A C++ alias template whose parameters bindgen could not represent - a
    // non-type parameter, say - reaches us as a plain `pub type X = Y;`, which
    // makes `X` look like an ordinary typedef even though naming it in C++
    // still takes template arguments. cxx would emit C++ naming `X` bare, which
    // is `use of alias template 'X' requires template arguments`. See
    // google/autocxx#1094. Where the dropped parameter was a *type* parameter
    // bindgen says so itself, and `check_for_fatal_attrs` above has already
    // turned that down.
    if let Some(params) = parse_callback_results.alias_template_params(&name.name) {
        if params.declared > params.type_params {
            return Err(ConvertErrorWithContext(
                ConvertErrorFromCpp::AliasTemplate {
                    declared: params.declared,
                    type_params: params.type_params,
                },
                Some(ErrorContext::new_for_item(name.name.get_final_ident())),
            ));
        }
    }
    // A typedef to a C function pointer. bindgen writes the target as
    // `Option<unsafe extern "C" fn(..)>`, which names no C++ type at all and
    // so gives the type converter nothing to do; put through it, the `Option`
    // would be taken for a C++ type we know nothing about and the typedef
    // discarded. Keep it exactly as bindgen wrote it, so that a struct with a
    // field of this type can still be POD. A typedef is only ever re-exported
    // from the bindgen module, never declared to cxx, so nothing downstream
    // has to be able to spell it. See google/autocxx#1494.
    if matches!(&*ity.ty, syn::Type::Path(typ) if unwrap_function_pointer(typ).is_some()) {
        return Ok(Api::Typedef {
            name,
            item: TypedefKind::Type(Box::new(converted_type.clone().into())),
            old_tyname,
            analysis: TypedefAnalysis {
                kind: TypedefKind::Type(Box::new(converted_type.into())),
                // What the type converter says of a function pointer wherever
                // one is allowed: it behaves as a pointer in every way the
                // later analyses ask about.
                target_kind: TypeKind::Pointer,
                // A pointer to a function, not a `const` one: the marker would
                // have wrapped the `Option<..>` and `function_pointer` would
                // not have recognised it.
                target_is_const: false,
                deps: HashSet::new(),
            },
        });
    }
    let type_conversion_results = type_converter.convert_type(
        (*ity.ty).clone(),
        name.name.get_namespace(),
        // A typedef is one API with no single place of use, so there is no
        // context to inherit. Where the answer has to be given now it is given
        // conservatively - a typedef whose target bindgen could only express as
        // an opaque blob is refused here, which the next arm turns into an
        // opaque type rather than an alias for some unrelated integer - and
        // where it can wait for a use, it waits.
        &TypeConversionContext::WithinTypedef,
    );
    match type_conversion_results {
        // bindgen could not name the type this typedef points at, so it gave
        // us a blob of bytes of the right size instead. The typedef itself has
        // a name, though, which is all an opaque type needs, so keep it as
        // one: `void f(const Alias&)` then works, where aliasing the blob would
        // have made it `f(&u8)` and put a lie in the bindings. This is the same
        // treatment `replace_hopeless_typedef_targets` gives a typedef whose
        // target autocxx had to ignore, and it carries the same caveat: cxx
        // cannot declare an opaque type nested inside another, so a nested one
        // still has to go.
        //
        // The reason travels with the stand-in, so that anything which goes on
        // to use the typedef in a position an opaque type can't fill is told
        // what was actually wrong. Its culprit is the typedef's own name,
        // which is a little circular and is nonetheless the truth: bindgen
        // erased the type this names, so the alias is the only name it has
        // left. `ConvertErrorFromCpp::TypeContainingUngeneratableTypedef`
        // knows to say it once rather than twice.
        Err(err @ ConvertErrorFromCpp::BindgenOpaqueBlob(_)) if !name.cpp_name().is_nested() => {
            let reason = OpaqueTypedefReason {
                culprit: name.name.clone(),
                reason: Box::new(err),
            };
            Ok(Api::OpaqueTypedef {
                forward_declaration: !config.instantiable.contains(&name.name.to_cpp_name()),
                name,
                reason: Some(reason),
            })
        }
        Err(err) => Err(ConvertErrorWithContext(
            err,
            Some(ErrorContext::new_for_item(name.name.get_final_ident())),
        )),
        Ok(Annotated {
            ty: syn::Type::Path(ref typ),
            ..
        }) if QualifiedName::from_type_path(typ) == name.name => Err(ConvertErrorWithContext(
            ConvertErrorFromCpp::InfinitelyRecursiveTypedef(name.name.clone()),
            Some(ErrorContext::new_for_item(name.name.get_final_ident())),
        )),
        Ok(mut final_type) => {
            converted_type.ty = Box::new(final_type.ty.clone());
            extra_apis.append(&mut final_type.extra_apis);
            Ok(Api::Typedef {
                name,
                item: TypedefKind::Type(Box::new(ity.into())),
                old_tyname,
                analysis: TypedefAnalysis {
                    kind: TypedefKind::Type(Box::new(converted_type.into())),
                    target_kind: final_type.kind,
                    target_is_const: final_type.is_const,
                    deps: final_type.types_encountered,
                },
            })
        }
    }
}

/// Where each typedef in `apis` points.
///
/// A base class or a field may be named by a typedef - `typedef Base Alias;
/// struct D : Alias {};` - and bindgen names the typedef both in the field it
/// generates for such a base and in the base it reports. C++'s rules about
/// special members and about pure virtuals run over the class the typedef
/// names, so the analyses which run them have to follow it.
pub(crate) fn typedef_targets<P: AnalysisPhase<TypedefAnalysis = TypedefAnalysis>>(
    apis: &ApiVec<P>,
) -> HashMap<QualifiedName, QualifiedName> {
    apis.iter()
        .filter_map(|api| match api {
            Api::Typedef {
                name,
                analysis: TypedefAnalysis { kind, .. },
                ..
            } => {
                let target = match kind {
                    TypedefKind::Type(type_item) => type_item.ty.as_ref(),
                    TypedefKind::Use(ty) => ty,
                };
                match target {
                    Type::Path(typ) => {
                        Some((name.name.clone(), QualifiedName::from_type_path(typ)))
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

/// The concrete template instantiations an `instantiable!` directive says
/// Rust may own: those it names outright, and those a typedef it names
/// resolves to.
///
/// Only these are given the special members and allocators of
/// google/autocxx#723, and the reason it takes a directive is that bindgen
/// reports *nothing* about a specialization - not its members, not its
/// bases, not one constructor it declares - so autocxx has no evidence for
/// which special members C++ actually gives it and can only claim them and
/// let the compiler arbitrate. Claiming them for every instantiation which
/// happened to pass through a signature emits C++ which the class may not
/// permit, and three shapes in the test suite prove it: a template declaring
/// a constructor of its own has no default one
/// (`test_cycle_generic_type`), and a `const` template argument makes a
/// `const` member, which deletes it
/// (`test_template_class_with_const_record_argument_is_its_own_type` and its
/// two siblings). All three are valid C++ which used to build.
///
/// `instantiable!` is the user saying they know better, which is what it has
/// always meant - see its documentation - so this widens that directive
/// rather than inventing one.
///
/// Excludes the opaque holders autocxx lowers a smart pointer to: what one of
/// those wraps is made and destroyed by the shims beside it, and a
/// default-constructed `std::shared_ptr` owns nothing at all.
pub(crate) fn instantiable_concrete_types<P: AnalysisPhase<TypedefAnalysis = TypedefAnalysis>>(
    apis: &ApiVec<P>,
    config: &IncludeCppConfig,
) -> HashSet<QualifiedName> {
    // A `concrete!` type answers to the C++ expression the user wrote for it
    // as well as to the Rust name they gave it, since they wrote both in the
    // one directive.
    let concrete: HashMap<QualifiedName, Option<&str>> = apis
        .iter()
        .filter_map(|api| match api {
            Api::ConcreteType {
                name,
                cpp_definition,
                holder_surface: None,
                ..
            } => Some((name.name.clone(), Some(cpp_definition.as_str()))),
            _ => None,
        })
        .collect();
    if concrete.is_empty() {
        return HashSet::new();
    }
    let nested_cpp_names = NestedCppNames::new(config, apis.iter().map(|api| api.name_info()));
    let spellings = |name: &QualifiedName, extra: Option<&str>| -> Vec<String> {
        nested_cpp_names
            .spellings(name)
            .chain(extra.map(ToString::to_string))
            .collect()
    };
    // Every name which resolves to an instantiation gets a say, and a
    // `block_constructors!` on any of them beats an `instantiable!` on any
    // other: an instantiation may be named by several aliases, all of which
    // are now the same Rust type, so a block written against one of them would
    // otherwise be lifted by a permission written against another.
    let mut instantiable: HashSet<QualifiedName> = HashSet::new();
    let mut blocked: HashSet<QualifiedName> = HashSet::new();
    let mut consider = |target: &QualifiedName, spellings: Vec<String>| {
        if spellings
            .iter()
            .any(|spelling| config.instantiable.contains(spelling))
        {
            instantiable.insert(target.clone());
        }
        if spellings
            .iter()
            .any(|spelling| config.is_on_constructor_blocklist(spelling))
        {
            blocked.insert(target.clone());
        }
    };
    for (name, cpp_definition) in &concrete {
        consider(name, spellings(name, *cpp_definition));
    }
    let targets = typedef_targets(apis);
    for api in apis.iter() {
        if let Api::Typedef { name, .. } = api {
            let target = resolve_typedefs(&targets, &name.name);
            if concrete.contains_key(&target) {
                consider(&target, spellings(&name.name, None));
            }
        }
    }
    instantiable.retain(|name| !blocked.contains(name));
    instantiable
}

/// Follow `targets` from `name` to the type it finally names.
///
/// A chain of typedefs is finite, but nothing proves the map built above is
/// acyclic, so this stops after as many steps as there are typedefs and
/// answers with whatever it reached.
pub(crate) fn resolve_typedefs(
    targets: &HashMap<QualifiedName, QualifiedName>,
    name: &QualifiedName,
) -> QualifiedName {
    let mut name = name.clone();
    for _ in 0..targets.len() {
        match targets.get(&name) {
            Some(target) => name = target.clone(),
            None => break,
        }
    }
    name
}
