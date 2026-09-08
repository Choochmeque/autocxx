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

#include <cstdint>

typedef int c_int;
typedef unsigned int c_uint;
typedef long c_long;
typedef unsigned long c_ulong;
typedef short c_short;
typedef unsigned short c_ushort;
typedef long long c_longlong;
typedef unsigned long long c_ulonglong;

// The fixed-width integers. Several of these name the same C++ type as one
// above - `c_u32` and `c_uint` are both `unsigned int` where an int is 32 bits
// - which is the point: cxx has an atom for the fixed-width spelling and none
// for the variable-length one, and only the latter may be a `unique_ptr`
// payload. Qualified because only `<stdint.h>` is required to put these in the
// global namespace.
typedef std::uint8_t c_u8;
typedef std::int8_t c_i8;
typedef std::uint16_t c_u16;
typedef std::int16_t c_i16;
typedef std::uint32_t c_u32;
typedef std::int32_t c_i32;
typedef std::uint64_t c_u64;
typedef std::int64_t c_i64;
