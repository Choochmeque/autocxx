// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use autocxx_bindgen::callbacks::{Explicitness, SpecialMemberKind, Visibility as CppVisibility};
use indexmap::map::IndexMap as HashMap;
use indexmap::{map::Entry, set::IndexSet as HashSet};

use syn::{PatType, Type, TypeArray};

use crate::conversion::analysis::type_converter::TypeKind;
use crate::conversion::type_helpers::type_is_reference;
use crate::{
    conversion::{
        analysis::{depth_first::fields_and_bases_first, pod::PodAnalysis},
        api::{Api, ApiName, FuncToConvert},
        apivec::ApiVec,
        convert_error::{ConvertErrorWithContext, ErrorContext},
        ConvertErrorFromCpp,
    },
    known_types::{known_types, KnownTypeConstructorDetails},
    types::{make_ident, QualifiedName},
};

use super::{FnAnalysis, FnKind, FnPrePhase1, MethodKind, ReceiverMutability, TraitMethodKind};

/// Indicates what we found out about a category of special member function.
///
/// In the end, we only care whether it's public and exists, but we track a bit more information to
/// support determining the information for dependent classes.
#[derive(Debug, Copy, Clone)]
pub(super) enum SpecialMemberFound {
    /// This covers being deleted in any way:
    ///   * Explicitly deleted
    ///   * Implicitly defaulted when that means being deleted
    ///   * Explicitly defaulted when that means being deleted
    ///
    /// It also covers not being either user declared or implicitly defaulted.
    NotPresent,
    /// Implicit special member functions, indicated by this, are always public.
    Implicit,
    /// This covers being explicitly defaulted (when that is not deleted) or being user-defined.
    Explicit(CppVisibility),
}

impl SpecialMemberFound {
    /// Returns whether code outside of subclasses can call this special member function.
    pub fn callable_any(&self) -> bool {
        matches!(self, Self::Explicit(CppVisibility::Public) | Self::Implicit)
    }

    /// Returns whether code in a subclass can call this special member function.
    pub fn callable_subclass(&self) -> bool {
        matches!(
            self,
            Self::Explicit(CppVisibility::Public)
                | Self::Explicit(CppVisibility::Protected)
                | Self::Implicit
        )
    }

    /// Returns whether this exists at all. Note that this will return true even if it's private,
    /// which is generally not very useful, but does come into play for some rules around which
    /// default special member functions are deleted vs don't exist.
    pub fn exists(&self) -> bool {
        matches!(self, Self::Explicit(_) | Self::Implicit)
    }

    pub fn exists_implicit(&self) -> bool {
        matches!(self, Self::Implicit)
    }

    pub fn exists_explicit(&self) -> bool {
        matches!(self, Self::Explicit(_))
    }
}

/// Information about which special member functions exist based on the C++ rules.
///
/// Not all of this information is used directly, but we need to track it to determine the
/// information we do need for classes which are used as members or base classes.
#[derive(Debug, Clone)]
pub(super) struct ItemsFound {
    pub(super) default_constructor: SpecialMemberFound,
    pub(super) destructor: SpecialMemberFound,
    pub(super) const_copy_constructor: SpecialMemberFound,
    /// Remember that [`const_copy_constructor`] may be used in place of this if it exists.
    pub(super) non_const_copy_constructor: SpecialMemberFound,
    pub(super) move_constructor: SpecialMemberFound,

    /// The full name of the type. We identify instances by [`QualifiedName`], because that's
    /// the only thing which [`FnKind::Method`] has to tie it to, and that's unique enough for
    /// identification.  However, when generating functions for implicit special members, we need
    /// the extra information here.
    ///
    /// Will always be `Some` if any of the other fields are [`SpecialMemberFound::Implict`],
    /// otherwise optional.
    pub(super) name: Option<ApiName>,
}

impl ItemsFound {
    /// Returns whether we should generate a default constructor wrapper, because bindgen won't do
    /// one for the implicit default constructor which exists.
    pub(super) fn implicit_default_constructor_needed(&self) -> bool {
        self.default_constructor.exists_implicit()
    }

    /// Returns whether we should generate a copy constructor wrapper, because bindgen won't do one
    /// for the implicit copy constructor which exists.
    pub(super) fn implicit_copy_constructor_needed(&self) -> bool {
        let any_implicit_copy = self.const_copy_constructor.exists_implicit()
            || self.non_const_copy_constructor.exists_implicit();
        let no_explicit_copy = !(self.const_copy_constructor.exists_explicit()
            || self.non_const_copy_constructor.exists_explicit());
        any_implicit_copy && no_explicit_copy
    }

