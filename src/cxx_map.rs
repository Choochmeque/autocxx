// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `std::map<K, V>` as an opaque Rust type.
//!
//! `cxx` has `CxxVector<T>` and `CxxString` and nothing for an associative
//! container, so a `std::map` has had no way across. [`CxxMap`] is the same
//! shape of answer as `CxxVector`: a zero-sized opaque type which only ever
//! exists behind a `UniquePtr` or a reference, a [`MapPair`] trait carrying
//! one set of `extern "C"` glue per key/value pair, and a table of the pairs
//! that glue is compiled for.
//!
//! Where `cxx` names its glue per element type, this names it per *pair* -
//! `std::map<int, std::string>` and `std::map<int, int>` are different C++
//! types with different code behind them - so the table is a cross product and
//! the C++ it compiles is what makes this an opt-in feature rather than part
//! of `c-type-vectors`.

use core::ffi::c_void;
use core::fmt::{self, Formatter};
use core::marker::{PhantomData, PhantomPinned};
use core::mem::MaybeUninit;
use core::pin::Pin;
use cxx::memory::UniquePtrTarget;
use cxx::vector::VectorElement;
use cxx::{CxxVector, UniquePtr};

/// Binding to C++ `std::map<K, V, std::less<K>, std::allocator<...>>`.
///
/// Like [`cxx::CxxVector`], this never exists by value in Rust: it is reached
/// through a `UniquePtr<CxxMap<K, V>>`, a `&CxxMap<K, V>` or a
/// `Pin<&mut CxxMap<K, V>>`. Nothing here claims a size or a layout for
/// `std::map`, which differs between standard libraries.
///
/// `K` and `V` are limited to the pairs [`MapPair`] is implemented for: the
/// integer and character atoms plus [`cxx::CxxString`] on either side, and
/// `f32`/`f64` as values only. See that trait for the list and for why it is
/// closed.
///
/// # Ordering
///
/// The map is ordered by `std::less<K>`, which for every supported `K` is the
/// C++ built-in `<`: numeric order for the numbers, byte order for a
/// `std::string`. [`keys`] and [`values`] hand back snapshots in that order,
/// and `values` is ordered by *key*, so the two line up entry by entry.
///
/// [`keys`]: CxxMap::keys
/// [`values`]: CxxMap::values
///
/// # Exceptions
///
/// The glue behind these methods is `noexcept`: a C++ exception inside it -
/// in practice an allocation failure - terminates the process rather than
/// unwinding into Rust, as in `cxx`'s own container glue.
///
/// # Example
///
/// ```
/// use autocxx::{c_int, CxxMap};
///
/// let mut map = CxxMap::<c_int, c_int>::new();
/// assert!(map.is_empty());
///
/// assert!(map.pin_mut().insert(&c_int(1), &c_int(10)));
/// assert!(map.pin_mut().insert(&c_int(2), &c_int(20)));
///
/// // `insert` is `std::map::insert`: the first value for a key wins.
/// assert!(!map.pin_mut().insert(&c_int(1), &c_int(99)));
/// assert_eq!(map.get(&c_int(1)), Some(&c_int(10)));
///
/// // `insert_or_assign` is the one which overwrites.
/// assert!(!map.pin_mut().insert_or_assign(&c_int(1), &c_int(99)));
/// assert_eq!(map.get(&c_int(1)), Some(&c_int(99)));
///
/// assert!(map.pin_mut().erase(&c_int(2)));
/// assert_eq!(map.len(), 1);
/// assert!(!map.contains(&c_int(2)));
/// ```
#[repr(C, packed)]
pub struct CxxMap<K, V> {
    // A field, because a `repr(C)` struct may not be all `PhantomData`.
    _void: [c_void; 0],
    // The entries the C++ map holds, so that auto traits follow `K` and `V`.
    _entries: PhantomData<[(K, V)]>,
    // No `Pin<&mut CxxMap<..>>` may be unpinned back to `&mut CxxMap<..>`.
    _pinned: PhantomData<PhantomPinned>,
}

// The type stands for a C++ object elsewhere; nothing of it is stored here.
const _: () = {
    assert!(core::mem::size_of::<CxxMap<crate::c_int, crate::c_int>>() == 0);
    assert!(core::mem::align_of::<CxxMap<crate::c_int, crate::c_int>>() == 1);
};

