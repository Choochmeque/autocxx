// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// The C++ half of `autocxx::CxxMap<K, V>`: the table of supported key/value
// pairs, and a name for each `std::map` instantiation the table covers.
//
// `cxx::type_id!` spells a C++ type as `::`-separated identifiers and nothing
// else, so `std::map<int, int>` has no spelling it accepts. Each pair
// therefore gets a typedef whose name is a path of identifiers -
// `autocxx::map::c_int::c_int` - which is what the `ExternType` impl in
// `cxx_map.rs` names and what a generated `cxx::bridge` writes. The typedef is
// an alias, so a bridge which declares a parameter of that name is declaring
// one of the `std::map` itself.

#pragma once

#include <cstddef>
#include <cstdint>
#include <map>
#include <string>

// One expansion of `X` per supported value type, for the key type `KN`/`K`.
// `KN` and `VN` are the names Rust knows the two types by; `K` and `V` are the
// C++ types themselves.
#define AUTOCXX_MAP_FOR_EACH_VALUE(X, KN, K)                                   \
  X(KN, c_int, K, int)                                                         \
  X(KN, c_uint, K, unsigned int)                                               \
  X(KN, c_long, K, long)                                                       \
  X(KN, c_ulong, K, unsigned long)                                             \
  X(KN, c_short, K, short)                                                     \
  X(KN, c_ushort, K, unsigned short)                                           \
  X(KN, c_longlong, K, long long)                                              \
  X(KN, c_ulonglong, K, unsigned long long)                                    \
  X(KN, u8, K, std::uint8_t)                                                   \
  X(KN, i8, K, std::int8_t)                                                    \
  X(KN, u16, K, std::uint16_t)                                                 \
  X(KN, i16, K, std::int16_t)                                                  \
  X(KN, u32, K, std::uint32_t)                                                 \
  X(KN, i32, K, std::int32_t)                                                  \
  X(KN, u64, K, std::uint64_t)                                                 \
  X(KN, i64, K, std::int64_t)                                                  \
  X(KN, usize, K, std::size_t)                                                 \
  X(KN, f32, K, float)                                                         \
  X(KN, f64, K, double)                                                        \
  X(KN, c_char16_t, K, char16_t)                                               \
  X(KN, c_char32_t, K, char32_t)                                               \
  X(KN, c_wchar_t, K, wchar_t)                                                 \
  X(KN, string, K, std::string)

// One expansion of `X(KN, VN, K, V)` per supported key/value pair. The value
// list is every atom either side of this bridge can already put in a
// `std::vector`, less the ones whose C++ type another entry here already
// names (`c_u8` through `c_i64`, which exist only to get a fixed-width
// integer inside a `unique_ptr`) and `isize`, which is a `cxx` type rather
// than a C++ one. `bool` is absent because there is no `CxxVector<bool>` to
// return from `keys`/`values`, and `std::vector<bool>` would not be one. The
// key list below is the value list less `f32` and `f64`, because NaN would
// break the strict weak ordering `std::less` owes a key.
//
// Keep the two lists in step with `for_each_map_pair!` in `cxx_map.rs`: a pair
// Rust knows and this file does not is a link error, which `every_pair_links`
// provokes on purpose.
#define AUTOCXX_MAP_FOR_EACH_PAIR(X)                                           \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_int, int)                                    \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_uint, unsigned int)                          \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_long, long)                                  \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_ulong, unsigned long)                        \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_short, short)                                \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_ushort, unsigned short)                      \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_longlong, long long)                         \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_ulonglong, unsigned long long)               \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, u8, std::uint8_t)                              \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, i8, std::int8_t)                               \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, u16, std::uint16_t)                            \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, i16, std::int16_t)                             \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, u32, std::uint32_t)                            \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, i32, std::int32_t)                             \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, u64, std::uint64_t)                            \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, i64, std::int64_t)                             \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, usize, std::size_t)                            \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_char16_t, char16_t)                          \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_char32_t, char32_t)                          \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, c_wchar_t, wchar_t)                            \
  AUTOCXX_MAP_FOR_EACH_VALUE(X, string, std::string)

// A namespace per key type holding a typedef per value type, so that the pair
// is reachable as `autocxx::map::<key>::<value>`. Naming a namespace member
// after its enclosing namespace is allowed, which is why the diagonal entries
// - `autocxx::map::c_int::c_int` - are legal.
#define AUTOCXX_MAP_TYPEDEF(KN, VN, K, V)                                      \
  namespace KN {                                                               \
  typedef std::map<K, V> VN;                                                   \
  }

namespace autocxx {
namespace map {
AUTOCXX_MAP_FOR_EACH_PAIR(AUTOCXX_MAP_TYPEDEF)
} // namespace map
} // namespace autocxx