    /// Returns whether we should generate a move constructor wrapper, because bindgen won't do one
    /// for the implicit move constructor which exists.
    pub(super) fn implicit_move_constructor_needed(&self) -> bool {
        self.move_constructor.exists_implicit()
    }

    /// Returns whether we should generate a destructor wrapper, because bindgen won't do one for
    /// the implicit destructor which exists.
    pub(super) fn implicit_destructor_needed(&self) -> bool {
        self.destructor.exists_implicit()
    }
}
#[derive(Hash, Eq, PartialEq)]
enum ExplicitKind {
    DefaultConstructor,
    ConstCopyConstructor,
    NonConstCopyConstructor,
    MoveConstructor,
    OtherConstructor,
    Destructor,
    ConstCopyAssignmentOperator,
    NonConstCopyAssignmentOperator,
    MoveAssignmentOperator,
}

/// Denotes a specific kind of explicit member function that we found.
#[derive(Hash, Eq, PartialEq)]
struct ExplicitType {
    ty: QualifiedName,
    kind: ExplicitKind,
}

/// Includes information about an explicit special member function which was found.
#[derive(Copy, Clone, Debug)]
enum ExplicitFound {
    UserDefined(CppVisibility),
    /// Explicitly defaulted, i.e. `= default`. The user asked for exactly what
    /// C++ would have written implicitly, so whether the member actually
    /// exists is decided by the same rules - it may well be deleted, which is
    /// what made this worth telling apart from `UserDefined`. See
    /// <https://github.com/google/autocxx/issues/815>.
    Defaulted(CppVisibility),
    /// Note that this always means explicitly deleted, because this enum only represents
    /// explicit declarations.
    Deleted,
    /// Indicates that we found more than one explicit of this kind. This is possible with most of
    /// them, and we just bail and mostly act as if they're deleted. We'd have to decide whether
    /// they're ambiguous to use them, which is really complicated.
    Multiple,
}

/// Whether this was declared `= default`.
fn is_defaulted(explicit: Option<&ExplicitFound>) -> bool {
    matches!(explicit, Some(ExplicitFound::Defaulted(_)))
}

/// What to report once the implicit rules have said the member survives.
/// A `= default`ed one is a real declaration which bindgen has already given
/// us a function for, and which may not be public; an absent one is implicit
/// and always public.
fn found_as_declared(explicit: Option<&ExplicitFound>) -> SpecialMemberFound {
    match explicit {
        Some(ExplicitFound::Defaulted(visibility)) => SpecialMemberFound::Explicit(*visibility),
        _ => SpecialMemberFound::Implicit,
    }
}

/// What to report for one of a pair of members - the const and non-const copy
/// constructors - where the user wrote `= default` on at least one of them,
/// and so said which of the two the class has.
fn found_only_if_defaulted(explicit: Option<&ExplicitFound>) -> SpecialMemberFound {
    match explicit {
        Some(ExplicitFound::Defaulted(visibility)) => SpecialMemberFound::Explicit(*visibility),
        _ => SpecialMemberFound::NotPresent,
    }
}

/// What to report when the implicit rules don't apply to this member at all,
/// so only a user-written definition would give it to us.
fn found_if_user_defined(explicit: Option<&ExplicitFound>) -> SpecialMemberFound {
    match explicit {
        Some(ExplicitFound::UserDefined(visibility)) => SpecialMemberFound::Explicit(*visibility),
        _ => SpecialMemberFound::NotPresent,
    }
}