impl<K, V> CxxMap<K, V>
where
    K: MapPair<V>,
    V: VectorElement,
{
    /// Constructs a new heap allocated map, wrapped by `UniquePtr`.
    ///
    /// The C++ map is default constructed, and so empty.
    pub fn new() -> UniquePtr<Self> {
        // SAFETY: `__map_new` hands back sole ownership of a map it allocated
        // with `new`, which is what `std::default_delete` will free.
        unsafe { UniquePtr::from_raw(K::__map_new()) }
    }

    /// Returns the number of entries in the map.
    ///
    /// Matches the behavior of C++ [std::map\<K, V\>::size][size].
    ///
    /// [size]: https://en.cppreference.com/w/cpp/container/map/size
    pub fn len(&self) -> usize {
        K::__map_size(self)
    }

    /// Returns true if the map contains no entries.
    ///
    /// Matches the behavior of C++ [std::map\<K, V\>::empty][empty].
    ///
    /// [empty]: https://en.cppreference.com/w/cpp/container/map/empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns true if the map holds an entry for `key`.
    pub fn contains(&self, key: &K) -> bool {
        !K::__map_find(self, key).is_null()
    }

    /// Returns a reference to the value stored for `key`, or `None`.
    ///
    /// The value belongs to the map, so the reference borrows the map for as
    /// long as it lives; adding or removing entries meanwhile needs
    /// `Pin<&mut Self>`, which the borrow rules out.
    pub fn get(&self, key: &K) -> Option<&V> {
        let found = K::__map_find(self, key);
        // SAFETY: `__map_find` returns either null or a pointer to the value
        // inside an entry of this map, which outlives the `&self` borrow the
        // return type ties the reference to. `std::map` does not invalidate a
        // reference to an entry it keeps.
        unsafe { found.as_ref() }
    }

    /// Inserts a copy of `value` under a copy of `key`, and reports whether
    /// anything was inserted.
    ///
    /// Matches the behavior of C++ [std::map\<K, V\>::insert][insert]: if the
    /// map already holds an entry for `key`, that entry is left alone and this
    /// returns false. [`insert_or_assign`] is the one which overwrites.
    ///
    /// [insert]: https://en.cppreference.com/w/cpp/container/map/insert
    /// [`insert_or_assign`]: CxxMap::insert_or_assign
    pub fn insert(self: Pin<&mut Self>, key: &K, value: &V) -> bool {
        K::__map_insert(self, key, value)
    }

    /// Stores a copy of `value` under `key`, replacing any value already
    /// there, and reports whether the key was new.
    ///
    /// Matches the behavior of C++17
    /// [std::map\<K, V\>::insert_or_assign][insert_or_assign], whose
    /// `pair::second` this returns.
    ///
    /// [insert_or_assign]: https://en.cppreference.com/w/cpp/container/map/insert_or_assign
    pub fn insert_or_assign(self: Pin<&mut Self>, key: &K, value: &V) -> bool {
        K::__map_insert_or_assign(self, key, value)
    }

    /// Removes the entry for `key`, and reports whether there was one.
    ///
    /// Matches the behavior of C++ [std::map\<K, V\>::erase][erase] taking a
    /// key, whose count of erased entries this reduces to a bool - a
    /// `std::map` holds at most one entry per key.
    ///
    /// [erase]: https://en.cppreference.com/w/cpp/container/map/erase
    pub fn erase(self: Pin<&mut Self>, key: &K) -> bool {
        K::__map_erase(self, key)
    }

    /// Returns a copy of the keys, in the map's order.
    ///
    /// This is a snapshot: it is built when called and does not follow later
    /// changes to the map.
    pub fn keys(&self) -> UniquePtr<CxxVector<K>> {
        // SAFETY: `__map_keys` hands back sole ownership of a vector it
        // allocated with `new`, which is what `std::default_delete` will free.
        unsafe { UniquePtr::from_raw(K::__map_keys(self)) }
    }

    /// Returns a copy of the values, ordered by their keys.
    ///
    /// This is a snapshot: it is built when called and does not follow later
    /// changes to the map. The order matches [`keys`], entry for entry.
    ///
    /// [`keys`]: CxxMap::keys
    pub fn values(&self) -> UniquePtr<CxxVector<V>> {
        // SAFETY: `__map_values` hands back sole ownership of a vector it
        // allocated with `new`, which is what `std::default_delete` will free.
        unsafe { UniquePtr::from_raw(K::__map_values(self)) }
    }
}

