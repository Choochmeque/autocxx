// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indoc::indoc;

/// Builds the map a `const std::map<K, V>&` parameter asked for out of the two
/// lists which took its place in the bridge.
///
/// The map is returned by value and bound to the `const` reference at the call,
/// so it lives exactly as long as the full-expression the call is part of - the
/// same lifetime a temporary argument would have had.
///
/// `M` is written out by the caller, because it is the type C++ declared and
/// not one deducible from the lists: which of the two maps, and with which
/// comparator, is decided where the parameter was recognised.
///
/// The two lists are `std::vector`s or `rust::Vec`s - a `std::string` key or
/// value crosses as the latter, because cxx gives Rust no way to fill a
/// `std::vector<std::string>` - so the container types are deduced too, and the
/// only things asked of them are `size()` and `operator[]`, which both have.
/// The elements a `rust::Vec<rust::String>` yields are the one thing which is
/// not already the map's own key or value type, so a single overload converts
/// those and everything else passes through untouched; a non-template overload
/// wins over the template for an exact match, which is what picks it.
///
/// The two are paired up to the shorter of them, which is a total operation
/// rather than the right answer: the Rust binding requires them to be the same
/// length before the call, and this is what that requirement leaves to be
/// written. Indexing to `keys.size()` regardless would read off the end of the
/// values list for anything which reached here with the two unequal.
///
/// `emplace` rather than `operator[]`, so that a value type with no default
/// constructor is as welcome as any other, and so that the first of two equal
/// keys wins rather than the last. Which one wins is a choice either way; the
/// binding's documentation states this one.
///
/// The `rust::String` overload is declared whether or not this wrapper has a
/// list of them, so `cxx.h` is asked for alongside this whether or not it does.
pub(super) static MAP_PRELUDE: &str = indoc! {"
    template <typename T>
    const T& autocxx_map_element(const T& value) { return value; }
    inline std::string autocxx_map_element(const ::rust::String& value) {
      return std::string(value.data(), value.size());
    }
    template <typename M, typename K, typename V>
    M autocxx_map_from_vectors(const K& keys, const V& values) {
      M result;
      const auto count = keys.size() < values.size() ? keys.size() : values.size();
      for (decltype(keys.size()) i = 0; i < count; ++i) {
        result.emplace(autocxx_map_element(keys[i]), autocxx_map_element(values[i]));
      }
      return result;
    }
"};