/// Analyzes which constructors are present for each type.
///
/// If a type has explicit constructors, bindgen will generate corresponding
/// constructor functions, which we'll have already converted to make_unique methods.
/// For types with implicit constructors, we enumerate them here.
///
/// It is tempting to make this a separate analysis phase, to be run later than
/// the function analysis; but that would make the code much more complex as it
/// would need to output a `FnAnalysisBody`. By running it as part of this phase
/// we can simply generate the sort of thing bindgen generates, then ask
/// the existing code in this phase to figure out what to do with it.
pub(super) fn find_constructors_present(
    apis: &ApiVec<FnPrePhase1>,
) -> HashMap<QualifiedName, ItemsFound> {
    let (explicits, unknown_types) = find_explicit_items(apis);
    let enums: HashSet<QualifiedName> = apis
        .iter()
        .filter_map(|api| match api {
            Api::Enum { name, .. } => Some(name.name.clone()),
            _ => None,
        })
        .collect();

    // These contain all the classes we've seen so far with the relevant properties on their
    // constructors of each kind. We iterate via [`depth_first`], so analyzing later classes
    // just needs to check these.
    //
    // Important only to ask for a depth-first analysis of structs, because
    // when all APIs are considered there may be reference loops and that would
    // panic.
    //
    // These analyses include all bases and members of each class.
    let mut all_items_found: HashMap<QualifiedName, ItemsFound> = HashMap::new();

    for api in fields_and_bases_first(apis.iter()) {
        if let Api::Struct {
            name,
            analysis:
                PodAnalysis {
                    // Do not include TypeKind::Opaque here
                    kind:
                        crate::conversion::api::TypeKind::Abstract
                        | crate::conversion::api::TypeKind::Pod
                        | crate::conversion::api::TypeKind::NonPod,
                    bases,
                    field_info,
                    num_generics: 0usize,
                    in_anonymous_namespace: false,
                    ..
                },
            details,
            ..
        } = api
        {
            let find_explicit = |kind: ExplicitKind| -> Option<&ExplicitFound> {
                explicits.get(&ExplicitType {
                    ty: name.name.clone(),
                    kind,
                })
            };
            let get_items_found = |qn: &QualifiedName| -> Option<ItemsFound> {
                if enums.contains(qn) {
                    Some(ItemsFound {
                        default_constructor: SpecialMemberFound::NotPresent,
                        destructor: SpecialMemberFound::Implicit,
                        const_copy_constructor: SpecialMemberFound::Implicit,
                        non_const_copy_constructor: SpecialMemberFound::NotPresent,
                        move_constructor: SpecialMemberFound::Implicit,
                        name: Some(name.clone()),
                    })
                } else if let Some(constructor_details) = known_types().get_constructor_details(qn)
                {
                    Some(known_type_items_found(constructor_details))
                } else {
                    all_items_found.get(qn).cloned()
                }
            };
            let bases_items_found: Vec<_> = bases.iter().map_while(get_items_found).collect();
            let fields_items_found: Vec<_> = field_info
                .iter()
                .filter_map(|field_info| match field_info.type_kind {
                    TypeKind::Regular | TypeKind::SubclassHolder(_) => match field_info.ty {
                        Type::Path(ref qn) => get_items_found(&QualifiedName::from_type_path(qn)),
                        Type::Array(TypeArray { ref elem, .. }) => match elem.as_ref() {
                            Type::Path(ref qn) => {
                                get_items_found(&QualifiedName::from_type_path(qn))
                            }
                            _ => None,
                        },
                        _ => None,
                    },
                    // A pointer field does not delete the implicit default constructor: it is
                    // simply left uninitialized. bindgen tells pointers and references apart for
                    // us by wrapping the latter in its `__bindgen_marker_Reference` /
                    // `__bindgen_marker_RValueReference` markers, which the type converter turns
                    // into the reference `TypeKind`s below, so a `TypeKind::Pointer` here really
                    // is a C++ pointer. Treating it like a reference silently suppressed `new()`
                    // for any struct with a pointer member.
                    // See https://github.com/google/autocxx/issues/1366.
                    TypeKind::Pointer => Some(ItemsFound {
                        default_constructor: SpecialMemberFound::Implicit,
                        destructor: SpecialMemberFound::Implicit,
                        const_copy_constructor: SpecialMemberFound::Implicit,
                        non_const_copy_constructor: SpecialMemberFound::NotPresent,
                        move_constructor: SpecialMemberFound::Implicit,
                        name: Some(name.clone()),
                    }),
                    // A reference field, by contrast, does delete the implicit default
                    // constructor unless it has a default member initializer.
                    TypeKind::Reference
                    | TypeKind::MutableReference
                    | TypeKind::RValueReference => Some(ItemsFound {
                        default_constructor: SpecialMemberFound::NotPresent,
                        destructor: SpecialMemberFound::Implicit,
                        const_copy_constructor: SpecialMemberFound::Implicit,
                        non_const_copy_constructor: SpecialMemberFound::NotPresent,
                        move_constructor: SpecialMemberFound::Implicit,
                        name: Some(name.clone()),
                    }),
                })
                .collect();
            let has_rvalue_reference_fields = details.has_rvalue_reference_fields;

            // Check that all the bases and field types are known first. This combined with
            // iterating via [`depth_first`] means we can safely search in `items_found` for all of
            // them.
            //
            // Conservatively, we will not acknowledge the existence of most defaulted or implicit
            // special member functions for any struct/class where we don't fully understand all
            // field types.  However, we can still look for explictly declared versions and use
            // those. See below for destructors.
            //
            // We need to extend our knowledge to understand the constructor behavior of things in
            // known_types.rs, then we'll be able to cope with types which contain strings,
            // unique_ptrs etc.
            let items_found = if bases_items_found.len() != bases.len()
                || fields_items_found.len() != field_info.len()
                || unknown_types.contains(&name.name)
            {
                let is_explicit = |kind: ExplicitKind| -> SpecialMemberFound {
                    match find_explicit(kind) {
                        None => SpecialMemberFound::NotPresent,
                        // We don't understand this class's bases and members,
                        // so we can't run the rules which decide whether a
                        // `= default`ed member is deleted. Assume it is.
                        Some(
                            ExplicitFound::Deleted
                            | ExplicitFound::Multiple
                            | ExplicitFound::Defaulted(_),
                        ) => SpecialMemberFound::NotPresent,
                        Some(ExplicitFound::UserDefined(visibility)) => {
                            SpecialMemberFound::Explicit(*visibility)
                        }
                    }
                };
                let items_found = ItemsFound {
                    default_constructor: is_explicit(ExplicitKind::DefaultConstructor),
                    // The destructor is the one member we're optimistic about
                    // for a class like this. Assuming unknown types have one is
                    // common and lets us generate UniquePtr wrappers for them;
                    // assuming they don't would withdraw ownership from a great
                    // many types we handle perfectly well today.
                    //
                    // The cost is the same as it has always been: if the unknown
                    // type turns out not to have an accessible destructor, the
                    // C++ we generate won't compile. Maybe we should have a way
                    // to disable that?
                    //
                    // A `= default`ed destructor rides along with that
                    // optimism, and is the one place a `= default` isn't put
                    // through the C++ rules the way google/autocxx#815 asks -
                    // we can't run them without knowing the bases and members
                    // whose destructors decide the answer. Writing
                    // `~T() = default;` says exactly what writing nothing says,
                    // so it would make no sense to treat the two differently
                    // here, and this arm follows the `None` one above. Deciding
                    // it properly needs the same thing the surrounding
                    // conservatism needs: understanding these field types in the
                    // first place. Remaining #815 scope.
                    destructor: match find_explicit(ExplicitKind::Destructor) {
                        None => SpecialMemberFound::Implicit,
                        // If there are multiple destructors, assume that one of them will be
                        // selected by overload resolution.
                        Some(ExplicitFound::Multiple) => {
                            SpecialMemberFound::Explicit(CppVisibility::Public)
                        }
                        Some(ExplicitFound::Deleted) => SpecialMemberFound::NotPresent,
                        // A declared destructor, defaulted or not, at least
                        // tells us how visible it is.
                        Some(
                            ExplicitFound::UserDefined(visibility)
                            | ExplicitFound::Defaulted(visibility),
                        ) => SpecialMemberFound::Explicit(*visibility),
                    },
                    const_copy_constructor: is_explicit(ExplicitKind::ConstCopyConstructor),
                    non_const_copy_constructor: is_explicit(ExplicitKind::NonConstCopyConstructor),
                    move_constructor: is_explicit(ExplicitKind::MoveConstructor),
                    name: Some(name.clone()),
                };
                log::info!(
                    "Special member functions (explicits only) found for {:?}: {:?}",
                    name,
                    items_found
                );
                items_found
            } else {
                // If no user-declared constructors of any kind are provided for a class type (struct, class, or union),
                // the compiler will always declare a default constructor as an inline public member of its class.
                //
                // The implicitly-declared or defaulted default constructor for class T is defined as deleted if any of the following is true:
                // T has a member of reference type without a default initializer.
                // T has a non-const-default-constructible const member without a default member initializer.
                // T has a member (without a default member initializer) which has a deleted default constructor, or its default constructor is ambiguous or inaccessible from this constructor.
                // T has a direct or virtual base which has a deleted default constructor, or it is ambiguous or inaccessible from this constructor.
                // T has a direct or virtual base or a non-static data member which has a deleted destructor, or a destructor that is inaccessible from this constructor.
                // T is a union with at least one variant member with non-trivial default constructor, and no variant member of T has a default member initializer. // we don't support unions anyway
                // T is a non-union class with a variant member M with a non-trivial default constructor, and no variant member of the anonymous union containing M has a default member initializer.
                // T is a union and all of its variant members are const. // we don't support unions anyway
                //
                // Variant members are the members of anonymous unions.
                let default_constructor = {
                    let explicit = find_explicit(ExplicitKind::DefaultConstructor);
                    // `T() = default;` declares the default constructor no
                    // matter what other constructors the class has. Only the
                    // implicit one waits for there to be none at all.
                    let have_defaulted = is_defaulted(explicit)
                        || (explicit.is_none()
                            && !explicits.iter().any(|(ExplicitType { ty, kind }, _)| {
                                ty == &name.name
                                    && match *kind {
                                        ExplicitKind::DefaultConstructor => false,
                                        ExplicitKind::ConstCopyConstructor => true,
                                        ExplicitKind::NonConstCopyConstructor => true,
                                        ExplicitKind::MoveConstructor => true,
                                        ExplicitKind::OtherConstructor => true,
                                        ExplicitKind::Destructor => false,
                                        ExplicitKind::ConstCopyAssignmentOperator => false,
                                        ExplicitKind::NonConstCopyAssignmentOperator => false,
                                        ExplicitKind::MoveAssignmentOperator => false,
                                    }
                            }));
                    if have_defaulted {
                        let bases_allow = bases_items_found.iter().all(|items_found| {
                            items_found.destructor.callable_subclass()
                                && items_found.default_constructor.callable_subclass()
                        });
                        // TODO: Allow member initializers for
                        // https://github.com/google/autocxx/issues/816.
                        let members_allow = fields_items_found.iter().all(|items_found| {
                            items_found.destructor.callable_any()
                                && items_found.default_constructor.callable_any()
                        });
                        if !has_rvalue_reference_fields && bases_allow && members_allow {
                            found_as_declared(explicit)
                        } else {
                            SpecialMemberFound::NotPresent
                        }
                    } else {
                        found_if_user_defined(explicit)
                    }
                };

                // If no user-declared prospective destructor is provided for a class type (struct, class, or union), the compiler will always declare a destructor as an inline public member of its class.
                //
                // The implicitly-declared or explicitly defaulted destructor for class T is defined as deleted if any of the following is true:
                // T has a non-static data member that cannot be destructed (has deleted or inaccessible destructor)
                // T has direct or virtual base class that cannot be destructed (has deleted or inaccessible destructors)
                // T is a union and has a variant member with non-trivial destructor. // we don't support unions anyway
                // The implicitly-declared destructor is virtual (because the base class has a virtual destructor) and the lookup for the deallocation function (operator delete()) results in a call to ambiguous, deleted, or inaccessible function.
                let destructor = {
                    let explicit = find_explicit(ExplicitKind::Destructor);
                    if explicit.is_none() || is_defaulted(explicit) {
                        let bases_allow = bases_items_found
                            .iter()
                            .all(|items_found| items_found.destructor.callable_subclass());
                        let members_allow = fields_items_found
                            .iter()
                            .all(|items_found| items_found.destructor.callable_any());
                        if bases_allow && members_allow {
                            found_as_declared(explicit)
                        } else {
                            SpecialMemberFound::NotPresent
                        }
                    } else {
                        found_if_user_defined(explicit)
                    }
                };

                // If no user-defined copy constructors are provided for a class type (struct, class, or union),
                // the compiler will always declare a copy constructor as a non-explicit inline public member of its class.
                // This implicitly-declared copy constructor has the form T::T(const T&) if all of the following are true:
                //  each direct and virtual base B of T has a copy constructor whose parameters are const B& or const volatile B&;
                //  each non-static data member M of T of class type or array of class type has a copy constructor whose parameters are const M& or const volatile M&.
                //
                // The implicitly-declared or defaulted copy constructor for class T is defined as deleted if any of the following conditions are true:
                // T is a union-like class and has a variant member with non-trivial copy constructor; // we don't support unions anyway
                // T has a user-defined move constructor or move assignment operator (this condition only causes the implicitly-declared, not the defaulted, copy constructor to be deleted).
                // T has non-static data members that cannot be copied (have deleted, inaccessible, or ambiguous copy constructors);
                // T has direct or virtual base class that cannot be copied (has deleted, inaccessible, or ambiguous copy constructors);
                // T has direct or virtual base class or a non-static data member with a deleted or inaccessible destructor;
                // T has a data member of rvalue reference type;
                let (const_copy_constructor, non_const_copy_constructor) = {
                    let explicit_const = find_explicit(ExplicitKind::ConstCopyConstructor);
                    let explicit_non_const = find_explicit(ExplicitKind::NonConstCopyConstructor);
                    let explicit_move = find_explicit(ExplicitKind::MoveConstructor);

                    let copy_is_defaulted =
                        is_defaulted(explicit_const) || is_defaulted(explicit_non_const);
                    let have_defaulted = (explicit_const.is_none() || is_defaulted(explicit_const))
                        && (explicit_non_const.is_none() || is_defaulted(explicit_non_const));
                    if have_defaulted {
                        // A user-declared move constructor deletes the
                        // *implicitly declared* copy constructor, but not one
                        // the user asked for with `= default`.
                        let class_allows = (copy_is_defaulted || explicit_move.is_none())
                            && !has_rvalue_reference_fields;
                        let bases_allow = bases_items_found.iter().all(|items_found| {
                            items_found.destructor.callable_subclass()
                                && (items_found.const_copy_constructor.callable_subclass()
                                    || items_found.non_const_copy_constructor.callable_subclass())
                        });
                        let members_allow = fields_items_found.iter().all(|items_found| {
                            items_found.destructor.callable_any()
                                && (items_found.const_copy_constructor.callable_any()
                                    || items_found.non_const_copy_constructor.callable_any())
                        });
                        if class_allows && bases_allow && members_allow {
                            if copy_is_defaulted {
                                // The user wrote the signature out, so it
                                // decides which of the two the class has, and
                                // how visible each is - not the bases and
                                // members, which only get to choose for the
                                // implicitly declared one below.
                                (
                                    found_only_if_defaulted(explicit_const),
                                    found_only_if_defaulted(explicit_non_const),
                                )
                            } else {
                                let dependencies_are_const = bases_items_found
                                    .iter()
                                    .chain(fields_items_found.iter())
                                    .all(|items_found| items_found.const_copy_constructor.exists());
                                if dependencies_are_const {
                                    (SpecialMemberFound::Implicit, SpecialMemberFound::NotPresent)
                                } else {
                                    (SpecialMemberFound::NotPresent, SpecialMemberFound::Implicit)
                                }
                            }
                        } else {
                            (
                                SpecialMemberFound::NotPresent,
                                SpecialMemberFound::NotPresent,
                            )
                        }
                    } else {
                        (
                            found_if_user_defined(explicit_const),
                            found_if_user_defined(explicit_non_const),
                        )
                    }
                };

                // If no user-defined move constructors are provided for a class type (struct, class, or union), and all of the following is true:
                // there are no user-declared copy constructors;
                // there are no user-declared copy assignment operators;
                // there are no user-declared move assignment operators;
                // there is no user-declared destructor.
                // then the compiler will declare a move constructor as a non-explicit inline public member of its class with the signature T::T(T&&).
                //
                // A class can have multiple move constructors, e.g. both T::T(const T&&) and T::T(T&&). If some user-defined move constructors are present, the user may still force the generation of the implicitly declared move constructor with the keyword default.
                //
                // The implicitly-declared or defaulted move constructor for class T is defined as deleted if any of the following is true:
                // T has non-static data members that cannot be moved (have deleted, inaccessible, or ambiguous move constructors);
                // T has direct or virtual base class that cannot be moved (has deleted, inaccessible, or ambiguous move constructors);
                // T has direct or virtual base class with a deleted or inaccessible destructor;
                // T is a union-like class and has a variant member with non-trivial move constructor. // we don't support unions anyway
                let move_constructor = {
                    let explicit = find_explicit(ExplicitKind::MoveConstructor);
                    // As with the default constructor, `T(T&&) = default;`
                    // declares it whatever else the class declares; the
                    // implicit one appears only if nothing else does.
                    let have_defaulted = is_defaulted(explicit)
                        || !(explicit.is_some()
                            || find_explicit(ExplicitKind::ConstCopyConstructor).is_some()
                            || find_explicit(ExplicitKind::NonConstCopyConstructor).is_some()
                            || find_explicit(ExplicitKind::ConstCopyAssignmentOperator).is_some()
                            || find_explicit(ExplicitKind::NonConstCopyAssignmentOperator)
                                .is_some()
                            || find_explicit(ExplicitKind::MoveAssignmentOperator).is_some()
                            || find_explicit(ExplicitKind::Destructor).is_some());
                    if have_defaulted {
                        let bases_allow = bases_items_found.iter().all(|items_found| {
                            items_found.destructor.callable_subclass()
                                && items_found.move_constructor.callable_subclass()
                        });
                        let members_allow = fields_items_found
                            .iter()
                            .all(|items_found| items_found.move_constructor.callable_any());
                        if bases_allow && members_allow {
                            found_as_declared(explicit)
                        } else {
                            SpecialMemberFound::NotPresent
                        }
                    } else {
                        found_if_user_defined(explicit)
                    }
                };

                let items_found = ItemsFound {
                    default_constructor,
                    destructor,
                    const_copy_constructor,
                    non_const_copy_constructor,
                    move_constructor,
                    name: Some(name.clone()),
                };
                log::info!(
                    "Special member items found for {:?}: {:?}",
                    name,
                    items_found
                );
                items_found
            };
            assert!(
                all_items_found
                    .insert(name.name.clone(), items_found)
                    .is_none(),
                "Duplicate struct: {name:?}"
            );
        }
    }

    all_items_found
}