// SAFETY: every method forwards to the glue compiled for this pair, which
// works on `std::unique_ptr<std::map<K, V>>` - the same representation
// `UniquePtr` stores, one pointer wide - exactly as `cxx`'s own impl for
// `CxxVector<T>` forwards to `VectorElement`.
unsafe impl<K, V> UniquePtrTarget for CxxMap<K, V>
where
    K: MapPair<V>,
    V: VectorElement,
{
    fn __typename(f: &mut Formatter) -> fmt::Result {
        K::__map_typename(f)
    }
    fn __null() -> MaybeUninit<*mut c_void> {
        K::__map_unique_ptr_null()
    }
    unsafe fn __raw(raw: *mut Self) -> MaybeUninit<*mut c_void> {
        // SAFETY: the caller promises `raw` is a map to take ownership of.
        unsafe { K::__map_unique_ptr_raw(raw) }
    }
    unsafe fn __get(repr: MaybeUninit<*mut c_void>) -> *const Self {
        // SAFETY: the caller promises `repr` is an initialised unique_ptr.
        unsafe { K::__map_unique_ptr_get(repr) }
    }
    unsafe fn __release(repr: MaybeUninit<*mut c_void>) -> *mut Self {
        // SAFETY: the caller promises `repr` is an initialised unique_ptr.
        unsafe { K::__map_unique_ptr_release(repr) }
    }
    unsafe fn __drop(repr: MaybeUninit<*mut c_void>) {
        // SAFETY: the caller promises `repr` is an initialised unique_ptr
        // which nobody will use again.
        unsafe { K::__map_unique_ptr_drop(repr) }
    }
}

/// Trait bound for the key/value pairs a [`CxxMap`] may be built from.
///
/// It is implemented on the key, parameterised by the value, so the bound on
/// generic code over a map reads `K: MapPair<V>`. It has no publicly callable
/// or implementable methods, and unlike [`cxx::vector::VectorElement`] it
/// cannot be implemented from outside at all: each impl needs a matching
/// `std::map` instantiation, and the only ones which exist are the ones this
/// crate compiles.
///
/// # Supported pairs
///
/// Every combination of these as value, and every one but `f32` and `f64` as
/// key:
///
/// - the variable-width C integers: [`c_int`](crate::c_int),
///   [`c_uint`](crate::c_uint), [`c_long`](crate::c_long),
///   [`c_ulong`](crate::c_ulong), [`c_short`](crate::c_short),
///   [`c_ushort`](crate::c_ushort), [`c_longlong`](crate::c_longlong),
///   [`c_ulonglong`](crate::c_ulonglong)
/// - the fixed-width integers, as the plain Rust types: `u8`, `i8`, `u16`,
///   `i16`, `u32`, `i32`, `u64`, `i64`, `usize`
/// - `f32` and `f64`
/// - the character types: [`c_char16_t`](crate::c_char16_t),
///   [`c_char32_t`](crate::c_char32_t), [`c_wchar_t`](crate::c_wchar_t)
/// - [`cxx::CxxString`]
///
/// That is the set which already goes inside a `std::vector` from either side
/// of the bridge, less the ones whose C++ type another entry names anyway
/// ([`c_u8`](crate::c_u8) through [`c_i64`](crate::c_i64), which exist to get
/// a fixed-width integer inside a `UniquePtr`) and `isize`, which is a `cxx`
/// type rather than a C++ one. `bool` is out because there is no
/// `CxxVector<bool>` for [`keys`] or [`values`] to return, and
/// `std::vector<bool>` would not be one. `f32` and `f64` are out as *keys*
/// because NaN would break the strict weak ordering `std::less<K>` owes the
/// map, so a floating-point key is refused at compile time:
///
/// ```compile_fail
/// autocxx::CxxMap::<f64, autocxx::c_int>::new();
/// ```
///
/// [`keys`]: CxxMap::keys
/// [`values`]: CxxMap::values
///
/// # Example
///
/// A bound `K: MapPair<V>` may be necessary when manipulating [`CxxMap`] in
/// generic code.
///
/// ```
/// use autocxx::{CxxMap, MapPair};
/// use cxx::vector::VectorElement;
///
/// pub fn count_entries<K, V>(map: &CxxMap<K, V>) -> usize
/// where
///     K: MapPair<V>,
///     V: VectorElement,
/// {
///     map.len()
/// }
/// ```
///
/// # Safety
///
/// An implementation asserts that the `extern "C"` glue it names operates on
/// the `std::map` instantiation `CxxMap<Self, V>` stands for. Nothing outside
/// this crate can honour that, which is why the trait is sealed.
pub unsafe trait MapPair<V>: private::Sealed<V> + VectorElement
where
    V: VectorElement,
{
    #[doc(hidden)]
    fn __map_typename(f: &mut Formatter) -> fmt::Result;
    #[doc(hidden)]
    fn __map_new() -> *mut CxxMap<Self, V>;
    #[doc(hidden)]
    fn __map_size(m: &CxxMap<Self, V>) -> usize;
    #[doc(hidden)]
    fn __map_find(m: &CxxMap<Self, V>, key: &Self) -> *const V;
    #[doc(hidden)]
    fn __map_insert(m: Pin<&mut CxxMap<Self, V>>, key: &Self, value: &V) -> bool;
    #[doc(hidden)]
    fn __map_insert_or_assign(m: Pin<&mut CxxMap<Self, V>>, key: &Self, value: &V) -> bool;
    #[doc(hidden)]
    fn __map_erase(m: Pin<&mut CxxMap<Self, V>>, key: &Self) -> bool;
    #[doc(hidden)]
    fn __map_keys(m: &CxxMap<Self, V>) -> *mut CxxVector<Self>;
    #[doc(hidden)]
    fn __map_values(m: &CxxMap<Self, V>) -> *mut CxxVector<V>;
    #[doc(hidden)]
    fn __map_unique_ptr_null() -> MaybeUninit<*mut c_void>;
    #[doc(hidden)]
    unsafe fn __map_unique_ptr_raw(raw: *mut CxxMap<Self, V>) -> MaybeUninit<*mut c_void>;
    #[doc(hidden)]
    unsafe fn __map_unique_ptr_get(repr: MaybeUninit<*mut c_void>) -> *const CxxMap<Self, V>;
    #[doc(hidden)]
    unsafe fn __map_unique_ptr_release(repr: MaybeUninit<*mut c_void>) -> *mut CxxMap<Self, V>;
    #[doc(hidden)]
    unsafe fn __map_unique_ptr_drop(repr: MaybeUninit<*mut c_void>);
}

