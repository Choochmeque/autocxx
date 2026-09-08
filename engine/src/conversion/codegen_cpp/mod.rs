// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

mod function_wrapper_cpp;
mod move_or_copy_prelude;
mod new_and_delete_prelude;
pub(crate) mod type_to_cpp;

use crate::{
    conversion::analysis::fun::{function_wrapper::CppFunctionKind, FnAnalysis},
    types::QualifiedName,
    CppCodegenOptions, CppFilePair,
};
use autocxx_parser::IncludeCppConfig;
use indexmap::map::IndexMap as HashMap;
use indexmap::set::IndexSet as HashSet;
use indoc::indoc;
use itertools::Itertools;
use std::borrow::Cow;
use type_to_cpp::CppNameMap;

use crate::conversion::analysis::fun::ReceiverMutability;
use crate::conversion::array_witness::{array_element_witnesses, witness_name};
use crate::minisyn::Ident;

use super::{
    analysis::{
        fun::{
            function_wrapper::{CppFunction, CppFunctionBody, RECEIVER_ARG_NAME},
            FnPhase, PodAndDepAnalysis, SubclassAnalysis,
        },
        pod::PodAnalysis,
    },
    api::{
        Api, ConstRefShim, HolderSurface, Provenance, SharedPtrShim, SubclassName, TypeKind,
        UniquePtrShim, VectorShim, WeakPtrShim,
    },
    apivec::ApiVec,
    parse::CppRefQualifier,
    ConvertErrorFromCpp, CppEffectiveName,
};
use crate::vendored_bindgen::callbacks::Visibility as CppVisibility;

static GENERATED_FILE_HEADER: &str =
    "// Generated using autocxx - do not edit directly.\n// @generated.\n\n";

/// The one C++ built-in whose width is the target's to choose, and so the one
/// the generated C++ has to be held to. See
/// [`CppCodeGenerator::assert_wchar_t_width`].
static WCHAR_T: &str = "wchar_t";

/// Bracket generated C++ which names a declaration C++ marked `[[deprecated]]`
/// with a pragma silencing `-Wdeprecated-declarations` for it, and nothing
/// else.
///
/// The warning is right about the code it fires on and useless to the person
/// reading it: nobody edits this file, and the wrapper is emitted for
/// everything `generate!` names whether or not any Rust calls it, so a project
/// building with `-Werror` cannot bind a deprecated function at all. The
/// signal is not dropped, it moves: the Rust binding carries `#[deprecated]`
/// with the same message, so the caller who asked for the function is the one
/// who hears about it. See google/autocxx#1403.
///
/// clang defines `__GNUC__` and honours `#pragma GCC diagnostic`, so the two
/// arms between them cover every compiler autocxx generates for.
fn silence_deprecation(code: String) -> String {
    format!(
        "{}\n{code}\n{}",
        indoc! {"
            #if defined(_MSC_VER)
            #pragma warning(push)
            #pragma warning(disable : 4996)
            #elif defined(__GNUC__)
            #pragma GCC diagnostic push
            #pragma GCC diagnostic ignored \"-Wdeprecated-declarations\"
            #endif"},
        indoc! {"
            #if defined(_MSC_VER)
            #pragma warning(pop)
            #elif defined(__GNUC__)
            #pragma GCC diagnostic pop
            #endif"},
    )
}

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Hash)]
enum Header {
    System(&'static str),
    CxxH,
    CxxgenH,
    NewDeletePrelude,
    MoveOrCopyPrelude,
}

impl Header {
    fn include_stmt(
        &self,
        cpp_codegen_options: &CppCodegenOptions,
        cxxgen_header_name: &str,
    ) -> String {
        let blank = "".to_string();
        match self {
            Self::System(name) => format!("#include <{name}>"),
            Self::CxxH => {
                let prefix = cpp_codegen_options.path_to_cxx_h.as_ref().unwrap_or(&blank);
                format!("#include \"{prefix}cxx.h\"")
            }
            Self::CxxgenH => {
                let prefix = cpp_codegen_options
                    .path_to_cxxgen_h
                    .as_ref()
                    .unwrap_or(&blank);
                format!("#include \"{prefix}{cxxgen_header_name}\"")
            }
            Header::NewDeletePrelude => new_and_delete_prelude::NEW_AND_DELETE_PRELUDE.to_string(),
            Header::MoveOrCopyPrelude => move_or_copy_prelude::MOVE_OR_COPY_PRELUDE.to_string(),
        }
    }

    fn is_system(&self) -> bool {
        matches!(self, Header::System(_) | Header::CxxH)
    }
}

enum ConversionDirection {
    RustCallsCpp,
    CppCallsCpp,
    CppCallsRust,
}

/// Some extra snippet of C++ which we (autocxx) need to generate, beyond
/// that which cxx itself generates.
#[derive(Default)]
struct ExtraCpp {
    type_definition: Option<String>, // are output before main declarations
    declaration: Option<String>,
    definition: Option<String>,
    headers: Vec<Header>,
    cpp_headers: Vec<Header>,
}

/// Generates additional C++ glue functions needed by autocxx.
/// In some ways it would be preferable to be able to pass snippets
/// of C++ through to `cxx` for inclusion in the C++ file which it
/// generates, and perhaps we'll explore that in future. But for now,
/// autocxx generates its own _additional_ C++ files which therefore
/// need to be built and included in linking procedures.
pub(crate) struct CppCodeGenerator<'a> {
    additional_functions: Vec<ExtraCpp>,
    inclusions: String,
    original_name_map: CppNameMap,
    config: &'a IncludeCppConfig,
    cpp_codegen_options: &'a CppCodegenOptions<'a>,
    cxxgen_header_name: &'a str,
    /// See [`Self::assert_wchar_t_width`].
    wchar_t_size: u32,
}

struct SubclassFunction<'a> {
    fun: &'a CppFunction,
    /// See [`RustSubclassFnDetails::cpp_super_fn_name`].
    cpp_super_fn_name: Option<&'a Ident>,
}

