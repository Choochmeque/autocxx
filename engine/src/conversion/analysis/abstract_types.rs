// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::vendored_bindgen::callbacks::Virtualness;
use indexmap::map::IndexMap as HashMap;
use syn::{punctuated::Punctuated, token::Comma};

use super::{
    fun::{
        FnAnalysis, FnKind, FnPhase, FnPrePhase3, MethodKind, PodAndConstructorAnalysis,
        TraitMethodKind,
    },
    pod::PodAnalysis,
};
use crate::{
    conversion::{
        analysis::{
            depth_first::fields_and_bases_first,
            fun::ReceiverMutability,
            tdef::{resolve_typedefs, typedef_targets},
        },
        api::{ApiName, TypeKind},
        error_reporter::{convert_apis, convert_item_apis},
        ConvertErrorFromCpp, CppEffectiveName,
    },
    minisyn::FnArg,
};
use crate::{
    conversion::{api::Api, apivec::ApiVec},
    types::QualifiedName,
};
use indexmap::set::IndexSet as HashSet;

#[derive(Hash, PartialEq, Eq, Clone, Debug)]
struct Signature {
    name: CppEffectiveName,
    args: Vec<syn::Type>,
    constness: ReceiverMutability,
}

impl Signature {
    fn new(
        name: &ApiName,
        params: &Punctuated<FnArg, Comma>,
        constness: ReceiverMutability,
    ) -> Self {
        Signature {
            name: name.cpp_name(),
            args: params
                .iter()
                .skip(1) // skip `this` implicit argument
                .filter_map(|p| {
                    if let syn::FnArg::Typed(t) = &p.0 {
                        Some((*t.ty).clone())
                    } else {
                        None
                    }
                })
                .collect(),
            constness,
        }
    }
}

/// A pure virtual function which some class has yet to override, kept with the
/// class which declared it pure.
///
/// The signature alone would not do. Two unrelated hierarchies can declare
/// functions of the same name and parameters, and a class inheriting both
/// carries both obligations; an override of one is not an override of the
/// other.
///
/// The declaring class is not the whole identity either. A class can hold two
/// subobjects of the *same* declaring class, reached through different virtual
/// bases - `struct B : A {}; struct C : A {}; struct L : virtual B {};
/// struct R : virtual C {};` and then `struct D : L, R {}` - and an override in
/// one branch settles only that branch's copy. Telling those apart needs the
/// path to the subobject, not just its type, which is more than this analysis
/// carries. Such a class is called concrete when C++ calls it abstract, which
/// is what it was called before bases were reported at all; the generated
/// constructor then fails to compile, naming the abstract class.
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
struct PureVirtual {
    declared_by: QualifiedName,
    signature: Signature,
}