mod private {
    /// Closes [`super::MapPair`]: only this crate can name it.
    pub trait Sealed<V> {}
}

/// The glue and the `ExternType` identity for one key/value pair.
///
/// `$kid` and `$vid` are the names both languages know the two types by: they
/// spell the symbols `cxx_map.cc` defines, and the namespace and typedef
/// `cxx_map.h` declares. `$kty` and `$vty` are the same two types in Rust.
macro_rules! impl_map_pair {
    ($kty:ty, $kid:ident, $vty:ty, $vid:ident) => {
        impl private::Sealed<$vty> for $kty {}

        // SAFETY: every function named below is the one `AUTOCXX_MAP_GLUE`
        // emitted in `cxx_map.cc` for this pair, working on the same
        // `std::map<$kid, $vid>` that `CxxMap<$kty, $vty>` stands for. All of
        // them are `noexcept`, so none can unwind into these frames.
        unsafe impl MapPair<$vty> for $kty {
            fn __map_typename(f: &mut Formatter) -> fmt::Result {
                f.write_str(concat!(
                    "CxxMap<",
                    stringify!($kid),
                    ", ",
                    stringify!($vid),
                    ">"
                ))
            }

            fn __map_new() -> *mut CxxMap<$kty, $vty> {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$std$map$", stringify!($kid), "_to_", stringify!($vid), "$new"
                    )]
                    fn __map_new() -> *mut CxxMap<$kty, $vty>;
                }
                // SAFETY: takes nothing, and returns a map it has just
                // allocated, so there is nothing for a caller to get wrong.
                unsafe { __map_new() }
            }

            fn __map_size(m: &CxxMap<$kty, $vty>) -> usize {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$std$map$", stringify!($kid), "_to_", stringify!($vid), "$size"
                    )]
                    fn __map_size(_: &CxxMap<$kty, $vty>) -> usize;
                }
                // SAFETY: reads through a live shared reference, and the C++
                // reads the map without changing it.
                unsafe { __map_size(m) }
            }

            fn __map_find(m: &CxxMap<$kty, $vty>, key: &$kty) -> *const $vty {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$std$map$", stringify!($kid), "_to_", stringify!($vid), "$find"
                    )]
                    fn __map_find(_: &CxxMap<$kty, $vty>, _: &$kty) -> *const $vty;
                }
                // SAFETY: both arguments are live shared references, and the
                // C++ only compares the key and reads the map.
                unsafe { __map_find(m, key) }
            }

            fn __map_insert(
                m: Pin<&mut CxxMap<$kty, $vty>>,
                key: &$kty,
                value: &$vty,
            ) -> bool {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$std$map$", stringify!($kid), "_to_", stringify!($vid), "$insert"
                    )]
                    fn __map_insert(
                        _: Pin<&mut CxxMap<$kty, $vty>>,
                        _: &$kty,
                        _: &$vty,
                    ) -> bool;
                }
                // SAFETY: the map is reached through a live pinned unique
                // reference and the two operands through live shared ones; the
                // C++ copies out of the operands and never keeps them.
                unsafe { __map_insert(m, key, value) }
            }

            fn __map_insert_or_assign(
                m: Pin<&mut CxxMap<$kty, $vty>>,
                key: &$kty,
                value: &$vty,
            ) -> bool {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$std$map$",
                        stringify!($kid),
                        "_to_",
                        stringify!($vid),
                        "$insert_or_assign"
                    )]
                    fn __map_insert_or_assign(
                        _: Pin<&mut CxxMap<$kty, $vty>>,
                        _: &$kty,
                        _: &$vty,
                    ) -> bool;
                }
                // SAFETY: as `__map_insert`, and the assignment it may do
                // instead writes a value the map already owns.
                unsafe { __map_insert_or_assign(m, key, value) }
            }

            fn __map_erase(m: Pin<&mut CxxMap<$kty, $vty>>, key: &$kty) -> bool {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$std$map$", stringify!($kid), "_to_", stringify!($vid), "$erase"
                    )]
                    fn __map_erase(_: Pin<&mut CxxMap<$kty, $vty>>, _: &$kty) -> bool;
                }
                // SAFETY: the map is reached through a live pinned unique
                // reference, so no reference handed out by `get` is alive; the
                // key is a live shared reference the C++ only compares.
                unsafe { __map_erase(m, key) }
            }

            fn __map_keys(m: &CxxMap<$kty, $vty>) -> *mut CxxVector<$kty> {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$std$map$", stringify!($kid), "_to_", stringify!($vid), "$keys"
                    )]
                    fn __map_keys(_: &CxxMap<$kty, $vty>) -> *mut CxxVector<$kty>;
                }
                // SAFETY: reads through a live shared reference, and returns a
                // vector of copies it has just allocated.
                unsafe { __map_keys(m) }
            }

            fn __map_values(m: &CxxMap<$kty, $vty>) -> *mut CxxVector<$vty> {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$std$map$", stringify!($kid), "_to_", stringify!($vid), "$values"
                    )]
                    fn __map_values(_: &CxxMap<$kty, $vty>) -> *mut CxxVector<$vty>;
                }
                // SAFETY: reads through a live shared reference, and returns a
                // vector of copies it has just allocated.
                unsafe { __map_values(m) }
            }

            fn __map_unique_ptr_null() -> MaybeUninit<*mut c_void> {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$unique_ptr$std$map$",
                        stringify!($kid),
                        "_to_",
                        stringify!($vid),
                        "$null"
                    )]
                    fn __map_unique_ptr_null(this: *mut MaybeUninit<*mut c_void>);
                }
                let mut repr = MaybeUninit::uninit();
                // SAFETY: `repr` is a live, suitably sized and aligned slot
                // for the one pointer a `std::unique_ptr` is, which is what
                // the C++ constructs into it.
                unsafe { __map_unique_ptr_null(&mut repr) }
                repr
            }

            unsafe fn __map_unique_ptr_raw(
                raw: *mut CxxMap<$kty, $vty>,
            ) -> MaybeUninit<*mut c_void> {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$unique_ptr$std$map$",
                        stringify!($kid),
                        "_to_",
                        stringify!($vid),
                        "$raw"
                    )]
                    fn __map_unique_ptr_raw(
                        this: *mut MaybeUninit<*mut c_void>,
                        raw: *mut CxxMap<$kty, $vty>,
                    );
                }
                let mut repr = MaybeUninit::uninit();
                // SAFETY: `repr` is a live slot as in `__map_unique_ptr_null`, and
                // the caller promises `raw` is a map to take ownership of.
                unsafe { __map_unique_ptr_raw(&mut repr, raw) }
                repr
            }

            unsafe fn __map_unique_ptr_get(
                repr: MaybeUninit<*mut c_void>,
            ) -> *const CxxMap<$kty, $vty> {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$unique_ptr$std$map$",
                        stringify!($kid),
                        "_to_",
                        stringify!($vid),
                        "$get"
                    )]
                    fn __map_unique_ptr_get(
                        this: *const MaybeUninit<*mut c_void>,
                    ) -> *const CxxMap<$kty, $vty>;
                }
                // SAFETY: the caller promises `repr` holds an initialised
                // `std::unique_ptr`, which is all the C++ reads.
                unsafe { __map_unique_ptr_get(&repr) }
            }

            unsafe fn __map_unique_ptr_release(
                mut repr: MaybeUninit<*mut c_void>,
            ) -> *mut CxxMap<$kty, $vty> {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$unique_ptr$std$map$",
                        stringify!($kid),
                        "_to_",
                        stringify!($vid),
                        "$release"
                    )]
                    fn __map_unique_ptr_release(
                        this: *mut MaybeUninit<*mut c_void>,
                    ) -> *mut CxxMap<$kty, $vty>;
                }
                // SAFETY: the caller promises `repr` holds an initialised
                // `std::unique_ptr`, which this leaves holding null.
                unsafe { __map_unique_ptr_release(&mut repr) }
            }

            unsafe fn __map_unique_ptr_drop(mut repr: MaybeUninit<*mut c_void>) {
                unsafe extern "C" {
                    #[link_name = concat!(
                        "autocxx$unique_ptr$std$map$",
                        stringify!($kid),
                        "_to_",
                        stringify!($vid),
                        "$drop"
                    )]
                    fn __map_unique_ptr_drop(this: *mut MaybeUninit<*mut c_void>);
                }
                // SAFETY: the caller promises `repr` holds an initialised
                // `std::unique_ptr` nobody will use again, which this runs the
                // destructor of in place.
                unsafe { __map_unique_ptr_drop(&mut repr) }
            }
        }

        // SAFETY: `autocxx::map::$kid::$vid` is the typedef `cxx_map.h`
        // declares for this very `std::map` instantiation, so a bridge which
        // writes that name writes this type. `Opaque` because a `std::map` has
        // a destructor and a non-trivial move constructor.
        unsafe impl ::cxx::ExternType for CxxMap<$kty, $vty> {
            type Id = ::cxx::type_id!(autocxx::map::$kid::$vid);
            type Kind = ::cxx::kind::Opaque;
        }
    };
}

