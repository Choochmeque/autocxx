// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

mod byvalue_checker;

use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;

use autocxx_parser::IncludeCppConfig;
use byvalue_checker::ByValueChecker;
use syn::{ItemStruct, Type, Visibility};

use crate::{
    conversion::{
        analysis::type_converter::{
            self, add_analysis, attach_deferred_holder_surfaces, TypeConversionContext,
            TypeConverter,
        },
        api::{AnalysisPhase, Api, ApiName, NestedCppNames, NullPhase, StructDetails, TypeKind},
        apivec::ApiVec,
        check_for_fatal_attrs,
        convert_error::{ConvertErrorWithContext, ErrorContext},
        error_reporter::convert_apis,
        type_helpers::array_element_type,
        ConvertErrorFromCpp,
    },
    known_types::known_types,
    parse_callbacks::{BaseClass, DataMember, UsingDeclaration},
    types::{Namespace, QualifiedName},
    ParseCallbackResults,
};

use super::tdef::{TypedefAnalysis, TypedefPhase};

#[derive(std::fmt::Debug)]

pub(crate) struct FieldInfo {
    /// The field's name, as bindgen spelled it. Absent for the members of an
    /// anonymous union, which bindgen doesn't name. Only used to explain
    /// things to the user, never to generate code.
    pub(crate) name: Option<String>,
    pub(crate) ty: Type,
    pub(crate) type_kind: type_converter::TypeKind,
    pub(crate) bindgen_opaque_data: bool,
    /// Whether C++ declared the field itself `const`, as opposed to it
    /// pointing at something const. Deletes the implicitly declared default
    /// constructor unless the field also has a default member initializer;
    /// see `find_constructors_present`.
    pub(crate) is_const: bool,
    /// Whether C++ gave the field a default member initializer, `int x = 5;`.
    /// One stands in for whatever an implicitly declared default constructor
    /// would otherwise have had to do with the field.
    pub(crate) has_default_initializer: bool,
}

#[derive(std::fmt::Debug)]
pub(crate) struct PodAnalysis {
    pub(crate) kind: TypeKind,
    /// Every base class, whether or not bindgen generated a field for it.
    pub(crate) bases: HashSet<QualifiedName>,
    /// The subset of `bases` C++ inherits publicly. Only through one of these
    /// is a base's member reachable from outside the class, so this is the
    /// ancestry a caller sees; `bases` is the ancestry name lookup walks,
    /// which C++ runs before it applies access control at all.
    pub(crate) public_bases: HashSet<QualifiedName>,
    /// Whether this class has a base bindgen reported but could not name -
    /// a template instantiation, which it announces through no callback. Such
    /// a base is missing from `bases` above, so the sets there are known to be
    /// incomplete and the constructor analysis declines to run C++'s rules.
    /// Abstractness does not account for it: a pure virtual inherited from an
    /// unnameable base still goes unnoticed, exactly as every base did before
    /// bindgen reported any.
    pub(crate) has_unnamed_base: bool,
    /// The subset of `bases` this type inherits virtually. Such a base is one
    /// subobject shared with everything else which inherits it virtually, so
    /// it sits at no fixed offset here, and a function overriding one of its
    /// pure virtuals overrides it for every class sharing it.
    pub(crate) virtual_bases: HashSet<QualifiedName>,
    /// The `using Base::foo;` declarations C++ wrote in this class, which
    /// make base class members nameable through it. bindgen generates nothing
    /// for one, so the member is reachable in C++ and not in the generated
    /// Rust unless something puts it back.
    pub(crate) using_declarations: Vec<UsingDeclaration>,
    /// Base classes for which we should create casts.
    /// That's just those which are on the allowlist,
    /// because otherwise we don't know whether they're
    /// abstract or not.
    pub(crate) castable_bases: HashSet<QualifiedName>,
    /// All field types. e.g. for std::unique_ptr<A>, this would include
    /// both std::unique_ptr and A
    pub(crate) field_deps: HashSet<QualifiedName>,
    /// Types within fields where we need a definition, e.g. for
    /// std::unique_ptr<A> it would just be std::unique_ptr.
    pub(crate) field_definition_deps: HashSet<QualifiedName>,
    pub(crate) field_info: Vec<FieldInfo>,
    /// The class's bitfield members. `field_info` has no entry for one:
    /// bindgen gives a run of bitfields a single allocation unit and accessors
    /// over it, so the only thing which says a bitfield exists, let alone what
    /// its own type was, is bindgen's per-member report.
    pub(crate) bitfields: Vec<DataMember>,
    pub(crate) num_generics: usize,
    pub(crate) in_anonymous_namespace: bool,
}