impl<'a> CppCodeGenerator<'a> {
    pub(crate) fn generate_cpp_code(
        inclusions: String,
        apis: &ApiVec<FnPhase>,
        config: &'a IncludeCppConfig,
        cpp_codegen_options: &CppCodegenOptions,
        cxxgen_header_name: &str,
        shadowed_types: &HashSet<QualifiedName>,
        wchar_t_size: u32,
    ) -> Result<Option<CppFilePair>, ConvertErrorFromCpp> {
        let mut gen = CppCodeGenerator {
            additional_functions: Vec::new(),
            inclusions,
            original_name_map: CppNameMap::new_from_apis(apis, shadowed_types),
            config,
            cpp_codegen_options,
            cxxgen_header_name,
            wchar_t_size,
        };
        // These have to come first: everything else may refer to them, and the
        // type definitions are emitted in the order they're pushed.
        gen.generate_unshadowing_aliases();
        // The 'filter' on the following line is designed to ensure we don't accidentally
        // end up out of sync with needs_cpp_codegen
        gen.add_needs(apis.iter().filter(|api| api.needs_cpp_codegen()))?;
        gen.generate_array_element_witnesses(apis);
        Ok(gen.generate())
    }

    /// Emit the typedefs which let us name types whose own names are hidden by
    /// a variable of the same name. See
    /// [`crate::conversion::codegen_cpp::type_to_cpp::UnshadowingAlias`].
    fn generate_unshadowing_aliases(&mut self) {
        let typedefs = self
            .original_name_map
            .unshadowing_aliases()
            .map(|(name, alias)| {
                // Name the target from the global namespace, so that the
                // typedef can't pick up a different type of the same name
                // nested inside the namespace we're about to reopen.
                let typedef = format!(
                    "typedef {} ::{} {};",
                    alias.tag, alias.original_cpp_name, alias.alias
                );
                // Emit the typedef in the type's own namespace, so that only
                // the final segment of its C++ name changes. Wrap innermost
                // segment first, or a::b would come out as
                // `namespace b { namespace a { ... } }`.
                let segments: Vec<_> = name.ns_segment_iter().collect();
                segments.into_iter().rev().fold(typedef, |inner, segment| {
                    format!("namespace {segment} {{ {inner} }}")
                })
            })
            .join("\n");
        if typedefs.is_empty() {
            return;
        }
        // We always say `struct`, which is interchangeable with `class` in an
        // elaborated type specifier, but bindgen doesn't tell us which of the
        // two the type was declared with. Saying the wrong one is valid C++ but
        // draws a warning, so silence it rather than make callers' `-Werror`
        // builds depend on a distinction we can't see.
        self.additional_functions.push(ExtraCpp {
            type_definition: Some(format!(
                indoc! {"
                    #if defined(_MSC_VER)
                    #pragma warning(push)
                    #pragma warning(disable : 4099)
                    #elif defined(__clang__)
                    #pragma clang diagnostic push
                    #pragma clang diagnostic ignored \"-Wmismatched-tags\"
                    #endif
                    {}
                    #if defined(_MSC_VER)
                    #pragma warning(pop)
                    #elif defined(__clang__)
                    #pragma clang diagnostic pop
                    #endif"},
                typedefs
            )),
            ..Default::default()
        });
    }

    // It's important to keep this in sync with Api::needs_cpp_codegen.
    fn add_needs<'b>(
        &mut self,
        apis: impl Iterator<Item = &'a Api<FnPhase>>,
    ) -> Result<(), ConvertErrorFromCpp> {
        let mut constructors_by_subclass: HashMap<SubclassName, Vec<&CppFunction>> = HashMap::new();
        let mut methods_by_subclass: HashMap<SubclassName, Vec<SubclassFunction>> = HashMap::new();
        let mut deferred_apis = Vec::new();
        for api in apis {
            match &api {
                Api::StringConstructor { .. } => self.generate_string_constructor(),
                Api::Function {
                    analysis:
                        FnAnalysis {
                            cpp_wrapper: Some(cpp_wrapper),
                            ignore_reason: Ok(_),
                            externally_callable: true,
                            ..
                        },
                    fun,
                    ..
                } => {
                    if let Provenance::SynthesizedSubclassConstructor(details) = &fun.provenance {
                        constructors_by_subclass
                            .entry(details.subclass.clone())
                            .or_default()
                            .push(&details.cpp_impl);
                    }
                    self.generate_cpp_function(cpp_wrapper)?
                }
                Api::ConcreteType {
                    rs_definition,
                    cpp_definition,
                    holder_surface,
                    ..
                } => {
                    let effective_cpp_definition = match rs_definition {
                        Some(rs_definition) => {
                            Cow::Owned(self.original_name_map.type_to_cpp(rs_definition)?)
                        }
                        None => Cow::Borrowed(cpp_definition),
                    };

                    self.generate_typedef(api.name(), &effective_cpp_definition);
                    match holder_surface {
                        Some(HolderSurface::SharedPtr { .. }) => {
                            self.generate_shared_ptr_shims(api.name())
                        }
                        Some(HolderSurface::UniquePtr { .. }) => {
                            self.generate_unique_ptr_shims(api.name())
                        }
                        Some(HolderSurface::WeakPtr { shared_holder, .. }) => {
                            self.generate_weak_ptr_shims(api.name(), shared_holder)
                        }
                        Some(HolderSurface::VectorOfPointers { .. }) => {
                            self.generate_vector_shims(api.name())
                        }
                        Some(HolderSurface::ConstRef { .. }) => {
                            self.generate_const_ref_shims(api.name())
                        }
                        None => {}
                    }
                }
                Api::CType { typename, .. } => self.generate_ctype_typedef(typename),
                Api::Subclass { .. } => deferred_apis.push(api),
                Api::RustSubclassFn {
                    subclass, details, ..
                } => {
                    methods_by_subclass
                        .entry(subclass.clone())
                        .or_default()
                        .push(SubclassFunction {
                            fun: &details.cpp_impl,
                            cpp_super_fn_name: details.cpp_super_fn_name.as_ref(),
                        });
                }
                Api::Struct {
                    name,
                    analysis:
                        PodAndDepAnalysis {
                            pod:
                                PodAnalysis {
                                    kind: TypeKind::Pod,
                                    ..
                                },
                            constructors,
                            ..
                        },
                    ..
                } => {
                    let cpp_name = self.original_name_map.map(&name.name);
                    if constructors.destructor_omitted_as_trivial {
                        self.generate_trivial_destructor_assertion(cpp_name.clone());
                    }
                    self.generate_pod_assertion(cpp_name);
                }
                _ => panic!("Should have filtered on needs_cpp_codegen"),
            }
        }

        for api in deferred_apis.into_iter() {
            match api {
                Api::Subclass {
                    name,
                    superclass,
                    analysis:
                        SubclassAnalysis {
                            superclass_destructor_visibility,
                            superclass_destructor_virtual,
                        },
                } => self.generate_subclass(
                    superclass,
                    superclass_destructor_visibility,
                    *superclass_destructor_virtual,
                    name,
                    constructors_by_subclass.remove(name).unwrap_or_default(),
                    methods_by_subclass.remove(name).unwrap_or_default(),
                )?,
                _ => panic!("Unexpected deferred API"),
            }
        }
        Ok(())
    }