/// Invokes `$cb!($kty, $kid, $vty, $vid)` once per supported value type.
///
/// The types are written crate-absolute because a `macro_rules!` body names
/// items in the scope it is *called* from, and the callers are in two modules.
macro_rules! for_each_map_value {
    ($cb:ident, $kty:ty, $kid:ident) => {
        $cb!($kty, $kid, crate::c_int, c_int);
        $cb!($kty, $kid, crate::c_uint, c_uint);
        $cb!($kty, $kid, crate::c_long, c_long);
        $cb!($kty, $kid, crate::c_ulong, c_ulong);
        $cb!($kty, $kid, crate::c_short, c_short);
        $cb!($kty, $kid, crate::c_ushort, c_ushort);
        $cb!($kty, $kid, crate::c_longlong, c_longlong);
        $cb!($kty, $kid, crate::c_ulonglong, c_ulonglong);
        $cb!($kty, $kid, u8, u8);
        $cb!($kty, $kid, i8, i8);
        $cb!($kty, $kid, u16, u16);
        $cb!($kty, $kid, i16, i16);
        $cb!($kty, $kid, u32, u32);
        $cb!($kty, $kid, i32, i32);
        $cb!($kty, $kid, u64, u64);
        $cb!($kty, $kid, i64, i64);
        $cb!($kty, $kid, usize, usize);
        $cb!($kty, $kid, f32, f32);
        $cb!($kty, $kid, f64, f64);
        $cb!($kty, $kid, crate::c_char16_t, c_char16_t);
        $cb!($kty, $kid, crate::c_char32_t, c_char32_t);
        $cb!($kty, $kid, crate::c_wchar_t, c_wchar_t);
        $cb!($kty, $kid, ::cxx::CxxString, string);
    };
}

