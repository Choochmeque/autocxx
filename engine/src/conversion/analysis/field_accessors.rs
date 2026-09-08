// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::vendored_bindgen::callbacks::MethodKind as CppMethodKind;
use autocxx_parser::IncludeCppConfig;
use indexmap::set::IndexSet as HashSet;
use syn::{parse_quote, Field, Type};

use crate::{
    conversion::{
        api::{
            Api, ApiName, CppVisibility, FuncToConvert, NestedCppNames, Provenance, StructDetails,
            TypeKind,
        },
        apivec::ApiVec,
        convert_error::{ErrorContext, UnrepresentableMember},
        parse::CppRefQualifier,
        type_helpers::{is_pointer_like, is_volatile_qualified, strip_const_markers},
        ConvertErrorFromCpp, CppOriginalName,
    },
    known_types::known_types,
    minisyn::{Attribute, FnArg, ReturnType},
    parse_callbacks::DataMember,
    types::{make_ident, QualifiedName},
    ParseCallbackResults,
};

use super::{
    fun::function_wrapper::{CppFunctionBody, CppFunctionKind},
    pod::{pod_safe_types, FieldInfo, PodAnalysis, PodPhase},
    type_converter,
};

/// What we append to the name bindgen gave a field to name the API which
/// carries its accessor.
///
/// The accessor cannot be filed under the name bindgen would have given such a
/// method - `Bob_b` is already taken whenever C++ declares both a field `b` and
/// a method `b()`, and two APIs of one name destroy each other. Only this API
/// name carries the suffix; the method the user calls is named from the field,
/// and the overload machinery settles it if the class has a method of that name
/// too.
const ACCESSOR_SUFFIX: &str = "_autocxx_field";

/// Give every public data member of a generated non-POD type a getter, so that
/// Rust can read a field of a type it is not allowed to see the layout of.
///
/// A non-POD type reaches Rust as an opaque struct with private contents
/// (`codegen_rs::non_pod_struct`), deliberately: Rust must not be told offsets
/// C++ owns. That left no way at all to read a member, and the documented
/// workaround was to write the getter in C++ by hand - which is exactly what
/// this synthesizes, one per member, through the same wrapper machinery any
/// other method goes through. See google/autocxx#53.
///
/// POD types get nothing here: their fields are already Rust fields.
///
/// Two kinds of member are passed over rather than refused, because there is
/// nothing to hang a refusal on:
///
/// * a bitfield, which has no field of its own in the struct bindgen emitted
///   and no reported type either, so nothing says what a getter would return.
///   `denote_data_member` reports facts about it - its width, whether it is
///   `const` or `volatile` - but not a type, and the volatile case is answered
///   by refusing the class POD status rather than by a refusal here.
/// * a member of an anonymous union or struct, which bindgen neither names nor
///   generates a field for; the member is reported with no name at all.
///
/// A class template gets nothing either, refusals included: its methods are
/// turned down as a class (`MethodOfGenericType`), and a refusal filed against
/// one would put a documentation stub in an `impl` block for a type which
/// needs its arguments written out.
///
/// Two things C++ can say about a member are invisible here, because bindgen
/// reports neither and Rust has no way to write either.
///
/// A `mutable` member is one C++ may write through a `const` receiver, so a
/// borrowed accessor for one hands Rust a shared reference to something which
/// changes underneath it. That is the standing hazard of every reference
/// autocxx returns from C++, stated in the book, rather than a new one; it is
/// the reason a borrowed accessor is not to be assumed safer than a by-value
/// one.
///
/// A `volatile` member gets a by-value getter like any other, and that getter
/// is honest: its body is `obj.member`, and reading a `volatile` glvalue is a
/// volatile access in C++, so the read happens once per call in the generated
/// C++ and Rust receives the copy. Only the borrowed shape is refused
/// (`UnrepresentableMember::Volatile`), because it would leave the reading to
/// Rust - and because the `const T&` it returns will not bind to a
/// `const volatile T` anyway.
pub(crate) fn add_field_accessors(
    apis: ApiVec<PodPhase>,
    config: &IncludeCppConfig,
    parse_callback_results: &ParseCallbackResults,
) -> ApiVec<PodPhase> {
    let nested_cpp_names = NestedCppNames::new(config, apis.iter().map(|api| api.name_info()));
    let types = TypeFacts {
        pod_safe: pod_safe_types(&apis),
        enums: apis
            .iter()
            .filter(|api| matches!(api, Api::Enum { .. }))
            .map(|api| api.name().clone())
            .collect(),
        nameable: nameable_types(&apis, &nested_cpp_names),
    };
    let mut names_taken: HashSet<QualifiedName> =
        apis.iter().map(|api| api.name().clone()).collect();
    let mut accessors = ApiVec::new();
    for api in apis.iter() {
        let Api::Struct {
            name,
            details,
            analysis:
                analysis @ PodAnalysis {
                    kind: TypeKind::NonPod,
                    num_generics: 0,
                    ..
                },
        } = api
        else {
            continue;
        };
        // A type generated only because something else mentioned it gets no
        // methods at all (`MethodOfNonAllowlistedType`), so accessors for one
        // would be nothing but ignored-item stubs.
        if !nested_cpp_names.is_on_allowlist(&name.name) {
            continue;
        }
        for member in parse_callback_results
            .data_members(&name.name)
            .unwrap_or_default()
        {
            if let Some(accessor) = accessor_for(
                &name.name,
                details,
                analysis,
                member,
                &types,
                &mut names_taken,
            ) {
                accessors.push(accessor);
            }
        }
    }
    // Appended after everything bindgen gave us, so that a field and a method
    // of the same name resolve the way a reader would expect: the real C++
    // method is analysed first and keeps the name, and the accessor takes the
    // overload suffix.
    let mut results = apis;
    results.append(&mut accessors);
    results
}