    fn generate(&self) -> Option<CppFilePair> {
        if self.additional_functions.is_empty() {
            None
        } else {
            let headers = self.collect_headers(|additional_need| &additional_need.headers);
            let cpp_headers = self.collect_headers(|additional_need| &additional_need.cpp_headers);
            let type_definitions = self.concat_additional_items(|x| x.type_definition.as_ref());
            let declarations = self.concat_additional_items(|x| x.declaration.as_ref());
            let declarations = format!(
                "{}\n#ifndef __AUTOCXXGEN_H__\n#define __AUTOCXXGEN_H__\n\n{}\n{}\n{}\n{}#endif // __AUTOCXXGEN_H__\n",
                GENERATED_FILE_HEADER, headers, self.inclusions, type_definitions, declarations
            );
            log::info!("Additional C++ decls:\n{}", declarations);
            let header_name = self
                .cpp_codegen_options
                .autocxxgen_header_namer
                .name_header(self.config.get_mod_name().to_string());
            let implementation = if self
                .additional_functions
                .iter()
                .any(|x| x.definition.is_some())
            {
                let definitions = self.concat_additional_items(|x| x.definition.as_ref());
                let definitions =
                    format!("{GENERATED_FILE_HEADER}\n#include \"{header_name}\"\n{cpp_headers}\n{definitions}");
                log::info!("Additional C++ defs:\n{}", definitions);
                Some(definitions.into_bytes())
            } else {
                None
            };
            Some(CppFilePair {
                header: declarations.into_bytes(),
                implementation,
                header_name,
            })
        }
    }

    fn collect_headers<F>(&self, filter: F) -> String
    where
        F: Fn(&ExtraCpp) -> &[Header],
    {
        let cpp_headers: HashSet<_> = self
            .additional_functions
            .iter()
            .flat_map(|x| filter(x).iter())
            .filter(|x| !self.cpp_codegen_options.suppress_system_headers || !x.is_system())
            .collect(); // uniqify
        cpp_headers
            .iter()
            .map(|x| x.include_stmt(self.cpp_codegen_options, self.cxxgen_header_name))
            .join("\n")
    }

    fn concat_additional_items<F>(&self, field_access: F) -> String
    where
        F: FnMut(&ExtraCpp) -> Option<&String>,
    {
        let mut s = self
            .additional_functions
            .iter()
            .flat_map(field_access)
            .join("\n");
        s.push('\n');
        s
    }

    /// Define the never-called by-value function which tells cxx that an
    /// array element is trivially movable.
    ///
    /// `inline`, and in the header rather than the implementation, because cxx
    /// calls it from a shim of its own: the definition has to be visible
    /// wherever that shim is compiled. See
    /// [`crate::conversion::array_witness::array_element_witnesses`] for why
    /// saying it at all is necessary.
    fn generate_array_element_witnesses(&mut self, apis: &ApiVec<FnPhase>) {
        for name in array_element_witnesses(apis) {
            let cpp_name = self.original_name_map.map(&name);
            let witness = witness_name(self.config, &name);
            self.additional_functions.push(ExtraCpp {
                declaration: Some(format!("inline void {witness}({cpp_name}) {{}}")),
                ..Default::default()
            })
        }
    }

    fn generate_pod_assertion(&mut self, name: String) {
        // These assertions are generated by cxx for trivial ExternTypes but
        // *only if* such types are used as trivial types in the cxx::bridge.
        // It's possible for types which we generate to be used even without
        // passing through the cxx::bridge, and as we generate Drop impls, that
        // can result in destructors for nested types being called multiple times
        // if we represent them as trivial types. So generate an extra
        // assertion to make sure.
        let declaration = Some(format!("static_assert(::rust::IsRelocatable<{name}>::value, \"type {name} should be trivially move constructible and trivially destructible to be used with generate_pod! in autocxx\");"));
        self.additional_functions.push(ExtraCpp {
            declaration,
            headers: vec![Header::CxxH],
            ..Default::default()
        })
    }

    /// Say in C++ what the trivial-destructor analysis assumed about this
    /// type, for a type where it concluded that no `impl Drop` was needed.
    ///
    /// This is not the same claim as the relocatability assertion above, and
    /// neither one covers the other. That one is cxx's `IsRelocatable`, which
    /// a user may opt into by hand - `using IsRelocatable = std::true_type;`
    /// on the type, or a `rust::IsRelocatable` specialization - so it does not
    /// establish anything about the destructor. This one asks the question we
    /// actually relied on, of the language rather than of a trait anyone may
    /// answer for. It matters because the analysis has a blind spot: where
    /// bindgen replaced a field's type with a blob of bytes, C++'s rules were
    /// run over a fiction, and a type which owns something with a destructor
    /// can come out looking trivially destructible. Rust would then never run
    /// that destructor, and nothing would say so. Now C++ does.
    fn generate_trivial_destructor_assertion(&mut self, name: String) {
        let declaration = Some(format!(
            "static_assert(::std::is_trivially_destructible<{name}>::value, \"autocxx generated no destructor call for {name}, because it worked out that C++ destroys one trivially, and this assertion says C++ disagrees. Something {name} owns - most likely through a member whose type bindgen replaced with a blob of bytes - has a destructor which does work, and Rust would never have run it. Use generate! rather than generate_pod! for this type.\");"
        ));
        self.additional_functions.push(ExtraCpp {
            declaration,
            headers: vec![Header::System("type_traits")],
            ..Default::default()
        })
    }

