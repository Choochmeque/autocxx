// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#pragma once

#include <cstdint>
#include <map>
#include <string>

// A settings store of the shape a real C++ library tends to have: one map
// of named settings, one of numbered limits, and functions which hand them
// over, take them back and read through them.
class Config {
public:
  Config() {
    settings_.emplace("theme", "dark");
    settings_.emplace("locale", "en");
    limits_.emplace(1, 4096);
    limits_.emplace(2, 64);
  }

  // Returned by value: Rust owns the copy.
  std::map<std::string, std::string> settings() const { return settings_; }

  // Returned by reference: Rust reads the store's own.
  const std::map<uint32_t, uint32_t>& limits() const { return limits_; }

  // Taken by const reference, and written through by mutable reference.
  void merge(const std::map<std::string, std::string>& overrides) {
    for (const auto& entry : overrides) {
      settings_[entry.first] = entry.second;
    }
  }
  void raise(std::map<uint32_t, uint32_t>& limits) const {
    for (auto& entry : limits) {
      entry.second *= 2;
    }
  }

  uint32_t setting_count() const {
    return static_cast<uint32_t>(settings_.size());
  }

private:
  std::map<std::string, std::string> settings_;
  std::map<uint32_t, uint32_t> limits_;
};
