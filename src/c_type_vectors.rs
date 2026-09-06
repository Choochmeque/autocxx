// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The `std::vector` and smart pointer glue for the `autocxx::c_*` newtypes.
//!
//! cxx implements [`cxx::vector::VectorElement`] and
//! [`cxx::memory::UniquePtrTarget`] only for the types it knows natively, and
//! the variable-width C integers are not among them, so a `std::vector<int>`
//! or a `std::unique_ptr<int>` cannot cross a bridge without help. The help
//! cxx offers is the "explicit shim trait impl": an `impl CxxVector<T> {}` -
//! or `UniquePtr`, `SharedPtr`, `WeakPtr` - written inside a `#[cxx::bridge]`,
//! which makes cxx synthesize the trait impl and emit the matching C++
//! template instantiation.
//!
//! It has to be *this* crate that writes it, because the orphan rule forbids
//! any downstream crate from implementing cxx's trait for our types - which is
//! exactly what stumped the reporter of google/autocxx#422. Doing it once here
//! serves every generated bridge.
//!
//! `int` gets in where `uint32_t` cannot: cxx's macro rejects a `unique_ptr`
//! of any of its own built-in atoms outright, and `u32` is one, whereas
//! `c_int` is just a named type to it.
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

    impl UniquePtr<c_int> {}
    impl UniquePtr<c_uint> {}
    impl UniquePtr<c_long> {}
    impl UniquePtr<c_ulong> {}
    impl UniquePtr<c_short> {}
    impl UniquePtr<c_ushort> {}
    impl UniquePtr<c_longlong> {}
    impl UniquePtr<c_ulonglong> {}

    impl SharedPtr<c_int> {}
    impl SharedPtr<c_uint> {}
    impl SharedPtr<c_long> {}
    impl SharedPtr<c_ulong> {}
    impl SharedPtr<c_short> {}
    impl SharedPtr<c_ushort> {}
    impl SharedPtr<c_longlong> {}
    impl SharedPtr<c_ulonglong> {}

    impl WeakPtr<c_int> {}
    impl WeakPtr<c_uint> {}
    impl WeakPtr<c_long> {}
    impl WeakPtr<c_ulong> {}
    impl WeakPtr<c_short> {}
    impl WeakPtr<c_ushort> {}
    impl WeakPtr<c_longlong> {}
    impl WeakPtr<c_ulonglong> {}
}