    fn generate_string_constructor(&mut self) {
        let makestring_name = self.config.get_makestring_name();
        let declaration = Some(format!("inline std::unique_ptr<std::string> {makestring_name}(::rust::Str str) {{ return std::make_unique<std::string>(std::string(str)); }}"));
        self.additional_functions.push(ExtraCpp {
            declaration,
            headers: vec![
                Header::System("memory"),
                Header::System("string"),
                Header::CxxH,
            ],
            ..Default::default()
        })
    }

    fn generate_cpp_function(&mut self, details: &CppFunction) -> Result<(), ConvertErrorFromCpp> {
        self.additional_functions
            .push(self.generate_cpp_function_inner(
                details,
                false,
                ConversionDirection::RustCallsCpp,
                false,
                None,
            )?);
        Ok(())
    }

    fn generate_cpp_function_inner(
        &self,
        details: &CppFunction,
        avoid_this: bool,
        conversion_direction: ConversionDirection,
        requires_rust_declarations: bool,
        force_name: Option<&CppEffectiveName>,
    ) -> Result<ExtraCpp, ConvertErrorFromCpp> {
        // Even if the original function call is in a namespace,
        // we generate this wrapper in the global namespace.
        // We could easily do this the other way round, and when
        // cxx::bridge comes to support nested namespace mods then
        // we wil wish to do that to avoid name conflicts. However,
        // at the moment this is simpler because it avoids us having
        // to generate namespace blocks in the generated C++.
        let is_a_method = !avoid_this
            && matches!(
                details.kind,
                CppFunctionKind::Method
                    | CppFunctionKind::ConstMethod
                    | CppFunctionKind::Constructor
            );
        let name = match force_name {
            Some(n) => n.to_string_for_cpp_generation().to_string(),
            None => details.wrapper_function_name.to_string(),
        };
        let get_arg_name = |counter: usize| -> String {
            if is_a_method && counter == 0 {
                // The receiver, under the name the `cxx::bridge` declaration
                // of this same function gives it - see `RECEIVER_ARG_NAME`.
                RECEIVER_ARG_NAME.to_string()
            } else {
                format!("arg{counter}")
            }
        };
        // If this returns a non-POD value, we may instead wish to emplace
        // it into a parameter, let's see.
        let args: Result<Vec<_>, _> = details
            .argument_conversion
            .iter()
            .enumerate()
            .map(|(counter, ty)| {
                Ok(format!(
                    "{} {}",
                    match conversion_direction {
                        ConversionDirection::RustCallsCpp =>
                            ty.unconverted_type(&self.original_name_map)?,
                        ConversionDirection::CppCallsCpp =>
                            ty.converted_type(&self.original_name_map)?,
                        ConversionDirection::CppCallsRust => ty
                            .inverse()
                            .ok_or(ConvertErrorFromCpp::NonInvertibleConversion)?
                            .unconverted_type(&self.original_name_map)?,
                    },
                    get_arg_name(counter)
                ))
            })
            .collect();
        let args = args?.join(", ");
        let default_return = match details.kind {
            CppFunctionKind::SynthesizedConstructor => "",
            _ => "void",
        };
        let ret_type = details
            .return_conversion
            .as_ref()
            .and_then(|x| match conversion_direction {
                ConversionDirection::RustCallsCpp => {
                    if x.populate_return_value() {
                        Some(x.converted_type(&self.original_name_map))
                    } else {
                        None
                    }
                }
                ConversionDirection::CppCallsCpp => {
                    Some(x.unconverted_type(&self.original_name_map))
                }
                ConversionDirection::CppCallsRust => Some(
                    x.inverse()
                        .ok_or(ConvertErrorFromCpp::NonInvertibleConversion)
                        .and_then(|x| x.converted_type(&self.original_name_map)),
                ),
            })
            .unwrap_or_else(|| Ok(default_return.to_string()))?;
        let constness = match details.kind {
            CppFunctionKind::ConstMethod => " const",
            _ => "",
        };
        // Only ever non-empty when we're overriding a ref-qualified virtual
        // method in a subclass, where the override has to repeat the
        // qualifier - google/autocxx#837.
        let ref_qualifier = match details.ref_qualifier {
            CppRefQualifier::None => "",
            CppRefQualifier::LValue => " &",
            CppRefQualifier::RValue => " &&",
        };
        // Only ever non-empty for a subclass peer's override of a superclass
        // virtual method - see `CppFunction::is_virtual_override`. It belongs
        // on the declaration alone; C++ rejects it on the out-of-line
        // definition below.
        let override_keyword = if details.is_virtual_override {
            " override"
        } else {
            ""
        };
        let declaration =
            format!("{ret_type} {name}({args}){constness}{ref_qualifier}{override_keyword}");
        let qualification = if let Some(qualification) = &details.qualification {
            format!("{}::", qualification.to_cpp_name())
        } else {
            "".to_string()
        };
        let qualified_declaration =
            format!("{ret_type} {qualification}{name}({args}){constness}{ref_qualifier}");
        // Whether there's a placement param in which to put the return value
        let placement_param = details
            .argument_conversion
            .iter()
            .enumerate()
            .filter_map(|(counter, conv)| {
                if conv.is_placement_parameter() {
                    Some(get_arg_name(counter))
                } else {
                    None
                }
            })
            .next();
        // Arguments to underlying function call
        let arg_list: Result<Vec<_>, _> = details
            .argument_conversion
            .iter()
            .enumerate()
            .map(|(counter, conv)| match conversion_direction {
                ConversionDirection::RustCallsCpp => {
                    conv.cpp_conversion(&get_arg_name(counter), &self.original_name_map, false)
                }
                ConversionDirection::CppCallsCpp => Ok(Some(get_arg_name(counter))),
                ConversionDirection::CppCallsRust => conv
                    .inverse()
                    .ok_or(ConvertErrorFromCpp::NonInvertibleConversion)
                    .and_then(|conv| {
                        conv.cpp_conversion(&get_arg_name(counter), &self.original_name_map, false)
                    }),
            })
            .collect();
        let mut arg_list = arg_list?.into_iter().flatten();
        let receiver = if is_a_method { arg_list.next() } else { None };
        if matches!(&details.payload, CppFunctionBody::ConstructSuperclass(_)) {
            arg_list.next();
        }
        let arg_list = if details.pass_obs_field {
            std::iter::once("*obs".to_string())
                .chain(arg_list)
                .join(",")
        } else {
            arg_list.join(", ")
        };
        // Whether we emit a global placement new, and therefore need <new> for
        // its declaration.
        let mut need_placement_new = false;
        let (mut underlying_function_call, field_assignments, need_allocators) = match &details
            .payload
        {
            CppFunctionBody::Cast => (arg_list, "".to_string(), false),
            CppFunctionBody::PlacementNew(ns, id) => {
                let ty_id = QualifiedName::new(ns, id.clone());
                let ty_id = self.namespaced_name(&ty_id);
                // `::new` (not plain `new`) so that we always get the global
                // placement operator new from <new>. A class-specific
                // `operator new` hides all the global forms, including the
                // placement one, so unqualified `new (ptr) T(...)` would either
                // fail to compile or call the wrong operator.
                // https://github.com/google/autocxx/issues/1342
                need_placement_new = true;
                (
                    format!("::new ({}) {}({})", receiver.unwrap(), ty_id, arg_list),
                    "".to_string(),
                    false,
                )
            }
            CppFunctionBody::Destructor(ns, id) => {
                let full_name = QualifiedName::new(ns, id.clone());
                let ty_id = self.original_name_map.get_final_item(&full_name);
                let is_a_nested_struct = self.original_name_map.get(&full_name).is_some();
                // This is all super duper fiddly.
                // All we want to do is call a destructor. Constraints:
                // * an unnamed struct, e.g. typedef struct { .. } A, does not
                //   have any way of fully qualifying its destructor name.
                //   We have to use a 'using' statement.
                // * we don't get enough information from bindgen to distinguish
                //   typedef struct { .. } A  // unnamed struct
                //   from
                //   struct A { .. }          // named struct
                // * we can only do 'using A::B::C' if 'B' is a namespace,
                //   as opposed to a type with an inner type.
                // * we can always do 'using C = A::B::C' but then SOME C++
                //   compilers complain that it's unused, iff it's a named struct.
                let destructor_call = format!("{arg_list}->{ty_id}::~{ty_id}()");
                let destructor_call = if ns.is_empty() {
                    destructor_call
                } else {
                    let path = self.original_name_map.map(&full_name);
                    if is_a_nested_struct {
                        format!("{{ using {ty_id} = {path}; {destructor_call}; {ty_id}* pointless; (void)pointless; }}")
                    } else {
                        format!("{{ using {path}; {destructor_call}; }}")
                    }
                };
                (destructor_call, "".to_string(), false)
            }
            CppFunctionBody::FunctionCall(ns, id) => match receiver {
                Some(receiver) => (
                    format!(
                        "{receiver}.{}({arg_list})",
                        id.to_string_for_cpp_generation()
                    ),
                    "".to_string(),
                    false,
                ),
                None => {
                    let underlying_function_call = ns
                        .into_iter()
                        .cloned()
                        .chain(std::iter::once(
                            id.to_string_for_cpp_generation().to_string(),
                        ))
                        .join("::");
                    (
                        format!("{underlying_function_call}({arg_list})"),
                        "".to_string(),
                        false,
                    )
                }
            },
            CppFunctionBody::BaseClassMethodCall(base, id, receiver_mutability) => {
                // On the receiver cast to the base, rather than on the
                // receiver itself. `self.foo(args)` would let anything the
                // receiver's own class declares under that name decide what is
                // called - a data member, a nested type, an enumerator, or a
                // member function template, which bindgen does not report at
                // all - and the answer would not be the member whose signature
                // this wrapper was built from. The cast moves the lookup into
                // the base, where it finds that member; it is not a qualified
                // call, `self.Base::foo(args)`, which would settle the name
                // and lose the virtual dispatch with it.
                let receiver = receiver.expect("a base class method call with no receiver");
                let constness = match receiver_mutability {
                    ReceiverMutability::Const => "const ",
                    ReceiverMutability::Mutable => "",
                };
                (
                    format!(
                        "static_cast<{constness}{base}&>({receiver}).{}({arg_list})",
                        id.to_string_for_cpp_generation()
                    ),
                    "".to_string(),
                    false,
                )
            }
            // The receiver has already been converted to whatever C++ needs to
            // read a member off it, which for the `const T&` an accessor takes
            // is `(*autocxx_gen_this)`.
            CppFunctionBody::FieldRead(field) => (
                format!("{}.{field}", receiver.expect("an accessor is a method")),
                "".to_string(),
                false,
            ),
            // The variable itself. The return conversion wraps this in
            // whatever gets it to Rust - for a non-POD type, construction of
            // the holder which refers to it.
            CppFunctionBody::VariableRead(name) => {
                (self.namespaced_name(name), "".to_string(), false)
            }
            CppFunctionBody::StaticMethodCall(ns, ty_id, fn_id) => {
                // The bindgen name flattens nesting - a `struct B` inside
                // `struct A` is `A_B` - so joining the namespace to it would
                // emit `A_B::f()`, which names nothing in C++. Ask the name
                // map for the C++ spelling instead; it restores the nesting
                // and already carries the namespace.
                let ty_name = QualifiedName::new(ns, ty_id.clone());
                let underlying_function_call = format!(
                    "{}::{}",
                    self.namespaced_name(&ty_name),
                    fn_id.to_string_for_cpp_generation()
                );
                (
                    format!("{underlying_function_call}({arg_list})"),
                    "".to_string(),
                    false,
                )
            }
            CppFunctionBody::ConstructSuperclass(_) => ("".to_string(), arg_list, false),
            CppFunctionBody::AllocUninitialized(ty) => {
                let namespaced_ty = self.namespaced_name(ty);
                (
                    format!("new_appropriately<{namespaced_ty}>();",),
                    "".to_string(),
                    true,
                )
            }
            CppFunctionBody::FreeUninitialized(ty) => (
                format!("delete_appropriately<{}>(arg0);", self.namespaced_name(ty)),
                "".to_string(),
                true,
            ),
        };
        if let Some(ret) = &details.return_conversion {
            let call_itself = match conversion_direction {
                ConversionDirection::RustCallsCpp => {
                    ret.cpp_conversion(&underlying_function_call, &self.original_name_map, true)?
                }
                ConversionDirection::CppCallsCpp => Some(underlying_function_call),
                ConversionDirection::CppCallsRust => ret
                    .inverse()
                    .ok_or(ConvertErrorFromCpp::NonInvertibleConversion)?
                    .cpp_conversion(&underlying_function_call, &self.original_name_map, true)?,
            }
            .expect(
                "Expected some conversion type for return value which resulted in a parameter name",
            );

            underlying_function_call = match placement_param {
                Some(placement_param) => {
                    let tyname = self.original_name_map.type_to_cpp(&ret.cxxbridge_type())?;
                    // `::new` for the same reason as in `PlacementNew` above.
                    need_placement_new = true;
                    format!("::new({placement_param}) {tyname}({call_itself})")
                }
                None => format!("return {call_itself}"),
            };
        };
        if !underlying_function_call.is_empty() {
            underlying_function_call = format!("{underlying_function_call};");
        }
        let field_assignments =
            if let CppFunctionBody::ConstructSuperclass(superclass_name) = &details.payload {
                let superclass_assignments = if field_assignments.is_empty() {
                    "".to_string()
                } else {
                    format!("{superclass_name}({field_assignments}), ")
                };
                format!(": {superclass_assignments}obs(std::move(arg0))")
            } else {
                "".into()
            };
        let definition_after_sig = format!("{field_assignments} {{ {underlying_function_call} }}",);
        let (declaration, definition) = if requires_rust_declarations {
            (
                Some(format!("{declaration};")),
                Some(format!("{qualified_declaration} {definition_after_sig}")),
            )
        } else {
            (
                Some(format!("inline {declaration} {definition_after_sig}")),
                None,
            )
        };
        let (declaration, definition) = if details.calls_deprecated {
            (
                declaration.map(silence_deprecation),
                definition.map(silence_deprecation),
            )
        } else {
            (declaration, definition)
        };
        let mut headers = vec![Header::System("memory")];
        if need_placement_new {
            headers.push(Header::System("new"));
        }
        if need_allocators {
            // `cstddef` for `std::size_t` and `std::max_align_t`, `new` for
            // `std::align_val_t` and `type_traits` for the tag the prelude
            // dispatches an over-aligned type on.
            headers.push(Header::System("cstddef"));
            headers.push(Header::System("exception"));
            headers.push(Header::System("new"));
            headers.push(Header::System("type_traits"));
            headers.push(Header::NewDeletePrelude);
        }
        let needs_move_or_copy = details
            .argument_conversion
            .iter()
            .chain(details.return_conversion.iter())
            .any(|conv| match conversion_direction {
                ConversionDirection::RustCallsCpp => conv.may_use_move_or_copy_helper(),
                ConversionDirection::CppCallsCpp => false,
                // A conversion with no opposite has already been reported by
                // the argument and return handling above, both of which run
                // first and both of which give up on the whole function. Say
                // yes anyway rather than pick a direction here: this only
                // decides whether to emit a template definition, and an unused
                // one costs nothing where a missing one fails to compile.
                ConversionDirection::CppCallsRust => conv
                    .inverse()
                    .is_none_or(|conv| conv.may_use_move_or_copy_helper()),
            });
        if needs_move_or_copy {
            headers.push(Header::System("type_traits"));
            headers.push(Header::MoveOrCopyPrelude);
        }
        Ok(ExtraCpp {
            declaration,
            definition,
            headers,
            ..Default::default()
        })
    }

