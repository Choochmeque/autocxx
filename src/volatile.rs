// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use core::fmt;
use core::ptr;

/// The address of storage C++ qualified `volatile`, and the only two
/// operations which keep that qualifier's promise once Rust holds it.
///
/// C++ writes `volatile` on the *object*; Rust states volatility in the
/// *access*, through [`ptr::read_volatile`] and [`ptr::write_volatile`], and
/// has no type which carries the promise. So a `volatile T*` or `volatile T&`
/// which autocxx handed over as an ordinary `*mut T` would be read with an
/// ordinary load, which the optimizer may elide, duplicate or reorder - the
/// one thing the qualifier exists to forbid. This type is what autocxx hands
/// over instead: the address, with [`read`](Self::read) and
/// [`write`](Self::write) as the way to use it.
///
/// What is promised is what [`ptr::read_volatile`] promises, which is what C++
/// promises for an access through the qualifier: the access happens, once per
/// call, and may not be elided, duplicated or reordered against other volatile
/// accesses. Neither language guarantees which instructions that becomes, but
/// on the compilers both of these are built with it is the same one.
///
/// # This is not a reference
///
/// It is deliberately not `&T` or `&mut T`. A Rust shared reference promises
/// the referent does not change while it lives, and a hardware register
/// changes precisely when nothing in the program touched it - so a `&u32` onto
/// one would be a false statement to the compiler rather than merely a lost
/// guarantee. (autocxx's [`crate::CppRef`] exists for the same reason on the
/// C++ side of the same problem.)
///
/// # This is not atomic
///
/// A volatile access is not an atomic one and carries no ordering relative to
/// anything but other volatile accesses. It says the access happens; it does
/// not say what another thread sees. Concurrent access to the same address is
/// a data race with the same consequences as any other, and volatility does
/// not excuse it. Use the [`core::sync::atomic`] types where synchronization
/// is what is wanted.
///
/// `VolatilePtr` is accordingly neither `Send` nor `Sync`, which it inherits
/// from the raw pointer it wraps. Nothing here overrides that.
///
/// # `T` is a scalar
///
/// autocxx generates this only for a pointee of built-in type. That line is
/// drawn by autocxx and not by the `Copy` bound here, which is weaker: `Copy`
/// is what makes [`read`](Self::read) sound, since [`ptr::read_volatile`]
/// produces a value without consuming the one it read and would otherwise
/// duplicate ownership of something with a destructor. Constructing the type
/// over some other `Copy` pointee is possible and is not unsound; it is simply
/// not something autocxx will generate.
///
/// # Read-only registers
///
/// A `const volatile T*` - how a status register the program may not write is
/// declared - becomes [`VolatileConstPtr`] instead, which has no `write`.
#[repr(transparent)]
pub struct VolatilePtr<T: Copy> {
    ptr: *mut T,
}

impl<T: Copy> VolatilePtr<T> {
    /// Wraps an address. Nothing is read or written, so nothing is required of
    /// the address until [`read`](Self::read) or [`write`](Self::write) is
    /// called.
    ///
    /// This is how a register block whose address is known to Rust rather than
    /// to C++ - `0x4000_0000 as *mut u32` - enters the same API as one C++
    /// returned.
    pub fn new(ptr: *mut T) -> Self {
        Self { ptr }
    }

    /// The address, to hand to something which wants a plain pointer.
    ///
    /// Reading through the result is an ordinary load and drops the promise
    /// this type exists to keep.
    pub fn as_raw(self) -> *mut T {
        self.ptr
    }

    /// Whether the address is null. C++ can return a null `volatile T*`; a
    /// `volatile T&` position never yields one.
    pub fn is_null(self) -> bool {
        self.ptr.is_null()
    }

