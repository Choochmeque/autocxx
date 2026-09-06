// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The `std::vector` glue for the `autocxx::c_*` newtypes.
//!
//! cxx implements [`cxx::vector::VectorElement`] only for the types it knows
//! natively, and the variable-width C integers are not among them, so a
//! `std::vector<int>` cannot cross a bridge without help. The help cxx offers
//! is the "explicit shim trait impl": an `impl CxxVector<T> {}` written inside
//! a `#[cxx::bridge]`, which makes cxx synthesize the trait impl and emit the
//! matching C++ template instantiation.
//!
//! It has to be *this* crate that writes it, because the orphan rule forbids
//! any downstream crate from implementing cxx's trait for our types - which is
//! exactly what stumped the reporter of google/autocxx#422. Doing it once here
//! serves every generated bridge.
#[cxx::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("c_type_vectors.h");

        type c_int = crate::c_int;
        type c_uint = crate::c_uint;
        type c_long = crate::c_long;
        type c_ulong = crate::c_ulong;
        type c_short = crate::c_short;
        type c_ushort = crate::c_ushort;
        type c_longlong = crate::c_longlong;
        type c_ulonglong = crate::c_ulonglong;
    }

    impl CxxVector<c_int> {}
    impl CxxVector<c_uint> {}
    impl CxxVector<c_long> {}
    impl CxxVector<c_ulong> {}
    impl CxxVector<c_short> {}
    impl CxxVector<c_ushort> {}
    impl CxxVector<c_longlong> {}
    impl CxxVector<c_ulonglong> {}
}