    fn namespaced_name(&self, name: &QualifiedName) -> String {
        self.original_name_map.map(name)
    }

    fn generate_ctype_typedef(&mut self, tn: &QualifiedName) {
        let cpp_name = tn.to_cpp_name();
        self.generate_typedef(tn, &cpp_name);
        if cpp_name == WCHAR_T {
            self.assert_wchar_t_width();
        }
    }

    /// Hold this compiler to the `wchar_t` width autocxx generated for.
    ///
    /// `autocxx::c_wchar_t` is a newtype over an integer chosen by `cfg` for the
    /// target, and a C++ compiler can be told to disagree with the target:
    /// `-fshort-wchar` makes `wchar_t` two bytes where the platform says four.
    /// cxx checks that this extern type is trivial but not that it is the size
    /// Rust thinks, so nothing else would notice - every value would be read
    /// from the wrong bytes. Emitted beside the typedef, so a header which
    /// never mentions `wchar_t` is not asked the question.
    fn assert_wchar_t_width(&mut self) {
        let size = self.wchar_t_size;
        self.additional_functions.push(ExtraCpp {
            type_definition: Some(format!(
                "static_assert(sizeof({WCHAR_T}) == {size}, \"autocxx generated these bindings \
                 for a target whose {WCHAR_T} is {size} bytes wide; this C++ compiler disagrees - \
                 check for -fshort-wchar\");"
            )),
            ..Default::default()
        })
    }

