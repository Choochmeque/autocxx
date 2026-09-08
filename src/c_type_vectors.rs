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
//!
//! The `c_u8`..`c_i64` half of the list is that same trick applied to the
//! fixed-width spellings: several of them name a C++ type one of the entries
//! above already names, and the shims are still distinct because cxx names
//! them after the Rust type. Only `UniquePtr` is reachable for those - a
//! `uint32_t` in any other position keeps its `u32` atom, which cxx supports
//! natively - and the other three are written anyway so that nothing which
//! passes `known_types`'s container checks can reach a missing impl.
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

        type c_u8 = crate::c_u8;
        type c_i8 = crate::c_i8;
        type c_u16 = crate::c_u16;
        type c_i16 = crate::c_i16;
        type c_u32 = crate::c_u32;
        type c_i32 = crate::c_i32;
        type c_u64 = crate::c_u64;
        type c_i64 = crate::c_i64;
    }

    impl CxxVector<c_int> {}
    impl CxxVector<c_uint> {}
    impl CxxVector<c_long> {}
    impl CxxVector<c_ulong> {}
    impl CxxVector<c_short> {}
    impl CxxVector<c_ushort> {}
    impl CxxVector<c_longlong> {}
    impl CxxVector<c_ulonglong> {}
    impl CxxVector<c_u8> {}
    impl CxxVector<c_i8> {}
    impl CxxVector<c_u16> {}
    impl CxxVector<c_i16> {}
    impl CxxVector<c_u32> {}
    impl CxxVector<c_i32> {}
    impl CxxVector<c_u64> {}
    impl CxxVector<c_i64> {}

    impl UniquePtr<c_int> {}
    impl UniquePtr<c_uint> {}
    impl UniquePtr<c_long> {}
    impl UniquePtr<c_ulong> {}
    impl UniquePtr<c_short> {}
    impl UniquePtr<c_ushort> {}
    impl UniquePtr<c_longlong> {}
    impl UniquePtr<c_ulonglong> {}
    impl UniquePtr<c_u8> {}
    impl UniquePtr<c_i8> {}
    impl UniquePtr<c_u16> {}
    impl UniquePtr<c_i16> {}
    impl UniquePtr<c_u32> {}
    impl UniquePtr<c_i32> {}
    impl UniquePtr<c_u64> {}
    impl UniquePtr<c_i64> {}

    impl SharedPtr<c_int> {}
    impl SharedPtr<c_uint> {}
    impl SharedPtr<c_long> {}
    impl SharedPtr<c_ulong> {}
    impl SharedPtr<c_short> {}
    impl SharedPtr<c_ushort> {}
    impl SharedPtr<c_longlong> {}
    impl SharedPtr<c_ulonglong> {}
    impl SharedPtr<c_u8> {}
    impl SharedPtr<c_i8> {}
    impl SharedPtr<c_u16> {}
    impl SharedPtr<c_i16> {}
    impl SharedPtr<c_u32> {}
    impl SharedPtr<c_i32> {}
    impl SharedPtr<c_u64> {}
    impl SharedPtr<c_i64> {}

    impl WeakPtr<c_int> {}
    impl WeakPtr<c_uint> {}
    impl WeakPtr<c_long> {}
    impl WeakPtr<c_ulong> {}
    impl WeakPtr<c_short> {}
    impl WeakPtr<c_ushort> {}
    impl WeakPtr<c_longlong> {}
    impl WeakPtr<c_ulonglong> {}
    impl WeakPtr<c_u8> {}
    impl WeakPtr<c_i8> {}
    impl WeakPtr<c_u16> {}
    impl WeakPtr<c_i16> {}
    impl WeakPtr<c_u32> {}
    impl WeakPtr<c_i32> {}
    impl WeakPtr<c_u64> {}
    impl WeakPtr<c_i64> {}
}
