// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// The `std::map` glue `autocxx::CxxMap<K, V>` calls, one set of functions per
// pair in the table in `cxx_map.h`.
//
// Every function here is `noexcept`, including the five which allocate: `new`,
// `insert`, `insert_or_assign`, `keys` and `values`. A C++ exception unwinding
// into Rust through an `extern "C"` frame is undefined behavior, not a
// guaranteed abort, so `noexcept` turns an escaping exception into
// `std::terminate` before it gets there; `cxx` makes the same choice for
// `std::vector::push_back`.
//
// The `$` in the symbol names is the same identifier extension `cxx` relies on
// for `cxxbridge1$...`, and `_to_` separates the two type names because a bare
// `$` between two `##` pastes is not a preprocessing token every compiler
// accepts. No supported type name contains `_to_`, so the joined name
// identifies the pair.

#include "cxx_map.h"

#include <memory>
#include <new>
#include <utility>
#include <vector>

// `autocxx::c_wchar_t` is the Rust integer of the width the *target* says
// `wchar_t` has, and `-fshort-wchar` makes a compiler disagree with the target.
// A `std::map<wchar_t, V>` built under that flag would be keyed by two bytes
// where Rust passes four. `build.rs` passes the width Rust chose.
static_assert(sizeof(wchar_t) == AUTOCXX_WCHAR_T_SIZE,
              "autocxx was built for a target whose wchar_t is a different "
              "width from the one this C++ compiler is using - check for "
              "-fshort-wchar in CXXFLAGS");

#define AUTOCXX_MAP_GLUE(KN, VN, K, V)                                         \
  std::map<K, V> *autocxx$std$map$##KN##_to_##VN##$new() noexcept {            \
    return new std::map<K, V>();                                               \
  }                                                                            \
  std::size_t autocxx$std$map$##KN##_to_##VN##$size(                           \
      const std::map<K, V> &m) noexcept {                                      \
    return m.size();                                                           \
  }                                                                            \
  const V *autocxx$std$map$##KN##_to_##VN##$find(const std::map<K, V> &m,      \
                                                 const K &key) noexcept {      \
    auto found = m.find(key);                                                  \
    return found == m.end() ? nullptr : &found->second;                        \
  }                                                                            \
  bool autocxx$std$map$##KN##_to_##VN##$insert(                                \
      std::map<K, V> &m, const K &key, const V &value) noexcept {              \
    return m.insert(std::pair<const K, V>(key, value)).second;                 \
  }                                                                            \
  bool autocxx$std$map$##KN##_to_##VN##$insert_or_assign(                      \
      std::map<K, V> &m, const K &key, const V &value) noexcept {              \
    auto at = m.lower_bound(key);                                              \
    if (at != m.end() && !m.key_comp()(key, at->first)) {                      \
      at->second = value;                                                      \
      return false;                                                            \
    }                                                                          \
    m.insert(at, std::pair<const K, V>(key, value));                           \
    return true;                                                               \
  }                                                                            \
  bool autocxx$std$map$##KN##_to_##VN##$erase(std::map<K, V> &m,               \
                                              const K &key) noexcept {         \
    return m.erase(key) != 0;                                                  \
  }                                                                            \
  std::vector<K> *autocxx$std$map$##KN##_to_##VN##$keys(                       \
      const std::map<K, V> &m) noexcept {                                      \
    std::vector<K> *out = new std::vector<K>();                                \
    out->reserve(m.size());                                                    \
    for (auto entry = m.begin(); entry != m.end(); ++entry) {                  \
      out->push_back(entry->first);                                            \
    }                                                                          \
    return out;                                                                \
  }                                                                            \
  std::vector<V> *autocxx$std$map$##KN##_to_##VN##$values(                     \
      const std::map<K, V> &m) noexcept {                                      \
    std::vector<V> *out = new std::vector<V>();                                \
    out->reserve(m.size());                                                    \
    for (auto entry = m.begin(); entry != m.end(); ++entry) {                  \
      out->push_back(entry->second);                                           \
    }                                                                          \
    return out;                                                                \
  }                                                                            \
  void autocxx$unique_ptr$std$map$##KN##_to_##VN##$null(                       \
      std::unique_ptr<std::map<K, V>> *ptr) noexcept {                         \
    new (ptr) std::unique_ptr<std::map<K, V>>();                               \
  }                                                                            \
  void autocxx$unique_ptr$std$map$##KN##_to_##VN##$raw(                        \
      std::unique_ptr<std::map<K, V>> *ptr, std::map<K, V> *raw) noexcept {    \
    new (ptr) std::unique_ptr<std::map<K, V>>(raw);                            \
  }                                                                            \
  const std::map<K, V>                                                         \
      *autocxx$unique_ptr$std$map$##KN##_to_##VN##$get(                        \
          const std::unique_ptr<std::map<K, V>> &ptr) noexcept {               \
    return ptr.get();                                                          \
  }                                                                            \
  std::map<K, V> *autocxx$unique_ptr$std$map$##KN##_to_##VN##$release(         \
      std::unique_ptr<std::map<K, V>> &ptr) noexcept {                         \
    return ptr.release();                                                      \
  }                                                                            \
  void autocxx$unique_ptr$std$map$##KN##_to_##VN##$drop(                       \
      std::unique_ptr<std::map<K, V>> *ptr) noexcept {                         \
    ptr->~unique_ptr();                                                        \
  }

extern "C" {
AUTOCXX_MAP_FOR_EACH_PAIR(AUTOCXX_MAP_GLUE)
} // extern "C"