/// Withdraws the special member functions which C++ declares because someone
/// wrote `= default`, but then defines as deleted - a copy constructor on a
/// class with a non-copyable member, say, or a default constructor on one with
/// a `const` member and no initializer for it.
///
/// bindgen reports these to us as declarations like any other, so without this
/// we generate a call to one and C++ refuses it: "call to implicitly-deleted
/// copy constructor". See <https://github.com/google/autocxx/issues/815>.
///
/// [`find_constructors_present`] has already applied the same C++ rules to
/// work out which of these actually exist, so this only has to act on what it
/// concluded.
pub(super) fn discard_deleted_defaulted_members(
    apis: ApiVec<FnPrePhase1>,
    all_items_found: &HashMap<QualifiedName, ItemsFound>,
) -> ApiVec<FnPrePhase1> {
    apis.into_iter()
        .map(|mut api| {
            if let Api::Function { fun, analysis, .. } = &mut api {
                if !matches!(fun.is_deleted, Some(Explicitness::Defaulted))
                    || analysis.ignore_reason.is_err()
                {
                    return api;
                }
                // These are the same kinds `find_explicit_items` recognizes.
                // A non-const copy constructor never reaches us as one, so
                // there's nothing here for that slot.
                let (found, ctx) = match &analysis.kind {
                    FnKind::Method {
                        impl_for,
                        method_kind: MethodKind::Constructor { is_default: true },
                        ..
                    } => (
                        all_items_found.get(impl_for).map(|i| i.default_constructor),
                        // The user writes `ffi::A::new()`, so leave them a
                        // stub saying where it went.
                        Some(ErrorContext::new_for_method(
                            impl_for.get_final_ident(),
                            make_ident(&analysis.rust_name),
                        )),
                    ),
                    FnKind::TraitMethod { impl_for, kind, .. } => (
                        all_items_found.get(impl_for).and_then(|i| match kind {
                            TraitMethodKind::Destructor => Some(i.destructor),
                            TraitMethodKind::CopyConstructor => Some(i.const_copy_constructor),
                            TraitMethodKind::MoveConstructor => Some(i.move_constructor),
                            _ => None,
                        }),
                        // Whereas these are trait impls with no name anyone
                        // could have asked for, so a stub would be noise.
                        None,
                    ),
                    _ => (None, None),
                };
                if matches!(found, Some(SpecialMemberFound::NotPresent)) {
                    analysis.ignore_reason = Err(ConvertErrorWithContext(
                        ConvertErrorFromCpp::DefaultedButDeleted,
                        ctx,
                    ));
                }
            }
            api
        })
        .collect()
}