/// Invokes `$cb!($kty, $kid, $vty, $vid)` once per supported pair.
///
/// This is the Rust half of `AUTOCXX_MAP_FOR_EACH_PAIR` in `cxx_map.h` and
/// lists the same types in the same order. A pair listed here and missing
/// there is an undefined symbol, which `every_pair_links` provokes. The key
/// rows are the value rows less `f32` and `f64`, which are values only.
macro_rules! for_each_map_pair {
    ($cb:ident) => {
        for_each_map_value!($cb, crate::c_int, c_int);
        for_each_map_value!($cb, crate::c_uint, c_uint);
        for_each_map_value!($cb, crate::c_long, c_long);
        for_each_map_value!($cb, crate::c_ulong, c_ulong);
        for_each_map_value!($cb, crate::c_short, c_short);
        for_each_map_value!($cb, crate::c_ushort, c_ushort);
        for_each_map_value!($cb, crate::c_longlong, c_longlong);
        for_each_map_value!($cb, crate::c_ulonglong, c_ulonglong);
        for_each_map_value!($cb, u8, u8);
        for_each_map_value!($cb, i8, i8);
        for_each_map_value!($cb, u16, u16);
        for_each_map_value!($cb, i16, i16);
        for_each_map_value!($cb, u32, u32);
        for_each_map_value!($cb, i32, i32);
        for_each_map_value!($cb, u64, u64);
        for_each_map_value!($cb, i64, i64);
        for_each_map_value!($cb, usize, usize);
        for_each_map_value!($cb, crate::c_char16_t, c_char16_t);
        for_each_map_value!($cb, crate::c_char32_t, c_char32_t);
        for_each_map_value!($cb, crate::c_wchar_t, c_wchar_t);
        for_each_map_value!($cb, ::cxx::CxxString, string);
    };
}

for_each_map_pair!(impl_map_pair);

