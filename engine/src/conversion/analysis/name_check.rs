// Copyright 2021 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::map::IndexMap as HashMap;

use crate::minisyn::Ident;

use crate::{
    conversion::{
        api::{Api, SubclassName},
        apivec::ApiVec,
        error_reporter::convert_item_apis,
        ConvertErrorFromCpp,
    },
    types::validate_ident_ok_for_cxx,
};

use super::bridge_type_names::{declares_bridge_type, fixed_bridge_names, BridgeTypeNames};
use super::fun::FnPhase;

/// Do some final checks that the names we've come up with can be represented
/// within cxx, and settle on the name each type will go by inside the bridge
/// mod, whose namespace is flat.
pub(crate) fn check_names(apis: ApiVec<FnPhase>) -> (ApiVec<FnPhase>, BridgeTypeNames) {
    // If any items have names which can't be represented by cxx,
    // abort. This check should ideally be done at the times we fill in the
    // `name` field of each `api` in the first place, at parse time, though
    // as the `name` field of each API may change during various analysis phases,
    // currently it seems better to do it here to ensure we respect
    // the output of any such changes.
    let mut intermediate = ApiVec::new();
    convert_item_apis(apis, &mut intermediate, |api| match api {
        Api::Typedef { ref name, .. }
        | Api::ForwardDeclaration { ref name, .. }
        | Api::OpaqueTypedef { ref name, .. }
        | Api::Const { ref name, .. }
        | Api::Static { ref name, .. }
        | Api::Enum { ref name, .. }
        | Api::Struct { ref name, .. } => {
            validate_all_segments_ok_for_cxx(name.name.segment_iter())?;
            if let Some(cpp_name) = name.cpp_name_if_present() {
                // The C++ name might itself be outer_type::inner_type and thus may
                // have multiple segments.
                validate_all_segments_ok_for_cxx(cpp_name.to_qualified_name().segment_iter())?;
            }
            Ok(Box::new(std::iter::once(api)))
        }
        Api::Subclass {
            name: SubclassName(ref name),
            ref superclass,
        } => {
            validate_all_segments_ok_for_cxx(name.name.segment_iter())?;
            validate_all_segments_ok_for_cxx(superclass.segment_iter())?;
            Ok(Box::new(std::iter::once(api)))
        }
        Api::Function { ref name, .. } => {
            // we don't handle function names here because
            // the function analysis does an equivalent check. Instead of just rejecting
            // the function, it creates a wrapper function instead with a more
            // palatable name. That's preferable to rejecting the API entirely.
            validate_all_segments_ok_for_cxx(name.name.segment_iter())?;
            Ok(Box::new(std::iter::once(api)))
        }
        Api::ConcreteType { .. }
        | Api::CType { .. }
        | Api::StringConstructor { .. }
        | Api::RustType { .. }
        | Api::RustSubclassFn { .. }
        | Api::RustFn { .. }
        | Api::SubclassTraitItem { .. }
        | Api::ExternCppType { .. }
        | Api::IgnoredItem { .. } => Ok(Box::new(std::iter::once(api))),
    });

    // Give each type its own name within the bridge mod, which has a flat
    // namespace - see google/autocxx#486.
    let (bridge_type_names, unnameable) = BridgeTypeNames::new(&intermediate);

    // Anything which still shares a bridge name with something else at this
    // point is beyond our renaming, and would make `cxx` fail with "the name
    // is defined multiple times". Say which items collided instead.
    let mut names_found: HashMap<Ident, Vec<String>> = HashMap::new();
    for api in intermediate.iter() {
        if let Some(name) = bridge_name(api, &bridge_type_names) {
            let e = names_found.entry(name).or_default();
            e.push(api.name_info().name.to_string());
        }
    }
    let mut results = ApiVec::new();
    convert_item_apis(intermediate, &mut results, |api| {
        if let Some(e) = unnameable.get(api.name()) {
            return Err(ConvertErrorFromCpp::InvalidIdent(e.clone()));
        }
        if let Some(name) = bridge_name(&api, &bridge_type_names) {
            let symbols_for_this_name = names_found.entry(name).or_default();
            if symbols_for_this_name.len() > 1usize {
                return Err(ConvertErrorFromCpp::DuplicateCxxBridgeName(
                    symbols_for_this_name.clone(),
                ));
            }
        }
        Ok(Box::new(std::iter::once(api)))
    });
    (results, bridge_type_names)
}

/// The name this API will go by inside the bridge mod, if it declares anything
/// there at all.
fn bridge_name(api: &Api<FnPhase>, bridge_type_names: &BridgeTypeNames) -> Option<Ident> {
    if declares_bridge_type(api) {
        return Some(bridge_type_names.get(api.name()));
    }
    fixed_bridge_names(api)
        .first()
        .map(crate::types::make_ident)
}

fn validate_all_segments_ok_for_cxx<'a>(
    items: impl Iterator<Item = &'a str>,
) -> Result<(), ConvertErrorFromCpp> {
    for seg in items {
        validate_ident_ok_for_cxx(seg).map_err(ConvertErrorFromCpp::InvalidIdent)?;
    }
    Ok(())
}