    /// The three C++ helpers which are the whole of what Rust can do with the
    /// opaque holder we lower a `std::shared_ptr<const T>` to.
    ///
    /// Written against the holder's own typedef rather than against `T`: the
    /// payload is spelt `H::element_type`, which is exactly the `const T` the
    /// specialization was made with, so nothing here has to re-derive a C++
    /// name for it. `element_type` is the one `std::shared_ptr` member which
    /// says what a `const` payload is without the qualifier being written
    /// again. See google/autocxx#799.
    fn generate_shared_ptr_shims(&mut self, tn: &QualifiedName) {
        let holder = tn.get_final_item();
        let declaration = SharedPtrShim::ALL
            .iter()
            .map(|shim| {
                let name = shim.cpp_name(tn);
                match shim {
                    // A `std::shared_ptr` may be empty, and `get` says so by
                    // answering null. That is why this returns a pointer and
                    // not the reference which would be nicer to hold: `*sp` on
                    // an empty one is undefined behaviour, and there would be
                    // no way for the caller to have checked first.
                    SharedPtrShim::Get => format!(
                        "inline {holder}::element_type* {name}(const {holder}& self) {{ return self.get(); }}"
                    ),
                    SharedPtrShim::Clone => format!(
                        "inline ::std::unique_ptr<{holder}> {name}(const {holder}& self) {{ return ::std::unique_ptr<{holder}>(new {holder}(self)); }}"
                    ),
                    SharedPtrShim::UseCount => format!(
                        "inline ::std::int64_t {name}(const {holder}& self) {{ return self.use_count(); }}"
                    ),
                }
            })
            .join("\n");
        self.additional_functions.push(ExtraCpp {
            declaration: Some(declaration),
            headers: vec![Header::System("memory"), Header::System("cstdint")],
            ..Default::default()
        })
    }

