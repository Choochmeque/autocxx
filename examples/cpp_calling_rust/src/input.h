// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#pragma once

#include <cstdint>
#include <sstream>
#include <stdint.h>
#include <stdexcept>
#include <string>

void jurassic();

// Brings the park's perimeter fences up, and objects if the generators
// aren't running. Rust hears the objection as an Err, not as an unwind.
void raise_fences(bool generators_running);