// Copyright 2026 The autocxx maintainers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Withdraws the ability to *own* a C++ type which C++ won't let anyone
//! destroy.
//!
//! A C++ class may deliberately have a `private`, `protected` or `= delete`d
//! destructor while still being constructible - that's how you say "instances
//! of me are never destroyed by outsiders". autocxx used to generate the full
//! ownership surface for such types anyway:
//!
//! * a constructor (`ffi::A::new()`, yielding a `moveit::New`);
//! * `CopyNew`/`MoveNew` impls for the implicit copy/move constructors;
//! * a `MakeCppStorage` impl, whose C++ side allocates with `operator new`
//!   and frees with a bare `operator delete`.
//!
//! Nothing in that path ever mentions `~A()`, so C++ never diagnoses it, and
//! `ffi::A::new().within_box()` compiled with no `unsafe` at all - then freed
//! the storage on drop without running the destructor. See
//! <https://github.com/google/autocxx/issues/829>.
//!
//! We therefore remove exactly the APIs which hand ownership to Rust, and
//! leave everything else - methods, static factories, casts - alone, so that
//! the common `flatbuffers::Table`-style pattern of borrowing a pointer
//! handed out by C++ keeps working.

use indexmap::set::IndexSet as HashSet;

use crate::{
    conversion::{
        analysis::fun::{
            FnKind, FnPrePhase2, MethodKind, PodAndConstructorAnalysis, TraitMethodKind,
        },
        api::Api,
        apivec::ApiVec,
        convert_error::{ConvertErrorWithContext, ErrorContext},
        ConvertErrorFromCpp,
    },
    types::{make_ident, QualifiedName},
};

/// Removes the APIs by which Rust could come to own a C++ object whose
/// destructor is inaccessible.
///
/// This runs after [`super::fun::FnAnalyzer::analyze_functions`], because
/// that's where `ItemsFound::destructor` - the C++ rules for which special
/// member functions exist and who may call them - is worked out. In
/// particular it can't be done back in [`super::allocators`], which
/// synthesizes the `MakeCppStorage` alloc/free pair before any of that is
/// known.
pub(crate) fn remove_ownership_of_non_destructible_types(
    apis: ApiVec<FnPrePhase2>,
) -> ApiVec<FnPrePhase2> {
    let non_destructible: HashSet<QualifiedName> = apis
        .iter()
        .filter_map(|api| match api {
            Api::Struct {
                name,
                analysis:
                    PodAndConstructorAnalysis {
                        constructors: fun_constructors,
                        ..
                    },
                ..
            } if fun_constructors.destructor_inaccessible => Some(name.name.clone()),
            _ => None,
        })
        .collect();
    if non_destructible.is_empty() {
        return apis;
    }
    log::info!("Types with inaccessible destructors: {non_destructible:?}");

    apis.into_iter()
        .map(|mut api| {
            if let Api::Function { analysis, .. } = &mut api {
                // Anything already rejected for some other reason keeps that
                // reason: it's more specific than ours, and clobbering it
                // would hide why the item really went away.
                if analysis.ignore_reason.is_err() {
                    return api;
                }
                match &analysis.kind {
                    // The constructors are the part of this the user actually
                    // writes (`ffi::A::new()`), so this is where we leave a
                    // diagnostic rather than silently dropping the API. The
                    // resulting `Api::IgnoredItem` becomes a documented stub
                    // in the `impl` block, naming the reason. Note we keep the
                    // type itself, complete with its methods, so an explicit
                    // `generate!` for it is still obeyed - a type you may only
                    // borrow is still useful.
                    FnKind::Method {
                        impl_for,
                        method_kind: MethodKind::Constructor { .. },
                        ..
                    } if non_destructible.contains(impl_for) => {
                        let ctx = ErrorContext::new_for_method(
                            impl_for.get_final_ident(),
                            make_ident(&analysis.rust_name),
                        );
                        analysis.ignore_reason = Err(ConvertErrorWithContext(
                            ConvertErrorFromCpp::DestructorInaccessible,
                            Some(ctx),
                        ));
                    }
                    // These, by contrast, are trait impls which we synthesized
                    // ourselves (`CopyNew`, `MoveNew`, `MakeCppStorage`); they
                    // have no name the user could have asked for, so a stub
                    // would be noise. An `ignore_reason` with no
                    // [`ErrorContext`] makes them disappear without one. The
                    // constructor stub above, and the note attached to the
                    // type itself in `codegen_rs`, carry the explanation.
                    FnKind::TraitMethod {
                        kind:
                            TraitMethodKind::CopyConstructor
                            | TraitMethodKind::MoveConstructor
                            | TraitMethodKind::Alloc
                            | TraitMethodKind::Dealloc,
                        impl_for,
                        ..
                    } if non_destructible.contains(impl_for) => {
                        analysis.ignore_reason = Err(ConvertErrorWithContext(
                            ConvertErrorFromCpp::DestructorInaccessible,
                            None,
                        ));
                    }
                    _ => {}
                }
            }
            api
        })
        .collect()
}