fn find_explicit_items(
    apis: &ApiVec<FnPrePhase1>,
) -> (HashMap<ExplicitType, ExplicitFound>, HashSet<QualifiedName>) {
    let mut result = HashMap::new();
    let mut merge_fun = |ty: QualifiedName, kind: ExplicitKind, fun: &FuncToConvert| match result
        .entry(ExplicitType { ty, kind })
    {
        Entry::Vacant(entry) => {
            entry.insert(match fun.is_deleted {
                Some(Explicitness::Deleted) => ExplicitFound::Deleted,
                Some(Explicitness::Defaulted) => ExplicitFound::Defaulted(fun.cpp_vis),
                None => ExplicitFound::UserDefined(fun.cpp_vis),
            });
        }
        Entry::Occupied(mut entry) => {
            entry.insert(ExplicitFound::Multiple);
        }
    };
    let mut unknown_types = HashSet::new();
    for api in apis.iter() {
        match api {
            Api::Function {
                analysis:
                    FnAnalysis {
                        kind: FnKind::Method { impl_for, .. },
                        param_details,
                        ignore_reason:
                            Ok(())
                            | Err(ConvertErrorWithContext(ConvertErrorFromCpp::AssignmentOperator, _)),
                        ..
                    },
                fun,
                ..
            } if matches!(
                fun.special_member,
                Some(SpecialMemberKind::AssignmentOperator)
            ) =>
            {
                let is_move_assignment_operator = !any_input_is_rvalue_reference(&fun.inputs);
                merge_fun(
                    impl_for.clone(),
                    if is_move_assignment_operator {
                        ExplicitKind::MoveAssignmentOperator
                    } else {
                        let receiver_mutability = &param_details
                            .iter()
                            .next()
                            .unwrap()
                            .self_type
                            .as_ref()
                            .unwrap()
                            .1;
                        match receiver_mutability {
                            ReceiverMutability::Const => ExplicitKind::ConstCopyAssignmentOperator,
                            ReceiverMutability::Mutable => {
                                ExplicitKind::NonConstCopyAssignmentOperator
                            }
                        }
                    },
                    fun,
                )
            }
            Api::Function {
                analysis:
                    FnAnalysis {
                        kind: FnKind::Method { impl_for, .. },
                        ..
                    },
                fun,
                ..
            } if matches!(
                fun.special_member,
                Some(SpecialMemberKind::AssignmentOperator)
            ) =>
            {
                unknown_types.insert(impl_for.clone());
            }
            Api::Function {
                analysis:
                    FnAnalysis {
                        kind:
                            FnKind::Method {
                                impl_for,
                                method_kind,
                                ..
                            },
                        ..
                    },
                fun,
                ..
            } => match method_kind {
                MethodKind::Constructor { is_default: true } => {
                    Some(ExplicitKind::DefaultConstructor)
                }
                MethodKind::Constructor { is_default: false } => {
                    Some(ExplicitKind::OtherConstructor)
                }
                _ => None,
            }
            .map_or((), |explicit_kind| {
                merge_fun(impl_for.clone(), explicit_kind, fun)
            }),
            Api::Function {
                analysis:
                    FnAnalysis {
                        kind: FnKind::TraitMethod { impl_for, kind, .. },
                        ..
                    },
                fun,
                ..
            } => match kind {
                TraitMethodKind::Destructor => Some(ExplicitKind::Destructor),
                // In `analyze_foreign_fn` we mark non-const copy constructors as not being copy
                // constructors for now, so we don't have to worry about them.
                //
                // TODO: which means `ExplicitKind::NonConstCopyConstructor` is
                // never recorded here, and a class whose only copy constructor
                // is `T(T&)` looks to the rules below like a class which
                // declares no copy constructor at all. They then hand it an
                // implicit `T(const T&)` and an implicit `T(T&&)`, neither of
                // which C++ declares for such a class, and we synthesize
                // wrappers calling both - so the generated C++ doesn't
                // compile. `T(T&)` reaches here as an
                // `ExplicitKind::OtherConstructor`, which is what would have
                // to be told apart to fix it.
                TraitMethodKind::CopyConstructor => Some(ExplicitKind::ConstCopyConstructor),
                TraitMethodKind::MoveConstructor => Some(ExplicitKind::MoveConstructor),
                _ => None,
            }
            .map_or((), |explicit_kind| {
                merge_fun(impl_for.clone(), explicit_kind, fun)
            }),
            _ => (),
        }
    }
    (result, unknown_types)
}

fn any_input_is_rvalue_reference(
    inputs: &syn::punctuated::Punctuated<crate::minisyn::FnArg, syn::token::Comma>,
) -> bool {
    inputs.iter().any(|input| match &input.0 {
        syn::FnArg::Receiver(_) => false,
        syn::FnArg::Typed(PatType { ty, .. }, ..) => type_is_reference(ty.as_ref(), true),
    })
}

/// Returns the information for a given known type.
fn known_type_items_found(constructor_details: KnownTypeConstructorDetails) -> ItemsFound {
    let exists_public = SpecialMemberFound::Explicit(CppVisibility::Public);
    let exists_public_if = |exists| {
        if exists {
            exists_public
        } else {
            SpecialMemberFound::NotPresent
        }
    };
    ItemsFound {
        default_constructor: exists_public,
        destructor: exists_public,
        const_copy_constructor: exists_public_if(constructor_details.has_const_copy_constructor),
        non_const_copy_constructor: SpecialMemberFound::NotPresent,
        move_constructor: exists_public_if(constructor_details.has_move_constructor),
        name: None,
    }
}