/// The API carrying the accessor for one data member - the getter itself, or
/// the documented refusal where the member is of a kind we write no getter for
/// - and `None` where the member is one of the two we pass over entirely.
fn accessor_for(
    self_ty: &QualifiedName,
    details: &StructDetails,
    analysis: &PodAnalysis,
    member: &DataMember,
    types: &TypeFacts,
    names_taken: &mut HashSet<QualifiedName>,
) -> Option<Api<PodPhase>> {
    if !member.is_public || member.is_bitfield {
        return None;
    }
    let (name, cpp_name) = member.name.as_ref().zip(member.cpp_name.as_ref())?;
    // bindgen reports every member C++ laid out, including ones it generates
    // no field for - an opaque type keeps its members' facts and shows a blob.
    // No field means no type to write, so there is no accessor to be had.
    let field = details
        .item
        .fields
        .iter()
        .find(|field| field.ident.as_ref().is_some_and(|id| id == name))?;
    // Nor is there one for a member whose type the converter turned down. Such
    // a refusal is not fatal to a non-POD class - the class is opaque and the
    // field is never rendered, so the reason is dropped rather than reported -
    // and there is no type here to write a signature with. A member which is
    // unusable because its own type is unusable is the one case here which
    // says nothing about itself.
    let field_info = analysis
        .field_info
        .iter()
        .find(|info| info.name.as_deref() == Some(name.as_str()))?;
    let api_name = first_free_name(self_ty, name, names_taken);
    names_taken.insert(api_name.clone());
    Some(match member_shape(field, field_info, types) {
        Ok(shape) => Api::Function {
            name: ApiName::new_from_qualified_name_and_cpp_name(
                api_name.clone(),
                Some(CppOriginalName::from_data_member_name(cpp_name)),
            ),
            fun: Box::new(getter(self_ty, &api_name, field, cpp_name, shape)),
            analysis: (),
        },
        // Refused where the accessor is built rather than left to the
        // machinery downstream, which would write C++ that does not compile.
        // The member is public, so a caller looking for it is entitled to be
        // told why it is missing; this becomes a documented stub in the output
        // mod, exactly as a method autocxx cannot generate does.
        Err(refusal) => Api::IgnoredItem {
            name: ApiName::new_from_qualified_name(api_name),
            err: refusal.into_error(format!("{}::{cpp_name}", self_ty.to_cpp_name())),
            ctx: Some(ErrorContext::new_for_method(
                self_ty.get_final_ident(),
                make_ident(name),
            )),
        },
    })
}

/// Every type an accessor may name: one the user asked for, and one autocxx
/// manufactured for itself.
///
/// A type autocxx generates only because something else mentioned it gets no
/// methods of its own (`MethodOfNonAllowlistedType`), and an accessor handing
/// out a reference to one would be the only thing keeping it in the output -
/// which is how a C++ name bindgen reported wrongly gets written out for the
/// first time. `test_colliding_names_from_template_members` is that case: a
/// member class of a template specialization is reported as a member of the
/// class which happens to use it, and no such type exists in C++. So an
/// accessor is written only where the type is already the user's business,
/// and where it is not, the refusal says which directive would change that.
///
/// A concrete type is included because its C++ name is autocxx's own - a
/// typedef we emit beside it - so nothing bindgen said can be wrong about it.
fn nameable_types<T: crate::conversion::api::AnalysisPhase>(
    apis: &ApiVec<T>,
    nested_cpp_names: &NestedCppNames,
) -> HashSet<QualifiedName> {
    apis.iter()
        .map(|api| api.name())
        .filter(|name| nested_cpp_names.is_on_allowlist(name))
        .chain(apis.iter().filter_map(|api| match api {
            Api::ConcreteType { .. } => Some(api.name()),
            _ => None,
        }))
        .cloned()
        .collect()
}

