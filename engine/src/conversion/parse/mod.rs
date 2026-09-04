// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

mod extern_fun_signatures;
mod linkage;
mod parse_bindgen;
mod parse_foreign_mod;
mod ref_qualifier;

pub(crate) use parse_bindgen::ParseBindgen;
pub(crate) use ref_qualifier::CppRefQualifier;