    /// Performs one volatile read.
    ///
    /// # Safety
    ///
    /// The address must be valid for a read of `T`, properly aligned, and
    /// point at an initialized value, exactly as [`ptr::read_volatile`]
    /// requires. Whether it is depends on what C++ handed over and on how long
    /// that stays true, neither of which autocxx can check.
    ///
    /// The read is not atomic: a concurrent write to the same address from
    /// another thread is a data race.
    pub unsafe fn read(self) -> T {
        // SAFETY: the caller has guaranteed the address is valid, aligned and
        // initialized for `T`, which is what `read_volatile` asks. `T: Copy`
        // rules out duplicating ownership of a value with a destructor.
        unsafe { ptr::read_volatile(self.ptr) }
    }

    /// Performs one volatile write.
    ///
    /// # Safety
    ///
    /// The address must be valid for a write of `T` and properly aligned,
    /// exactly as [`ptr::write_volatile`] requires.
    ///
    /// The write is not atomic: a concurrent access to the same address from
    /// another thread is a data race.
    pub unsafe fn write(self, value: T) {
        // SAFETY: the caller has guaranteed the address is valid for a write
        // and aligned, which is what `write_volatile` asks.
        unsafe { ptr::write_volatile(self.ptr, value) }
    }
}

// Derived implementations would demand `T: Clone`/`T: Copy` on the *value*,
// which is beside the point: an address is copyable whatever it addresses.
impl<T: Copy> Clone for VolatilePtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Copy> Copy for VolatilePtr<T> {}

impl<T: Copy> fmt::Debug for VolatilePtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The address only. Reading the pointee to print it would be a
        // volatile access performed by a `Debug` impl.
        f.debug_tuple("VolatilePtr").field(&self.ptr).finish()
    }
}

impl<T: Copy> PartialEq for VolatilePtr<T> {
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.ptr, other.ptr)
    }
}

impl<T: Copy> Eq for VolatilePtr<T> {}

/// The address of storage C++ qualified `const volatile` - a register the
/// program may read but not write, which is how a hardware status register is
/// declared.
///
/// Everything [`VolatilePtr`] documents applies here: it is not a reference,
/// the access is not atomic, the type is neither `Send` nor `Sync`, and `T` is
/// a scalar. The single difference is that there is no `write`, because C++
/// said the storage is `const` and writing through a pointer to a `const`
/// object is undefined however the write is performed.
#[repr(transparent)]
pub struct VolatileConstPtr<T: Copy> {
    ptr: *const T,
}

impl<T: Copy> VolatileConstPtr<T> {
    /// Wraps an address. Nothing is read, so nothing is required of the
    /// address until [`read`](Self::read) is called.
    pub fn new(ptr: *const T) -> Self {
        Self { ptr }
    }

    /// The address, to hand to something which wants a plain pointer.
    ///
    /// Reading through the result is an ordinary load and drops the promise
    /// this type exists to keep.
    pub fn as_raw(self) -> *const T {
        self.ptr
    }

    /// Whether the address is null.
    pub fn is_null(self) -> bool {
        self.ptr.is_null()
    }

    /// Performs one volatile read.
    ///
    /// # Safety
    ///
    /// The address must be valid for a read of `T`, properly aligned, and
    /// point at an initialized value, exactly as [`ptr::read_volatile`]
    /// requires.
    ///
    /// The read is not atomic: a concurrent write to the same address from
    /// another thread is a data race.
    pub unsafe fn read(self) -> T {
        // SAFETY: the caller has guaranteed the address is valid, aligned and
        // initialized for `T`, which is what `read_volatile` asks. `T: Copy`
        // rules out duplicating ownership of a value with a destructor.
        unsafe { ptr::read_volatile(self.ptr) }
    }
}

impl<T: Copy> Clone for VolatileConstPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Copy> Copy for VolatileConstPtr<T> {}

impl<T: Copy> fmt::Debug for VolatileConstPtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("VolatileConstPtr").field(&self.ptr).finish()
    }
}

impl<T: Copy> PartialEq for VolatileConstPtr<T> {
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.ptr, other.ptr)
    }
}

impl<T: Copy> Eq for VolatileConstPtr<T> {}