#[derive(std::fmt::Debug)]
pub(crate) struct PodPhase;

impl AnalysisPhase for PodPhase {
    type TypedefAnalysis = TypedefAnalysis;
    type StructAnalysis = PodAnalysis;
    type FunAnalysis = ();
    type SubclassAnalysis = ();
}

/// In our set of APIs, work out which ones are safe to represent
/// by value in Rust (e.g. they don't have a destructor) and record
/// as such. Return a set of APIs annotated with extra metadata,
/// and an object which can be used to query the POD status of any
/// type whether or not it's one of the [Api]s.
pub(crate) fn analyze_pod_apis(
    apis: ApiVec<TypedefPhase>,
    config: &IncludeCppConfig,
    parse_callback_results: &ParseCallbackResults,
) -> Result<ApiVec<PodPhase>, ConvertErrorFromCpp> {
    // This next line will return an error if any of the 'generate_pod'
    // directives from the user can't be met because, for instance,
    // a type contains a std::string or some other type which can't be
    // held safely by value in Rust.
    let byvalue_checker = ByValueChecker::new_from_apis(&apis, config, parse_callback_results)?;
    // A base class may be a nested type which the user allowlisted by the name
    // C++ gives it; see google/autocxx#1422.
    let nested_cpp_names = NestedCppNames::new(config, apis.iter().map(|api| api.name_info()));
    let mut extra_apis = ApiVec::new();
    let mut type_converter = TypeConverter::new(config, &apis, parse_callback_results);
    let mut results = ApiVec::new();
    convert_apis(
        apis,
        &mut results,
        Api::fun_unchanged,
        |name, details, _| {
            analyze_struct(
                &byvalue_checker,
                &mut type_converter,
                &mut extra_apis,
                name,
                details,
                &nested_cpp_names,
                parse_callback_results,
            )
        },
        |name, item| analyze_enum(name, item, parse_callback_results),
        Api::typedef_unchanged,
        Api::subclass_unchanged,
    );
    // Conceivably, the process of POD-analysing the first set of APIs could result
    // in us creating new APIs to concretize generic types.
    let extra_apis: ApiVec<PodPhase> = extra_apis.into_iter().map(add_analysis).collect();
    let mut more_extra_apis = ApiVec::new();
    convert_apis(
        extra_apis,
        &mut results,
        Api::fun_unchanged,
        |name, details, _| {
            analyze_struct(
                &byvalue_checker,
                &mut type_converter,
                &mut more_extra_apis,
                name,
                details,
                &nested_cpp_names,
                parse_callback_results,
            )
        },
        |name, item| analyze_enum(name, item, parse_callback_results),
        Api::typedef_unchanged,
        Api::subclass_unchanged,
    );
    assert!(more_extra_apis.is_empty());
    // As in `convert_typedef_targets`: a surface this phase's conversions
    // worked out for a holder which already existed goes on it before this
    // converter is discarded.
    Ok(attach_deferred_holder_surfaces(
        &mut type_converter,
        results,
    ))
}

/// The types Rust may hold, and hand to and from C++, by value: the ones this
/// analysis found or was told are POD, plus the built-ins whose Rust and C++
/// spellings are the same object.
///
/// Everything else crosses in a `cxx::UniquePtr` or behind a reference, so
/// this set is what decides the shape of any signature autocxx writes for
/// itself as well as the ones it converts.
pub(crate) fn pod_safe_types(apis: &ApiVec<PodPhase>) -> HashSet<QualifiedName> {
    apis.iter()
        .filter_map(|api| match api {
            Api::Struct {
                analysis:
                    PodAnalysis {
                        kind: TypeKind::Pod,
                        ..
                    },
                ..
            } => Some(api.name().clone()),
            Api::Enum { .. } => Some(api.name().clone()),
            Api::ExternCppType { pod: true, .. } => Some(api.name().clone()),
            _ => None,
        })
        .chain(
            known_types()
                .get_pod_safe_types()
                .filter_map(|(tn, is_pod_safe)| if is_pod_safe { Some(tn) } else { None }),
        )
        .collect()
}

fn analyze_enum(
    name: ApiName,
    item: crate::minisyn::ItemEnum,
    parse_callback_results: &ParseCallbackResults,
) -> Result<Box<dyn Iterator<Item = Api<PodPhase>>>, ConvertErrorWithContext> {
    check_for_fatal_attrs(parse_callback_results, &name.name)?;
    Ok(Box::new(std::iter::once(Api::Enum { name, item })))
}

