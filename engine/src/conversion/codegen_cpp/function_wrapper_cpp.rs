// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use syn::{Type, TypePtr, TypeReference};

use crate::conversion::{
    analysis::fun::function_wrapper::{CppConversionType, TypeConversionPolicy},
    ConvertErrorFromCpp,
};

use super::type_to_cpp::CppNameMap;

impl TypeConversionPolicy {
    pub(super) fn unconverted_type(
        &self,
        cpp_name_map: &CppNameMap,
    ) -> Result<String, ConvertErrorFromCpp> {
        match self.cpp_conversion {
            CppConversionType::FromUniquePtrToValue => self.unique_ptr_wrapped_type(cpp_name_map),
            CppConversionType::FromPtrToValue => {
                Ok(format!("{}*", self.unwrapped_type_as_string(cpp_name_map)?))
            }
            // `&var`. What this conversion is handed is the C++ reference;
            // the pointer in `cxxbridge_type` is what it produces.
            CppConversionType::FromReferenceToPointer => self.reference_type(cpp_name_map),
            _ => self.unwrapped_type_as_string(cpp_name_map),
        }
    }

    pub(super) fn converted_type(
        &self,
        cpp_name_map: &CppNameMap,
    ) -> Result<String, ConvertErrorFromCpp> {
        match self.cpp_conversion {
            CppConversionType::FromValueToUniquePtr => self.unique_ptr_wrapped_type(cpp_name_map),
            CppConversionType::FromReferenceToPointer => {
                let (const_string, ty) = match self.cxxbridge_type() {
                    Type::Ptr(TypePtr {
                        mutability: Some(_),
                        elem,
                        ..
                    }) => ("", elem.as_ref()),
                    Type::Ptr(TypePtr { elem, .. }) => ("const ", elem.as_ref()),
                    _ => panic!("Not a pointer"),
                };
                Ok(format!(
                    "{}{}*",
                    const_string,
                    cpp_name_map.type_to_cpp(ty)?
                ))
            }
            // `(*var)`. What this conversion produces is the C++ reference the
            // underlying function asked for, not the pointer it was handed.
            CppConversionType::FromPointerToReference => self.reference_type(cpp_name_map),
            _ => self.unwrapped_type_as_string(cpp_name_map),
        }
    }

    /// The C++ reference which the pointer in [`Self::cxxbridge_type`] stands
    /// for, for the two conversions which turn one into the other. Spelled by
    /// asking for the reference type itself, so that the referents which get a
    /// name of their own - `str`, which is `rust::Str` - keep it here too.
    fn reference_type(&self, cpp_name_map: &CppNameMap) -> Result<String, ConvertErrorFromCpp> {
        let ty = match self.cxxbridge_type() {
            Type::Ptr(TypePtr {
                mutability, elem, ..
            }) => Type::Reference(TypeReference {
                and_token: Default::default(),
                lifetime: None,
                mutability: *mutability,
                elem: elem.clone(),
            }),
            _ => panic!("Not a pointer"),
        };
        cpp_name_map.type_to_cpp(&ty)
    }

    fn unwrapped_type_as_string(
        &self,
        cpp_name_map: &CppNameMap,
    ) -> Result<String, ConvertErrorFromCpp> {
        cpp_name_map.type_to_cpp(self.cxxbridge_type())
    }

    fn unique_ptr_wrapped_type(
        &self,
        original_name_map: &CppNameMap,
    ) -> Result<String, ConvertErrorFromCpp> {
        Ok(format!(
            "std::unique_ptr<{}>",
            self.unwrapped_type_as_string(original_name_map)?
        ))
    }

    pub(super) fn cpp_conversion(
        &self,
        var_name: &str,
        cpp_name_map: &CppNameMap,
        is_return: bool,
    ) -> Result<Option<String>, ConvertErrorFromCpp> {
        // If is_return we want to avoid unnecessary std::moves because they
        // make RVO less effective
        Ok(match self.cpp_conversion {
            CppConversionType::None | CppConversionType::FromReturnValueToPlacementPtr => {
                Some(var_name.to_string())
            }
            CppConversionType::FromPointerToReference => Some(format!("(*{var_name})")),
            CppConversionType::Move => Some(format!("std::move({var_name})")),
            // A move constructor has to have a move constructor to call, so
            // `std::move` says exactly what is meant here.
            CppConversionType::FromPtrToMove => Some(format!("std::move(*{var_name})")),
            // Whereas these two are handing an ordinary parameter over by
            // value out of storage the Rust side owns and is about to destroy,
            // so a move is merely an optimization and must give way to a copy
            // for types whose move constructor is deleted. Name the helper
            // from the global namespace, or the argument's own namespaces
            // could offer a better-matching function of that name.
            CppConversionType::FromUniquePtrToValue => {
                Some(format!("::autocxx_move_or_copy(*{var_name})"))
            }
            // And this one is the wrapper's own by-value parameter, which is
            // about to be destroyed too, so the same reasoning applies. Passing
            // it bare would ask for a copy constructor, which a POD type that
            // opts into relocatability by writing its own move constructor no
            // longer has. See google/autocxx#1252.
            CppConversionType::MoveOrCopy => Some(format!("::autocxx_move_or_copy({var_name})")),
            CppConversionType::FromValueToUniquePtr => Some(format!(
                "std::make_unique<{}>({})",
                self.unconverted_type(cpp_name_map)?,
                var_name
            )),
            CppConversionType::FromPtrToValue => {
                let dereference = format!("*{var_name}");
                Some(if is_return {
                    dereference
                } else {
                    format!("::autocxx_move_or_copy({dereference})")
                })
            }
            CppConversionType::IgnoredPlacementPtrParameter => None,
            CppConversionType::FromReferenceToPointer => Some(format!("&{var_name}")),
        })
    }

    /// Whether [`Self::cpp_conversion`] may emit a call to the
    /// `autocxx_move_or_copy` helper, so that callers know to emit its
    /// definition. Keep in step with that function; over-reporting only costs
    /// an unused template definition, under-reporting fails to compile.
    pub(super) fn may_use_move_or_copy_helper(&self) -> bool {
        matches!(
            self.cpp_conversion,
            CppConversionType::FromUniquePtrToValue
                | CppConversionType::FromPtrToValue
                | CppConversionType::MoveOrCopy
        )
    }
}
