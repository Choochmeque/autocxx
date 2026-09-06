// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// The C++ spellings of the `autocxx::c_*` newtypes, under the names their
// `cxx::type_id!`s give them. autocxx's generated headers emit the same
// typedefs; these exist so that this crate can compile the `std::vector`
// glue for them once, on everybody's behalf. See google/autocxx#422.

#pragma once

typedef int c_int;
typedef unsigned int c_uint;
typedef long c_long;
typedef unsigned long c_ulong;
typedef short c_short;
typedef unsigned short c_ushort;
typedef long long c_longlong;
typedef unsigned long long c_ulonglong;