/// Spot types with pure virtual functions and mark them abstract.
pub(crate) fn mark_types_abstract(apis: ApiVec<FnPrePhase3>) -> ApiVec<FnPrePhase3> {
    #[derive(Default, Debug, Clone)]
    struct ClassAbstractState {
        /// Pure virtuals with no overrider, declared here or reached through a
        /// base each derived class gets its own copy of. Only an override in
        /// the class itself settles one of these: two non-virtual bases of the
        /// same class are two separate subobjects, and overriding in one says
        /// nothing about the other.
        undefined: HashSet<PureVirtual>,
        /// The same, for pure virtuals reached through a virtual base. There
        /// is one such base subobject however many paths lead to it, so a
        /// class which overrides one of them does so for every class sharing
        /// that base - which is why these are kept apart.
        undefined_through_virtual_base: HashSet<PureVirtual>,
        /// Virtual functions this class defines itself.
        defined: HashSet<Signature>,
        /// The obligations settled by a definition of something reached
        /// through a virtual base: the ones this class writes and the ones its
        /// bases contribute. Named by the obligation rather than by the
        /// signature, so that an override settles the base subobject it
        /// actually belongs to and no other. An override of something reached
        /// through a *non*-virtual base is not among them at all, because it
        /// settles only that class's own copy of the base.
        overrides_through_virtual_base: HashSet<PureVirtual>,
    }
    let typedef_targets = typedef_targets(&apis);
    let mut class_states: HashMap<QualifiedName, ClassAbstractState> = HashMap::new();
    let mut abstract_classes = HashSet::new();
    let mut pure_virtual_destructors: HashSet<QualifiedName> = HashSet::new();
    let mut virtual_destructors: HashSet<QualifiedName> = HashSet::new();

    for api in apis.iter() {
        match api {
            Api::Function {
                name,
                analysis:
                    FnAnalysis {
                        kind:
                            FnKind::Method {
                                impl_for: self_ty_name,
                                method_kind,
                                ..
                            },
                        params,
                        ..
                    },
                ..
            } => match method_kind {
                MethodKind::PureVirtual(constness) => {
                    class_states
                        .entry(self_ty_name.clone())
                        .or_default()
                        .undefined
                        .insert(PureVirtual {
                            declared_by: self_ty_name.clone(),
                            signature: Signature::new(name, params, *constness),
                        });
                }
                MethodKind::Virtual(constness) => {
                    class_states
                        .entry(self_ty_name.clone())
                        .or_default()
                        .defined
                        .insert(Signature::new(name, params, *constness));
                }
                _ => {}
            },
            // A destructor never becomes a [`FnKind::Method`] - it's routed
            // to a `Drop` trait impl instead - so its virtualness never
            // reaches [`MethodKind`] and the arm above can't see it. Read it
            // off the original C++ function, which carries it for everything
            // bindgen hands us. Destructors we synthesize ourselves have no
            // virtualness, which is right: we only synthesize one when C++
            // didn't declare one, and an undeclared destructor is never
            // virtual of its own accord - only by inheriting virtualness from
            // a base, which the walk below adds.
            Api::Function {
                fun,
                analysis:
                    FnAnalysis {
                        kind:
                            FnKind::TraitMethod {
                                kind: TraitMethodKind::Destructor,
                                impl_for: self_ty_name,
                                ..
                            },
                        ..
                    },
                ..
            } if fun.virtualness.is_some() => {
                if matches!(fun.virtualness, Some(Virtualness::PureVirtual)) {
                    pure_virtual_destructors.insert(self_ty_name.clone());
                }
                virtual_destructors.insert(self_ty_name.clone());
            }
            _ => {}
        }
    }

    // A destructor is virtual if a base's is, whether or not the class
    // declares one of its own, and however many classes down the chain the
    // `virtual` was written.
    //
    // Run to a fixed point rather than in one pass over
    // `fields_and_bases_first`. That order is taken from the base names as
    // bindgen reported them, and a base named through a typedef is resolved
    // here rather than there, so a chain which passes through an alias can be
    // visited derived-class-first. Hierarchies are shallow, and the loop stops
    // the first time a pass adds nothing.
    //
    // What this can't see is a base outside the APIs - not allowlisted, or one
    // bindgen could not name (`has_unnamed_base`) - which leaves a class whose
    // destructor is virtual looking as though it isn't. The answer is
    // therefore "no virtual destructor was found", not "the destructor is not
    // virtual", and everything generated from it says so.
    loop {
        let mut found_more = false;
        for api in apis.iter() {
            if let Api::Struct {
                name,
                analysis:
                    PodAndConstructorAnalysis {
                        pod: PodAnalysis { bases, .. },
                        ..
                    },
                ..
            } = api
            {
                if !virtual_destructors.contains(&name.name)
                    && bases.iter().any(|base| {
                        virtual_destructors.contains(&resolve_typedefs(&typedef_targets, base))
                    })
                {
                    virtual_destructors.insert(name.name.clone());
                    found_more = true;
                }
            }
        }
        if !found_more {
            break;
        }
    }

    // A `TypeKind::Opaque` class is left out, and so never becomes abstract
    // however many pure virtuals bindgen reported for it. That is what
    // abstractness has always done here, and everything keyed off it inherits
    // the limitation - including the container withdrawal below, which an
    // `opaque!`d class with pure virtuals and a non-virtual destructor
    // therefore escapes.
    for api in fields_and_bases_first(apis.iter()) {
        if let Api::Struct {
            analysis:
                PodAndConstructorAnalysis {
                    pod:
                        PodAnalysis {
                            bases,
                            virtual_bases,
                            kind: TypeKind::Pod | TypeKind::NonPod,
                            ..
                        },
                    ..
                },
            name,
            ..
        } = api
        {
            // resolve virtuals for a class: start with new pure virtuals in this class
            let mut self_cs = class_states.get(&name.name).cloned().unwrap_or_default();

            // then add pure virtuals of bases
            for base in bases.iter() {
                // Whether the inheritance is virtual is recorded against the
                // name the base was reported under; which class's pure virtuals
                // those are is a question for the class that name resolves to.
                let inherited_virtually = virtual_bases.contains(base);
                let base = resolve_typedefs(&typedef_targets, base);
                if let Some(base_cs) = class_states.get(&base) {
                    // A base's own unsettled pure virtuals reach us through a
                    // shared subobject exactly when we inherit that base
                    // virtually. Ones it already held as shared stay shared,
                    // however we inherit the base itself.
                    let inherited = if inherited_virtually {
                        &mut self_cs.undefined_through_virtual_base
                    } else {
                        &mut self_cs.undefined
                    };
                    inherited.extend(base_cs.undefined.iter().cloned());
                    self_cs
                        .undefined_through_virtual_base
                        .extend(base_cs.undefined_through_virtual_base.iter().cloned());
                    self_cs
                        .overrides_through_virtual_base
                        .extend(base_cs.overrides_through_virtual_base.iter().cloned());
                }
            }

            // A definition this class writes for something it reached through
            // a virtual base is the final overrider for every class sharing
            // that base. One it writes for something reached any other way is
            // not, and stays in `defined` where only this class's own
            // subobject is settled by it.
            let overrides_shared: Vec<_> = self_cs
                .undefined_through_virtual_base
                .iter()
                .filter(|pure| self_cs.defined.contains(&pure.signature))
                .cloned()
                .collect();
            self_cs
                .overrides_through_virtual_base
                .extend(overrides_shared);

            // then remove virtuals defined in this class
            self_cs
                .undefined
                .retain(|und| !self_cs.defined.contains(&und.signature));
            self_cs
                .undefined_through_virtual_base
                .retain(|und| !self_cs.overrides_through_virtual_base.contains(und));

            // if there are undefined functions, mark as virtual
            //
            // A pure virtual destructor is counted here rather than in the
            // signature sets above, because it doesn't inherit the way other
            // pure virtual methods do. `~Base` and `~Derived` are different
            // signatures, so a base's pure destructor could never be cancelled
            // out by a derived class's `defined` set - yet every derived class
            // does override it, explicitly or implicitly, and so is concrete.
            // Only the class which declares `= 0` on its own destructor is
            // abstract because of it.
            if !self_cs.undefined.is_empty()
                || !self_cs.undefined_through_virtual_base.is_empty()
                || pure_virtual_destructors.contains(&name.name)
            {
                abstract_classes.insert(name.name.clone());
            }

            // store it back so child classes can read it properly
            *class_states.entry(name.name.clone()).or_default() = self_cs;
        }
    }

    // mark abstract types as abstract
    let mut apis: ApiVec<_> = apis
        .into_iter()
        .map(|mut api| {
            match &mut api {
                Api::Struct { name, analysis, .. } if abstract_classes.contains(&name.name) => {
                    analysis.pod.kind = TypeKind::Abstract;
                    // Nothing may `delete` one of these through a pointer to
                    // the class itself, so the container support which does
                    // has to be withheld. See
                    // [`PublicConstructors::abstract_without_virtual_destructor`].
                    analysis.constructors.abstract_without_virtual_destructor =
                        !virtual_destructors.contains(&name.name);
                }
                // The same set answers the subclass question, which is about
                // the superclass rather than about this type; see
                // [`SubclassAnalysis::superclass_destructor_virtual`].
                Api::Subclass {
                    superclass,
                    analysis,
                    ..
                } => {
                    analysis.superclass_destructor_virtual = virtual_destructors
                        .contains(&resolve_typedefs(&typedef_targets, superclass));
                }
                _ => {}
            }
            api
        })
        .collect();

    // We also need to remove any constructors belonging to these
    // abstract types.
    apis.retain(|api| {
        !matches!(&api,
            Api::Function {
                analysis:
                    FnAnalysis {
                        kind: FnKind::Method{impl_for: self_ty, method_kind: MethodKind::Constructor{..}, ..}
                            | FnKind::TraitMethod{ kind: TraitMethodKind::CopyConstructor | TraitMethodKind::MoveConstructor, impl_for: self_ty, ..},
                        ..
                    },
                    ..
            } if abstract_classes.contains(self_ty)
        )
    });

    // Finally, if there are any types which are nested inside other types,
    // they can't be abstract. This is due to two small limitations in cxx.
    // Imagine we have class Foo { class Bar }
    // 1) using "type Foo = super::bindgen::root::Foo_Bar" results
    //    in the creation of std::unique_ptr code which isn't acceptable
    //    for an abtract class
    // 2) using "type Foo;" isn't possible unless Foo is a top-level item
    //    within its namespace. Any outer names will be interpreted as namespace
    //    names and result in cxx generating "namespace Foo { class Bar }"".
    let mut results = ApiVec::new();
    convert_item_apis(apis, &mut results, |api| match api {
        Api::Struct {
            analysis:
                PodAndConstructorAnalysis {
                    pod:
                        PodAnalysis {
                            kind: TypeKind::Abstract,
                            ..
                        },
                    ..
                },
            ..
        } if api
            .cpp_name()
            .as_ref()
            .map(|n| n.is_nested())
            .unwrap_or_default() =>
        {
            Err(ConvertErrorFromCpp::AbstractNestedType)
        }
        _ => Ok(Box::new(std::iter::once(api))),
    });

    results
}

pub(crate) fn discard_ignored_functions(apis: ApiVec<FnPhase>) -> ApiVec<FnPhase> {
    // Some APIs can't be generated, e.g. because they're protected.
    // Now we've finished analyzing abstract types and constructors, we'll
    // convert them to IgnoredItems.
    let mut apis_new = ApiVec::new();
    convert_apis(
        apis,
        &mut apis_new,
        |name, fun, analysis| {
            analysis.ignore_reason.clone()?;
            Ok(Box::new(std::iter::once(Api::Function {
                name,
                fun,
                analysis,
            })))
        },
        Api::struct_unchanged,
        Api::enum_unchanged,
        Api::typedef_unchanged,
        Api::subclass_unchanged,
    );
    apis_new
}