/// The contents of `cxx_map.h`, whose typedefs the C++ side of a hand-written
/// `cxx::bridge` naming a [`CxxMap`] has to see.
///
/// The same arrangement as `cxx_gen::HEADER` is for `cxx.h`: the header ships
/// as a string, and a build script writes it wherever its C++ compiles from.
/// With `autocxx` among the `[build-dependencies]`:
///
/// ```no_run
/// // build.rs
/// let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
/// std::fs::write(out.join("autocxx_cxx_map.h"), autocxx::CXX_MAP_HEADER).unwrap();
/// // ...and put `out` on the include path of whatever compiles the bridge,
/// // e.g. `cxx_build::bridge("src/main.rs").include(&out)`.
/// ```
///
/// The bridge then includes it and names a pair's typedef:
///
/// ```ignore
/// #[cxx::bridge]
/// mod ffi {
///     #[namespace = "autocxx::map::c_int"]
///     unsafe extern "C++" {
///         include!("autocxx_cxx_map.h");
///         type c_int = autocxx::CxxMap<autocxx::c_int, autocxx::c_int>;
///     }
/// }
/// ```
pub static CXX_MAP_HEADER: &str = include_str!("cxx_map.h");

/// Names each glue function of one pair, so that the linker has to find it.
#[cfg(test)]
fn require_glue<K, V>()
where
    K: MapPair<V>,
    V: VectorElement,
{
    use core::hint::black_box;
    black_box::<fn() -> *mut CxxMap<K, V>>(K::__map_new);
    black_box::<fn(&CxxMap<K, V>) -> usize>(K::__map_size);
    black_box::<fn(&CxxMap<K, V>, &K) -> *const V>(K::__map_find);
    black_box::<fn(Pin<&mut CxxMap<K, V>>, &K, &V) -> bool>(K::__map_insert);
    black_box::<fn(Pin<&mut CxxMap<K, V>>, &K, &V) -> bool>(K::__map_insert_or_assign);
    black_box::<fn(Pin<&mut CxxMap<K, V>>, &K) -> bool>(K::__map_erase);
    black_box::<fn(&CxxMap<K, V>) -> *mut CxxVector<K>>(K::__map_keys);
    black_box::<fn(&CxxMap<K, V>) -> *mut CxxVector<V>>(K::__map_values);
    black_box::<fn() -> MaybeUninit<*mut c_void>>(K::__map_unique_ptr_null);
    black_box::<unsafe fn(*mut CxxMap<K, V>) -> MaybeUninit<*mut c_void>>(K::__map_unique_ptr_raw);
    black_box::<unsafe fn(MaybeUninit<*mut c_void>) -> *const CxxMap<K, V>>(
        K::__map_unique_ptr_get,
    );
    black_box::<unsafe fn(MaybeUninit<*mut c_void>) -> *mut CxxMap<K, V>>(
        K::__map_unique_ptr_release,
    );
    black_box::<unsafe fn(MaybeUninit<*mut c_void>)>(K::__map_unique_ptr_drop);
}

#[cfg(test)]
macro_rules! require_pair_glue {
    ($kty:ty, $kid:ident, $vty:ty, $vid:ident) => {
        require_glue::<$kty, $vty>();
    };
}

#[cfg(test)]
mod tests {
    use super::{require_glue, CxxMap};
    use crate::{c_int, c_uint};
    use cxx::{let_cxx_string, CxxString};

    #[test]
    fn every_pair_links() {
        for_each_map_pair!(require_pair_glue);
    }

    #[test]
    fn header_constant_carries_the_typedef_tables() {
        use super::CXX_MAP_HEADER;
        // The namespaces every typedef lives under.
        assert!(CXX_MAP_HEADER.contains("namespace autocxx"));
        assert!(CXX_MAP_HEADER.contains("namespace map"));
        // Sentinel rows of the value table.
        assert!(CXX_MAP_HEADER.contains("X(KN, c_int, K, int)"));
        assert!(CXX_MAP_HEADER.contains("X(KN, string, K, std::string)"));
        assert!(CXX_MAP_HEADER.contains("X(KN, f64, K, double)"));
        // Sentinel rows of the key table, which has no float rows.
        assert!(CXX_MAP_HEADER.contains("AUTOCXX_MAP_FOR_EACH_VALUE(X, c_int, int)"));
        assert!(CXX_MAP_HEADER.contains("AUTOCXX_MAP_FOR_EACH_VALUE(X, string, std::string)"));
        assert!(!CXX_MAP_HEADER.contains("AUTOCXX_MAP_FOR_EACH_VALUE(X, f32, float)"));
        assert!(!CXX_MAP_HEADER.contains("AUTOCXX_MAP_FOR_EACH_VALUE(X, f64, double)"));
    }

    #[test]
    fn new_map_is_empty() {
        let map = CxxMap::<c_int, c_int>::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
        assert!(!map.contains(&c_int(0)));
        assert_eq!(map.get(&c_int(0)), None);
        assert!(map.keys().is_empty());
        assert!(map.values().is_empty());
    }