fn analyze_struct(
    byvalue_checker: &ByValueChecker,
    type_converter: &mut TypeConverter,
    extra_apis: &mut ApiVec<NullPhase>,
    name: ApiName,
    details: Box<StructDetails>,
    nested_cpp_names: &NestedCppNames,
    parse_callback_results: &ParseCallbackResults,
) -> Result<Box<dyn Iterator<Item = Api<PodPhase>>>, ConvertErrorWithContext> {
    let id = name.name.get_final_ident();
    check_for_fatal_attrs(parse_callback_results, &name.name)?;
    let (bases, has_unnamed_base) = get_bases(&name.name, &details.item, parse_callback_results);
    let data_members = parse_callback_results
        .data_members(&name.name)
        .unwrap_or_default();
    let using_declarations = parse_callback_results
        .using_declarations(&name.name)
        .to_vec();
    let mut field_deps = HashSet::new();
    let mut field_definition_deps = HashSet::new();
    let mut field_info = Vec::new();
    let field_conversion_errors = get_struct_field_types(
        type_converter,
        name.name.get_namespace(),
        &details.item,
        &mut field_deps,
        &mut field_definition_deps,
        &mut field_info,
        extra_apis,
    );
    add_reported_field_facts(&mut field_info, data_members);
    let bitfields = data_members
        .iter()
        .filter(|member| member.is_bitfield)
        .cloned()
        .collect();
    let type_kind = if field_info.iter().any(|fi| fi.bindgen_opaque_data) {
        TypeKind::Opaque
    } else if byvalue_checker.is_pod(&name.name) {
        // It's POD so any errors encountered parsing its fields are important.
        // Let's not allow anything to be POD if it's got rvalue reference fields.
        if details.has_rvalue_reference_fields {
            return Err(ConvertErrorWithContext(
                ConvertErrorFromCpp::RValueReferenceField,
                Some(ErrorContext::new_for_item(id)),
            ));
        }
        if let Some(err) = field_conversion_errors.into_iter().next() {
            return Err(ConvertErrorWithContext(
                err,
                Some(ErrorContext::new_for_item(id)),
            ));
        }
        TypeKind::Pod
    } else {
        TypeKind::NonPod
    };
    let public_bases: HashSet<QualifiedName> = bases
        .iter()
        .filter(|(_, base)| base.is_public)
        .map(|(base, _)| base.clone())
        .collect();
    let castable_bases = public_bases
        .iter()
        .filter(|base| nested_cpp_names.is_on_allowlist(base))
        .cloned()
        .collect();
    let virtual_bases = bases
        .iter()
        .filter(|(_, base)| base.is_virtual)
        .map(|(base, _)| base.clone())
        .collect();
    let num_generics = details.item.generics.params.len();
    let in_anonymous_namespace = name
        .name
        .ns_segment_iter()
        .any(|ns| ns.starts_with("_bindgen_mod"));
    Ok(Box::new(std::iter::once(Api::Struct {
        name,
        details,
        analysis: PodAnalysis {
            kind: type_kind,
            bases: bases.into_keys().collect(),
            public_bases,
            using_declarations,
            has_unnamed_base,
            virtual_bases,
            castable_bases,
            field_deps,
            field_definition_deps,
            field_info,
            bitfields,
            num_generics,
            in_anonymous_namespace,
        },
    })))
}

