// Copyright 2021 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#pragma once

// This is a simulation of _something like_ the way the steam API works.

class IEngine {
public:
	virtual int ConnectToGlobalUser(int) = 0;
    virtual void DisconnectGlobalUser(int user_id) = 0;
    // The real Steam interfaces have no virtual destructor - you never own one
    // of these. cxx still emits UniquePtr and Vec drop glue for every opaque
    // type it generates, and that glue makes clang warn about deleting an
    // abstract class through a non-virtual destructor even though this example
    // never runs it. One is cheaper than teaching the reader to ignore it.
    virtual ~IEngine() {}
};

void* GetSteamEngine(); // return an IEngine*