    #[test]
    fn insert_then_get_round_trips() {
        let mut map = CxxMap::<c_int, c_int>::new();
        assert!(map.pin_mut().insert(&c_int(7), &c_int(70)));
        assert_eq!(map.len(), 1);
        assert!(map.contains(&c_int(7)));
        assert_eq!(map.get(&c_int(7)), Some(&c_int(70)));
        assert_eq!(map.get(&c_int(8)), None);
    }

    #[test]
    fn first_insert_wins() {
        let mut map = CxxMap::<c_int, c_int>::new();
        assert!(map.pin_mut().insert(&c_int(1), &c_int(10)));
        assert!(!map.pin_mut().insert(&c_int(1), &c_int(20)));
        assert_eq!(map.get(&c_int(1)), Some(&c_int(10)));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn insert_or_assign_overwrites() {
        let mut map = CxxMap::<c_int, c_int>::new();
        assert!(map.pin_mut().insert_or_assign(&c_int(1), &c_int(10)));
        assert!(!map.pin_mut().insert_or_assign(&c_int(1), &c_int(20)));
        assert_eq!(map.get(&c_int(1)), Some(&c_int(20)));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn erase_reports_whether_there_was_an_entry() {
        let mut map = CxxMap::<c_int, c_int>::new();
        map.pin_mut().insert(&c_int(1), &c_int(10));
        assert!(map.pin_mut().erase(&c_int(1)));
        assert!(!map.pin_mut().erase(&c_int(1)));
        assert!(map.is_empty());
    }

    #[test]
    fn keys_and_values_snapshot_in_key_order() {
        let mut map = CxxMap::<c_int, c_uint>::new();
        for key in [3, 1, 2] {
            map.pin_mut().insert(&c_int(key), &c_uint(key as u32 * 10));
        }
        let keys = map.keys();
        let values = map.values();
        assert_eq!(keys.len(), 3);
        assert_eq!(values.len(), 3);
        assert_eq!(
            keys.iter().copied().collect::<Vec<_>>(),
            vec![c_int(1), c_int(2), c_int(3)]
        );
        assert_eq!(
            values.iter().copied().collect::<Vec<_>>(),
            vec![c_uint(10), c_uint(20), c_uint(30)]
        );
        // A snapshot, not a view.
        map.pin_mut().insert(&c_int(4), &c_uint(40));
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn integer_keys_with_string_values() {
        let mut map = CxxMap::<i32, CxxString>::new();
        let_cxx_string!(one = "one");
        let_cxx_string!(two = "two");
        assert!(map.pin_mut().insert(&1, &one));
        assert!(map.pin_mut().insert(&2, &two));
        // The map holds copies, so the originals are still ours.
        assert_eq!(one.to_str().unwrap(), "one");
        assert_eq!(map.get(&1).unwrap().to_str().unwrap(), "one");
        assert_eq!(map.get(&3), None);

        let values = map.values();
        let read: Vec<&str> = values.iter().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(read, vec!["one", "two"]);
    }

    #[test]
    fn string_keys_with_string_values() {
        let mut map = CxxMap::<CxxString, CxxString>::new();
        let_cxx_string!(beta = "beta");
        let_cxx_string!(alpha = "alpha");
        let_cxx_string!(first = "1st");
        let_cxx_string!(second = "2nd");
        assert!(map.pin_mut().insert(&beta, &second));
        assert!(map.pin_mut().insert(&alpha, &first));
        assert!(!map.pin_mut().insert(&alpha, &second));
        assert_eq!(map.get(&alpha).unwrap().to_str().unwrap(), "1st");

        // Byte order, so "alpha" sorts before "beta".
        let keys = map.keys();
        let read: Vec<&str> = keys.iter().map(|k| k.to_str().unwrap()).collect();
        assert_eq!(read, vec!["alpha", "beta"]);

        assert!(map.pin_mut().erase(&beta));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn a_reference_from_get_borrows_the_map() {
        // `get` hands back a reference into the map, so the map is borrowed
        // for as long as it lives. Nothing here would compile if it were not:
        // `insert` needs `Pin<&mut _>` from the same `UniquePtr`.
        let mut map = CxxMap::<c_int, c_int>::new();
        map.pin_mut().insert(&c_int(1), &c_int(10));
        let value: &c_int = map.get(&c_int(1)).unwrap();
        assert_eq!(*value, c_int(10));
        map.pin_mut().insert(&c_int(2), &c_int(20));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn a_null_unique_ptr_holds_no_map() {
        let map: cxx::UniquePtr<CxxMap<c_int, c_int>> = cxx::UniquePtr::null();
        assert!(map.is_null());
        assert!(map.as_ref().is_none());
        // Dropping a null unique_ptr must not free anything.
        drop(map);
    }
}