    /// The two C++ helpers which are the whole of what Rust can do with the
    /// opaque holder we lower a `std::unique_ptr<const T>` to.
    ///
    /// The payload is spelt `H::element_type` for the reason
    /// [`Self::generate_shared_ptr_shims`] gives. That is the pointee type and
    /// not `std::unique_ptr`'s `pointer`, which a custom deleter may define
    /// differently - but a `std::unique_ptr<const T, D>` never reaches here as
    /// itself in the first place: bindgen erases the deleter argument, so the
    /// typedef above already names the default-deleter specialization and C++
    /// refuses to bind it, exactly as it does for `std::vector`'s allocator.
    /// See google/autocxx#799.
    fn generate_unique_ptr_shims(&mut self, tn: &QualifiedName) {
        let holder = tn.get_final_item();
        let declaration = UniquePtrShim::ALL
            .iter()
            .map(|shim| {
                let name = shim.cpp_name(tn);
                match shim {
                    // A `std::unique_ptr` may hold nothing, so this returns a
                    // pointer rather than the reference which would be nicer
                    // to hold, for the reason `std::shared_ptr::get` does.
                    UniquePtrShim::Get => format!(
                        "inline {holder}::element_type* {name}(const {holder}& self) {{ return self.get(); }}"
                    ),
                    UniquePtrShim::PayloadIsNull => format!(
                        "inline bool {name}(const {holder}& self) {{ return !self; }}"
                    ),
                }
            })
            .join("\n");
        self.additional_functions.push(ExtraCpp {
            declaration: Some(declaration),
            headers: vec![Header::System("memory")],
            ..Default::default()
        })
    }

    /// The two C++ helpers which are the whole of what Rust can do with the
    /// opaque holder we lower a `std::weak_ptr<const T>` to.
    ///
    /// `lock` is the only way C++ reads a `std::weak_ptr`, and it answers with
    /// a `std::shared_ptr` - so this returns the holder for *that*
    /// specialization, `shared_holder`, which the conversion manufactured
    /// beside this one. Every typedef is written above every declaration in
    /// the generated header, so that name is in scope here whichever holder
    /// was made first. Where the payload has expired, `lock` answers with an
    /// empty `shared_ptr` and this hands back a holder of one, rather than
    /// nothing: it is the same object C++ would have got, and `get` on it is
    /// null. See google/autocxx#799.
    fn generate_weak_ptr_shims(&mut self, tn: &QualifiedName, shared_holder: &QualifiedName) {
        let holder = tn.get_final_item();
        let shared = shared_holder.get_final_item();
        let declaration = WeakPtrShim::ALL
            .iter()
            .map(|shim| {
                let name = shim.cpp_name(tn);
                match shim {
                    WeakPtrShim::Lock => format!(
                        "inline ::std::unique_ptr<{shared}> {name}(const {holder}& self) {{ return ::std::unique_ptr<{shared}>(new {shared}(self.lock())); }}"
                    ),
                    WeakPtrShim::UseCount => format!(
                        "inline ::std::int64_t {name}(const {holder}& self) {{ return self.use_count(); }}"
                    ),
                }
            })
            .join("\n");
        self.additional_functions.push(ExtraCpp {
            declaration: Some(declaration),
            headers: vec![Header::System("memory"), Header::System("cstdint")],
            ..Default::default()
        })
    }

    /// The two C++ helpers behind everything Rust can do with the opaque
    /// holder we lower a `std::vector<T*>` to.
    ///
    /// Written against the holder's own typedef rather than against `T*`: the
    /// element is spelt `H::value_type`, which is exactly the pointer type the
    /// specialization was made with, qualifiers and all, so nothing here has
    /// to re-derive a C++ name for it. `operator[]` on a `const` vector yields
    /// a `const` reference to the stored pointer, and returning `value_type`
    /// copies it out - so the caller holds an address rather than a borrow of
    /// the slot a later mutation would move. See google/autocxx#330.
    fn generate_vector_shims(&mut self, tn: &QualifiedName) {
        let holder = tn.get_final_item();
        let declaration = VectorShim::ALL
            .iter()
            .map(|shim| {
                let name = shim.cpp_name(tn);
                match shim {
                    VectorShim::Len => format!(
                        "inline ::std::size_t {name}(const {holder}& self) {{ return self.size(); }}"
                    ),
                    // No bounds check here. The Rust method this is reached
                    // through is `unsafe` and says the caller owes one, and
                    // the safe `get` above it does the checking - which it can
                    // do without a second call into C++ only because it is the
                    // one deciding to make this one.
                    VectorShim::GetUnchecked => format!(
                        "inline {holder}::value_type {name}(const {holder}& self, ::std::size_t pos) {{ return self[pos]; }}"
                    ),
                }
            })
            .join("\n");
        self.additional_functions.push(ExtraCpp {
            declaration: Some(declaration),
            headers: vec![Header::System("vector"), Header::System("cstddef")],
            ..Default::default()
        })
    }

    /// The one C++ helper which is the whole of what Rust can do with the
    /// opaque holder standing for a `const` reference to a C++ variable.
    ///
    /// Written against the holder's own typedef rather than against `T`: the
    /// referent is spelt `H::type`, which is exactly the `const T` the
    /// `std::reference_wrapper` was made with, so nothing here has to
    /// re-derive a C++ name for it. `get` answers a reference which is never
    /// null - that is what a `std::reference_wrapper` is - and its address is
    /// the referent's own, since the holder stores a pointer rather than a
    /// copy. `addressof` rather than `&`, because a class may overload
    /// `operator&` and one which does would hand back whatever it likes. See
    /// google/autocxx#94.
    fn generate_const_ref_shims(&mut self, tn: &QualifiedName) {
        let holder = tn.get_final_item();
        let declaration = ConstRefShim::ALL
            .iter()
            .map(|shim| {
                let name = shim.cpp_name(tn);
                match shim {
                    ConstRefShim::Get => format!(
                        "inline {holder}::type* {name}(const {holder}& self) {{ return ::std::addressof(self.get()); }}"
                    ),
                }
            })
            .join("\n");
        self.additional_functions.push(ExtraCpp {
            declaration: Some(declaration),
            headers: vec![Header::System("functional"), Header::System("memory")],
            ..Default::default()
        })
    }