fn get_struct_field_types(
    type_converter: &mut TypeConverter,
    ns: &Namespace,
    s: &ItemStruct,
    field_deps: &mut HashSet<QualifiedName>,
    field_definition_deps: &mut HashSet<QualifiedName>,
    field_info: &mut Vec<FieldInfo>,
    extra_apis: &mut ApiVec<NullPhase>,
) -> Vec<ConvertErrorFromCpp> {
    let mut convert_errors = Vec::new();
    let struct_type_params = s
        .generics
        .type_params()
        .map(|tp| tp.ident.clone())
        .collect();
    let type_conversion_context = TypeConversionContext::WithinStructField { struct_type_params };
    for f in &s.fields {
        let annotated = type_converter.convert_type(f.ty.clone(), ns, &type_conversion_context);
        match annotated {
            Ok(mut r) => {
                extra_apis.append(&mut r.extra_apis);
                // Skip base classes represented as fields. Anything which wants to include bases can chain
                // those to the list we're building.
                if !f
                    .ident
                    .as_ref()
                    .map(|id| {
                        id.to_string().starts_with("_base")
                            || id.to_string().starts_with("__bindgen_padding")
                    })
                    .unwrap_or(false)
                {
                    field_deps.extend(r.types_encountered);
                    if let Type::Path(typ) = array_element_type(&r.ty) {
                        // Later analyses need to know about the field
                        // types where we need full definitions, as opposed
                        // to just declarations. That means just the outermost
                        // type path - and, for an array, its element type,
                        // since holding N of something by value needs its
                        // definition just as much as holding one does.
                        field_definition_deps.insert(QualifiedName::from_type_path(typ));
                    }
                    field_info.push(FieldInfo {
                        name: f.ident.as_ref().map(|id| id.to_string()),
                        ty: r.ty,
                        type_kind: r.kind,
                        is_const: r.is_const,
                        has_default_initializer: false,
                        bindgen_opaque_data: f
                            .ident
                            .as_ref()
                            .map(|id| id == "_bindgen_opaque_blob")
                            .unwrap_or_default(),
                    });
                }
            }
            Err(e) => convert_errors.push(e),
        };
    }
    convert_errors
}

/// Fills in what bindgen reported about each member rather than rendered into
/// the field it emitted, joined on the name bindgen generated for it.
///
/// Bitfields are excluded from the join because they have no field here at
/// all, and their reported name is the one their *accessors* are built from -
/// which a real field may also hold, since C++ lets a member called `type`
/// and a member called `type_` coexist and bindgen mangles the first into the
/// second.
fn add_reported_field_facts(field_info: &mut [FieldInfo], data_members: &[DataMember]) {
    for field in field_info.iter_mut() {
        let Some(name) = field.name.as_ref() else {
            continue;
        };
        let Some(reported) = data_members
            .iter()
            .find(|member| !member.is_bitfield && member.name.as_ref() == Some(name))
        else {
            continue;
        };
        field.has_default_initializer = reported.has_default_member_initializer;
        // Two channels for the same fact, and each sees cases the other does
        // not. The marker survives an alias bindgen renders with it; the
        // report reads the member's type in bindgen's IR, where the qualifier
        // is still on whichever link of an alias chain C++ put it.
        field.is_const |= reported.is_const;
    }
}

/// The base classes of a type.
///
/// bindgen reports every base through `denote_base_class`, including the ones
/// it generates no field for - a base it finds zero-sized, and a virtual base,
/// which the object reaches indirectly. Those two are invisible in the generated struct, so
/// reading bases off the fields alone concluded that
/// `struct D : Empty { int x; };` had no bases at all, and D then got its
/// implicit special members, its abstractness and its upcasts from the wrong
/// set of ancestors.
///
/// The `_base` fields are read as well, because bindgen's report is not the
/// only route in: `conversion_tests` hands the conversion phases bindgen
/// output it wrote by hand, with no callbacks behind it. A base which arrives
/// both ways is described by the report, which knows the C++ access specifier
/// rather than guessing it from the field's Rust visibility.
fn get_bases(
    name: &QualifiedName,
    item: &ItemStruct,
    parse_callback_results: &ParseCallbackResults,
) -> (HashMap<QualifiedName, BaseClass>, bool) {
    let mut bases: HashMap<QualifiedName, BaseClass> = item
        .fields
        .iter()
        .filter_map(|f| {
            let is_public = matches!(f.vis, Visibility::Public(_));
            match &f.ty {
                Type::Path(typ) => f
                    .ident
                    .as_ref()
                    .filter(|id| id.to_string().starts_with("_base"))
                    .map(|_| {
                        let name = QualifiedName::from_type_path(typ);
                        (
                            name.clone(),
                            BaseClass {
                                name,
                                is_virtual: false,
                                is_public,
                            },
                        )
                    }),
                _ => None,
            }
        })
        .collect();
    let reported = parse_callback_results.get_bases(name);
    for base in reported.iter().flat_map(|bases| bases.named.iter()) {
        // A base which is somehow this class itself would make the class its
        // own ancestor, and `fields_and_bases_first` treats that as a
        // dependency cycle and panics. C++ has no such class, so the only way
        // to arrive at one is two classes sharing a name here: bindgen
        // flattens `Outer::Inner` to `Outer_Inner`, which a class actually
        // called `Outer_Inner` in the same namespace already answers to.
        if base.name == *name {
            continue;
        }
        bases.insert(base.name.clone(), base.clone());
    }
    (bases, reported.is_some_and(|bases| bases.any_unnamed))
}
