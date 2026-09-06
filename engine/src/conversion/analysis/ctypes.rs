// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indexmap::map::IndexMap as HashMap;

use crate::minisyn::Ident;

use crate::conversion::api::ApiName;
use crate::conversion::apivec::ApiVec;
use crate::types::Namespace;
use crate::{conversion::api::Api, known_types::known_types, types::QualifiedName};

use super::deps::HasDependencies;
use super::fun::FnPhase;

/// Spot any of the C types cxx cannot spell - the variable-length integers,
/// `void` and `char16_t` - used in the [Api]s, and append those as extra APIs
/// so that the generated C++ declares a typedef for each.
pub(crate) fn append_ctype_information(apis: &mut ApiVec<FnPhase>) {
    let ctypes: HashMap<Ident, QualifiedName> = apis
        .iter()
        .flat_map(|api| api.deps())
        // A dependency may name one of these types by an alias - `char16_t`
        // reaches us as bindgen's `bindgen_cchar16_t` - whereas the generated
        // code always calls it by the canonical name, which is what this
        // returns. Emit the typedef under that name, or nothing declares the
        // type the bridge goes on to use.
        .filter_map(|ty| known_types().as_ctype(ty))
        .map(|ty| (ty.get_final_ident(), ty))
        .collect();
    for (id, typename) in ctypes {
        apis.push(Api::CType {
            name: ApiName::new(&Namespace::new(), id),
            typename,
        });
    }
}