/// The synthesized function itself, written the way bindgen writes a method so
/// that the ordinary function analysis makes a method of it: a `this` pointer,
/// a C++ name taken from the member, and a body which reads the field.
fn getter(
    self_ty: &QualifiedName,
    api_name: &QualifiedName,
    field: &Field,
    cpp_name: &str,
    shape: Shape,
) -> FuncToConvert {
    let self_ty_path = self_ty.to_type_path();
    let this: FnArg = parse_quote! { this: *const #self_ty_path };
    // The `const` marker is peeled off whichever shape we take. It says C++
    // qualified the member's own type, which is not something either shape
    // needs to repeat: the copy a by-value getter returns is not bound by it,
    // and leaving it on would make the wrapper return a `const` value - the
    // very thing google/autocxx#1191 works around elsewhere - while a
    // reference to it is `const` already, being taken from a `const` receiver.
    let ty = strip_const_markers(&field.ty);
    let output: ReturnType = match shape {
        Shape::ByValue => parse_quote! { -> #ty },
        Shape::ByReference => parse_quote! { -> __bindgen_marker_Reference < *const #ty > },
    };
    FuncToConvert {
        provenance: Provenance::SynthesizedOther,
        ident: api_name.get_final_ident(),
        doc_attrs: accessor_doc(cpp_name),
        inputs: [this].into_iter().collect(),
        variadic: false,
        output,
        vis: parse_quote! { pub },
        virtualness: None,
        cpp_vis: CppVisibility::Public,
        special_member: None,
        // Said outright, because the name is C++'s and not ours: C++ lets a
        // class have a data member of its own name, and `Bob::Bob` read as a
        // name is exactly what a constructor is called. See
        // `constructor_with_suffix`.
        method_kind: Some(CppMethodKind::Normal),
        original_name: None,
        self_ty: None,
        synthesized_this_type: None,
        add_to_trait: None,
        synthetic_cpp: Some((
            CppFunctionBody::FieldRead(cpp_name.to_string()),
            CppFunctionKind::Method,
        )),
        is_deleted: None,
        deprecation: None,
        ref_qualifier: CppRefQualifier::None,
    }
}

/// What deciding one member's accessor needs to know about every other type,
/// gathered once for the whole set of APIs.
struct TypeFacts {
    /// The types Rust may hold by value.
    pod_safe: HashSet<QualifiedName>,
    /// The enumerations, which are copyable whatever else they are.
    enums: HashSet<QualifiedName>,
    /// The types an accessor may name; see [`nameable_types`].
    nameable: HashSet<QualifiedName>,
}

/// How a member of a given type comes back to Rust.
enum Shape {
    /// C++ hands a scalar, a pointer or a POD back by value, and so do we: a
    /// `&u32` accessor would be worse in every way.
    ByValue,
    /// Everything else is borrowed rather than copied. `self.member` by value
    /// would demand a copy constructor the member may not have, and would hand
    /// back a copy of something the caller asked to read; the reference
    /// borrows the receiver and needs neither.
    ByReference,
}

/// Why a member gets no accessor.
enum Refusal {
    /// Nothing autocxx could write would be a getter for a member of this
    /// kind.
    Kind(UnrepresentableMember),
    /// The member's type is one autocxx would not otherwise put in the output.
    NotNameable(QualifiedName),
}

impl Refusal {
    fn into_error(self, member: String) -> ConvertErrorFromCpp {
        match self {
            Self::Kind(kind) => ConvertErrorFromCpp::UnrepresentableDataMember(member, kind),
            Self::NotNameable(ty) => {
                ConvertErrorFromCpp::DataMemberOfNonAllowlistedType(member, ty)
            }
        }
    }
}

/// Which of the two shapes a member gets, or why it gets no accessor.
///
/// Decided on the *converted* type rather than on what bindgen wrote, because
/// the two disagree wherever an alias stands in the way: `using Ref = const
/// int&` reaches the field as a path, and only the conversion says it is a
/// reference. The unconverted type is still what the accessor's return type is
/// written from, and both shapes are built to survive the same conversion the
/// field's own type went through.
fn member_shape(
    field: &Field,
    field_info: &FieldInfo,
    types: &TypeFacts,
) -> Result<Shape, Refusal> {
    if matches!(
        field_info.type_kind,
        type_converter::TypeKind::Reference
            | type_converter::TypeKind::MutableReference
            | type_converter::TypeKind::RValueReference
    ) {
        return Err(Refusal::Kind(UnrepresentableMember::Reference));
    }
    if matches!(strip_const_markers(&field.ty), Type::Array(_))
        || matches!(&field_info.ty, Type::Array(_))
    {
        return Err(Refusal::Kind(UnrepresentableMember::Array));
    }
    // A pointer member is handed over as the pointer it is - always copyable,
    // whatever it points at - and the type converter has already had its say
    // about what may be pointed at.
    if is_pointer_like(&field_info.ty) {
        return Ok(Shape::ByValue);
    }
    let Type::Path(typ) = &field_info.ty else {
        // Nothing else is a shape a return type can be written around. The
        // converter makes a field's type a path, a pointer, an array or a
        // reference, and the three which are not this have been answered
        // above.
        return Err(Refusal::Kind(UnrepresentableMember::Unrepresentable));
    };
    let tn = QualifiedName::from_type_path(typ);
    if !known_types().is_known_type(&tn) && !types.nameable.contains(&tn) {
        return Err(Refusal::NotNameable(tn));
    }
    // A by-value getter copies the member out of a `const` object, which needs
    // a copy constructor C++ has not deleted - a different question from
    // whether Rust may hold the type by value. A `std::unique_ptr` member is
    // POD-safe and uncopyable; so is a POD struct whose class declared a move
    // constructor, which deletes the implicit copy one and stays trivially
    // relocatable. Neither of those has anything here to be asked, so the
    // by-value shape is kept to the types which are copyable by construction:
    // the built-ins, whose Rust and C++ spellings are the same object, and
    // enumerations, which have no special members at all.
    let copyable_by_construction = types.enums.contains(&tn)
        || (known_types().is_known_type(&tn) && !known_types().lacks_copy_constructor(&tn));
    let by_value = copyable_by_construction && types.pod_safe.contains(&tn);
    if is_volatile_qualified(&field.ty) {
        // A `volatile` member is readable by value, and only by value: the
        // getter's body is `obj.member`, and reading a `volatile` glvalue is a
        // volatile access in C++, so the read happens in the generated C++,
        // once per call, and what crosses to Rust is the copy it produced.
        //
        // Only for a scalar, though - a built-in, an enumeration or a
        // pointer. Copying a class calls a constructor, and an implicitly
        // declared copy constructor takes `const T&` or `T&`, neither of which
        // a `volatile T` binds to, so `obj.member` does not compile for one
        // however copyable it otherwise is. The borrowed shape does not work
        // either: it would leave Rust to do the reading, as an ordinary load,
        // and its `const T&` return will not bind to a `const volatile T`.
        return if by_value
            && (known_types().copyable_from_volatile(&tn) || types.enums.contains(&tn))
        {
            Ok(Shape::ByValue)
        } else {
            Err(Refusal::Kind(UnrepresentableMember::Volatile))
        };
    }
    if by_value {
        Ok(Shape::ByValue)
    } else {
        Ok(Shape::ByReference)
    }
}

/// A name for the accessor's API which nothing else has taken.
fn first_free_name(
    self_ty: &QualifiedName,
    field_name: &str,
    names_taken: &HashSet<QualifiedName>,
) -> QualifiedName {
    let stem = format!("{}_{field_name}{ACCESSOR_SUFFIX}", self_ty.get_final_item());
    let ns = self_ty.get_namespace();
    let mut candidate = QualifiedName::new(ns, make_ident(&stem));
    let mut n = 1;
    while names_taken.contains(&candidate) {
        candidate = QualifiedName::new(ns, make_ident(format!("{stem}{n}")));
        n += 1;
    }
    candidate
}

fn accessor_doc(cpp_name: &str) -> Vec<Attribute> {
    let doc = format!(
        "Reads the C++ data member `{cpp_name}`. autocxx generated this accessor because \
         this type reaches Rust as an opaque one, whose fields Rust may not name."
    );
    let attr: syn::Attribute = parse_quote! { #[doc = #doc] };
    vec![attr.into()]
}