    fn generate_typedef(&mut self, tn: &QualifiedName, definition: &str) {
        let our_name = tn.get_final_item();
        self.additional_functions.push(ExtraCpp {
            type_definition: Some(format!("typedef {definition} {our_name};")),
            ..Default::default()
        })
    }

    fn generate_subclass(
        &mut self,
        superclass: &QualifiedName,
        superclass_destructor_visibility: &Option<CppVisibility>,
        superclass_destructor_virtual: bool,
        subclass: &SubclassName,
        constructors: Vec<&CppFunction>,
        methods: Vec<SubclassFunction>,
    ) -> Result<(), ConvertErrorFromCpp> {
        let holder = subclass.holder();
        self.additional_functions.push(ExtraCpp {
            type_definition: Some(format!("struct {holder};")),
            ..Default::default()
        });
        let mut method_decls = Vec::new();
        for method in methods {
            // First the method which calls from C++ to Rust
            let mut fn_impl = self.generate_cpp_function_inner(
                method.fun,
                true,
                ConversionDirection::CppCallsRust,
                true,
                Some(&method.fun.original_cpp_name),
            )?;
            method_decls.push(fn_impl.declaration.take().unwrap());
            self.additional_functions.push(fn_impl);
            // And now the function to be called from Rust for default implementation (calls superclass in C++)
            if let Some(super_fn_name) = method.cpp_super_fn_name {
                let mut super_method = method.fun.clone();
                super_method.pass_obs_field = false;
                // `super_foo` is a new method on the subclass which we call
                // from Rust on an ordinary lvalue, so it doesn't inherit the
                // superclass method's ref-qualifier - nor its `override`, since
                // it overrides nothing and only shares a shape with the method
                // that does.
                super_method.ref_qualifier = CppRefQualifier::None;
                super_method.is_virtual_override = false;
                // Named during analysis, where the cxx bridge was given the
                // same name to call it by.
                super_method.wrapper_function_name = super_fn_name.clone();
                super_method.payload = CppFunctionBody::StaticMethodCall(
                    superclass.get_namespace().clone(),
                    superclass.get_final_ident(),
                    method.fun.original_cpp_name.clone(),
                );
                let mut super_fn_impl = self.generate_cpp_function_inner(
                    &super_method,
                    true,
                    ConversionDirection::CppCallsCpp,
                    false,
                    None,
                )?;
                method_decls.push(super_fn_impl.declaration.take().unwrap());
                self.additional_functions.push(super_fn_impl);
            }
        }
        // In future, for each superclass..
        let super_name = superclass.get_final_item();
        method_decls.push(format!(
            "const {super_name}& As_{super_name}() const {{ return *this; }}",
        ));
        method_decls.push(format!(
            "{super_name}& As_{super_name}_mut() {{ return *this; }}"
        ));
        // `std::unique_ptr<Superclass>` requires the superclass to be
        // destructible from wherever the deleter is instantiated, so only offer
        // this conversion when the superclass destructor is public. A protected
        // destructor is enough for the subclass itself, but not for this. It
        // also has to be virtual: the pointer handed over owns a subclass peer,
        // and `delete`ing that through a superclass pointer runs the wrong
        // destructor otherwise.
        if superclass_destructor_virtual
            && matches!(
                superclass_destructor_visibility,
                Some(CppVisibility::Public)
            )
        {
            self.additional_functions.push(ExtraCpp {
                declaration: Some(format!(
                    "inline std::unique_ptr<{}> {}_As_{}_UniquePtr(std::unique_ptr<{}> u) {{ return std::unique_ptr<{}>(u.release()); }}",
                    superclass.to_cpp_name(), subclass.cpp(), super_name, subclass.cpp(), superclass.to_cpp_name(),
                    )),
                    ..Default::default()
            });
        }
        // And now constructors
        let mut constructor_decls: Vec<String> = Vec::new();
        for constructor in constructors {
            let mut fn_impl = self.generate_cpp_function_inner(
                constructor,
                false,
                ConversionDirection::CppCallsCpp,
                false,
                None,
            )?;
            let decl = fn_impl.declaration.take().unwrap();
            constructor_decls.push(decl);
            self.additional_functions.push(fn_impl);
        }
        // A peer is deleted through a `std::unique_ptr<Peer>`. Where no
        // virtual destructor was found for the superclass, the peer's is not
        // virtual either, and deleting a non-final polymorphic class that way
        // is clang's -Wdelete-non-abstract-non-virtual-dtor - about a class
        // someone might derive from and then delete through this pointer.
        // `final` forecloses that, which makes the deletion valid rather than
        // suppressing a warning about it, in generated code a `-Werror` user
        // cannot scope off.
        //
        // It is a restriction, so it is imposed only where it buys that: with
        // a virtual superclass destructor there is nothing to warn about, and
        // a C++ class deriving from the peer - which nothing documents, but
        // nothing forbids either - keeps working.
        let final_kw = if superclass_destructor_virtual {
            ""
        } else {
            " final"
        };
        self.additional_functions.push(ExtraCpp {
            type_definition: Some(format!(
                "class {}{} : public {}\n{{\npublic:\n{}\n{}\nvoid {}() const;\nprivate:rust::Box<{}> obs;\nvoid really_remove_ownership();\n\n}};",
                subclass.cpp(),
                final_kw,
                superclass.to_cpp_name(),
                constructor_decls.join("\n"),
                method_decls.join("\n"),
                subclass.cpp_remove_ownership(),
                holder
            )),
            definition: Some(format!(
                "void {}::{}() const {{\nconst_cast<{}*>(this)->really_remove_ownership();\n}}\nvoid {}::really_remove_ownership() {{\nauto new_obs = {}(std::move(obs));\nobs = std::move(new_obs);\n}}\n",
                subclass.cpp(),
                subclass.cpp_remove_ownership(),
                subclass.cpp(),
                subclass.cpp(),
                subclass.remove_ownership()
            )),
            cpp_headers: vec![Header::CxxgenH],
            ..Default::default()
        });
        Ok(())
    }
}
