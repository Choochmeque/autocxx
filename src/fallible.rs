// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Storing the result of a C++ constructor which may throw.
//!
//! A constructor named by the `throws!` directive hands back a
//! [`moveit::new::TryNew`] rather than a [`moveit::new::New`]: constructing it
//! into a place may fail, and when it does the place is left uninitialized.
//! This module provides the three ways to store one - a [`cxx::UniquePtr`], a
//! [`Box`], and the stack - each of which gives back a `Result` and, on `Err`,
//! releases the storage without running a destructor for the object which was
//! never constructed.

use std::mem::MaybeUninit;
use std::pin::Pin;

use cxx::memory::UniquePtrTarget;
use cxx::UniquePtr;
use moveit::drop_flag::{DropFlag, TrappedFlag};
use moveit::new::TryNew;
use moveit::{Emplace, MakeCppStorage, MoveRef, Slot};

use crate::CppPin;

/// Emplaces a [`moveit::new::TryNew`] - what a `throws!` constructor hands
/// back - into a [`cxx::UniquePtr`]. Automatically imported by the autocxx
/// prelude.
///
/// This is the fallible counterpart of [`crate::WithinUniquePtr`]. Every
/// infallible [`moveit::new::New`] is also a `TryNew` whose error is
/// [`std::convert::Infallible`], so this trait is available on those too; the
/// `Result` it hands back for one of those can only ever be `Ok`.
pub trait TryWithinUniquePtr {
    /// The type being constructed.
    type Inner: UniquePtrTarget + MakeCppStorage;
    /// What construction fails with. For a `throws!` constructor this is
    /// [`cxx::Exception`].
    type Error;
    /// Create this item within a [`cxx::UniquePtr`], or report why the C++
    /// constructor refused to make it.
    ///
    /// If the constructor throws, the heap block allocated for the object is
    /// freed without any destructor being run on it: the object never came
    /// into existence, and C++ has already destroyed whichever of its
    /// sub-objects had been constructed by the time the exception was thrown.
    fn try_within_unique_ptr(self) -> Result<UniquePtr<Self::Inner>, Self::Error>;
}

impl<N, T> TryWithinUniquePtr for N
where
    N: TryNew<Output = T>,
    T: UniquePtrTarget + MakeCppStorage,
{
    type Inner = T;
    type Error = N::Error;
    fn try_within_unique_ptr(self) -> Result<UniquePtr<T>, N::Error> {
        UniquePtr::try_emplace(self)
    }
}

/// Emplaces a [`moveit::new::TryNew`] - what a `throws!` constructor hands
/// back - into a [`Box`] or a [`CppPin`]. Automatically imported by the autocxx
/// prelude.
///
/// This is the fallible counterpart of [`crate::WithinBox`]. Every infallible
/// [`moveit::new::New`] is also a `TryNew` whose error is
/// [`std::convert::Infallible`], so this trait is available on those too; the
/// `Result` it hands back for one of those can only ever be `Ok`.
pub trait TryWithinBox {
    /// The type being constructed.
    type Inner;
    /// What construction fails with. For a `throws!` constructor this is
    /// [`cxx::Exception`].
    type Error;
    /// Create this item inside a pinned box, or report why the C++ constructor
    /// refused to make it.
    ///
    /// If the constructor throws, the box's storage is released without any
    /// destructor being run on it.
    fn try_within_box(self) -> Result<Pin<Box<Self::Inner>>, Self::Error>;
    /// Create this item inside a [`CppPin`], or report why the C++ constructor
    /// refused to make it.
    fn try_within_cpp_pin(self) -> Result<CppPin<Self::Inner>, Self::Error>;
}

impl<N, T> TryWithinBox for N
where
    N: TryNew<Output = T>,
{
    type Inner = T;
    type Error = N::Error;
    fn try_within_box(self) -> Result<Pin<Box<T>>, N::Error> {
        Box::try_emplace(self)
    }
    fn try_within_cpp_pin(self) -> Result<CppPin<T>, N::Error> {
        Ok(CppPin::from_pinned_box(Box::try_emplace(self)?))
    }
}

/// Stack storage for a C++ object whose constructor may throw, created by
/// [`crate::stack_slot!`].
///
/// This exists because [`moveit::Slot`], the stack storage the `moveit!` macro
/// uses, cannot report a failed construction. `moveit::Slot::try_emplace`
/// raises its drop flag before running the constructor, and when the
/// constructor fails it returns early without lowering the flag again and
/// without handing out the [`MoveRef`] whose destructor would have lowered it.
/// The [`TrappedFlag`] guarding the storage then finds a raised flag at the end
/// of the scope and aborts the process - which is precisely the outcome a
/// throwing constructor is supposed to avoid. `StackSlot` keeps its own handle
/// on that flag and lowers it on the failure path.
pub struct StackSlot<'frame, T> {
    slot: Slot<'frame, T>,
    /// A second handle on the same counter `slot` holds. [`DropFlag`] is `Copy`
    /// and is only a reference to a shared counter, so this is another view of
    /// one flag rather than a second flag.
    flag: DropFlag<'frame>,
}

impl<'frame, T> StackSlot<'frame, T> {
    /// Creates a `StackSlot` over caller-provided storage.
    ///
    /// Use [`crate::stack_slot!`] rather than calling this; it is public only
    /// so that the macro has something to call.
    ///
    /// # Safety
    ///
    /// * `place` must not be outlived by any other pointer to its storage.
    /// * `trap`'s flag must be dead - that is, `trap` must be freshly created
    ///   and not shared with any other slot.
    /// * `trap` must be dropped before `place`'s storage is released or
    ///   reused, so that a [`MoveRef`] which was leaked rather than dropped
    ///   aborts the process while the storage it pinned is still there.
    #[doc(hidden)]
    pub unsafe fn new_unchecked(
        place: &'frame mut MaybeUninit<T>,
        trap: &'frame TrappedFlag,
    ) -> Self {
        let flag = trap.flag();
        Self {
            // SAFETY: this function's contract is `Slot::new_unchecked`'s
            // contract, and the caller has just promised it.
            slot: unsafe { Slot::new_unchecked(place, flag) },
            flag,
        }
    }

    /// Runs a fallible C++ constructor into this storage.
    ///
    /// On success the object is pinned to this stack frame and is destroyed
    /// when the returned [`MoveRef`] is dropped. On failure nothing was
    /// constructed, so nothing is destroyed and the storage is abandoned.
    ///
    /// A *panic* part-way through construction - as distinct from a returned
    /// `Err` - still leaves the drop flag raised and so aborts the process,
    /// exactly as it does with `moveit`'s own `Slot`. Nothing autocxx generates
    /// panics here: a C++ exception is caught on the C++ side of the boundary
    /// and arrives as an `Err`.
    pub fn try_emplace<N: TryNew<Output = T>>(
        self,
        new: N,
    ) -> Result<Pin<MoveRef<'frame, T>>, N::Error> {
        let Self { slot, flag } = self;
        slot.try_emplace(new).inspect_err(|_| {
            // `Slot::try_emplace` raised the flag before running the
            // constructor and, having failed, returned without lowering it and
            // without handing out the `MoveRef` which would have lowered it.
            // Lower it here, or the `TrappedFlag` this slot was built over
            // aborts the process when the enclosing scope ends. Nothing was
            // constructed, so there is nothing to destroy alongside it.
            //
            // Were a future moveit to stop raising the flag until construction
            // has succeeded, this would become a no-op rather than an
            // underflow: `dec_and_check_if_died` returns `false` without
            // touching a counter which is already zero.
            flag.dec_and_check_if_died();
        })
    }
}

/// Reserves stack storage for a C++ object whose constructor may throw, and
/// binds a [`StackSlot`] over it.
///
/// This is the fallible counterpart of `moveit!`. Where `moveit!` reserves the
/// storage and constructs into it in a single `let`, this macro can only do
/// the first half: construction may fail, and a `let` binding has nowhere to
/// put the failure. Writing the two halves separately has the advantage that
/// the fallible half is an ordinary expression, and so takes `?`:
///
/// ```
/// use autocxx::prelude::*;
/// use autocxx::moveit::new;
///
/// fn make_one() -> Result<(), std::num::TryFromIntError> {
///     autocxx::stack_slot!(storage);
///     let n = storage.try_emplace(new::try_from::<i32, i64>(42))?;
///     assert_eq!(*n, 42);
///     Ok(())
/// }
/// # make_one().unwrap();
/// ```
///
/// In autocxx code the thing being emplaced is whatever a `throws!` constructor
/// handed back, and the error is a [`cxx::Exception`].
///
/// As with `moveit!`'s storage, the slot must be a statement of its own: it
/// binds stack storage which the constructed object then borrows.
#[macro_export]
macro_rules! stack_slot {
    ($($name:ident $(: $ty:ty)?),* $(,)*) => {$(
        let mut place = ::core::mem::MaybeUninit::<
            $crate::stack_slot!(@tyof $($ty)?)
        >::uninit();
        // Declared after `place` so that it is dropped before it: a `MoveRef`
        // which was leaked leaves this flag raised, and the abort that then
        // follows has to happen while the storage it was guarding is still
        // there.
        let trap = $crate::moveit::drop_flag::TrappedFlag::new();
        // SAFETY: `place` is a fresh local which nothing else points at, and
        // `trap` is a fresh flag used by this slot alone. Both outlive the
        // slot, and `trap` is dropped before `place`.
        //
        // The two lints below are about *this expansion*, not about anything a
        // caller wrote, and neither is reporting a problem. `unsafe_code` is a
        // lint a crate turns on to forbid its own authors from writing
        // `unsafe`; the `unsafe` here is this macro's, not theirs, and a macro
        // which cannot be called from such a crate is a macro they cannot use
        // at all. `unused_unsafe` fires when a caller invokes this from inside
        // an `unsafe` block or fn, where the block below is redundant - which
        // is a fact about the call site and not something the caller can act on
        // here. `moveit::slot!` carries the same pair for the same reasons.
        // Nothing else in this crate is suppressed.
        #[allow(unsafe_code, unused_unsafe)]
        let $name = unsafe { $crate::StackSlot::new_unchecked(&mut place, &trap) };
    )*};
    (@tyof) => {_};
    (@tyof $ty:ty) => {$ty};
}

#[cfg(test)]
mod tests {
    use super::TryWithinBox;
    use moveit::new;
    use std::cell::Cell;

    thread_local! {
        static DROPS: Cell<usize> = const { Cell::new(0) };
    }

    struct CountsItsDrops;

    impl Drop for CountsItsDrops {
        fn drop(&mut self) {
            DROPS.with(|d| d.set(d.get() + 1));
        }
    }

    fn drops() -> usize {
        DROPS.with(|d| d.get())
    }

    /// A constructor which fails part-way, as a throwing C++ one does. The
    /// place is left untouched, which is what [`moveit::new::TryNew`] requires
    /// of an `Err` return.
    fn refuses<T>() -> impl moveit::new::TryNew<Output = T, Error = &'static str> {
        // SAFETY: the closure never writes to `this`, and returns `Err`, which
        // is exactly the case in which `TryNew` permits the place to be left
        // uninitialized.
        unsafe { new::try_by_raw(|_| Err("no")) }
    }

    /// The reason [`super::StackSlot`] exists: emplacing straight into a
    /// `moveit::Slot` aborts the process when the constructor fails, so this
    /// asserts that ours returns instead. Reaching the assertion at all is
    /// most of the test.
    #[test]
    fn a_failed_stack_construction_is_reported_rather_than_fatal() {
        let before = drops();
        {
            crate::stack_slot!(storage);
            let outcome = storage.try_emplace(refuses::<CountsItsDrops>());
            assert_eq!(outcome.err(), Some("no"));
        }
        assert_eq!(
            drops(),
            before,
            "nothing was constructed, so nothing to drop"
        );
    }

    #[test]
    fn a_stack_constructed_object_is_destroyed_at_the_end_of_its_scope() {
        let before = drops();
        {
            crate::stack_slot!(storage);
            let obj = storage
                .try_emplace(new::try_by(|| Ok::<_, &str>(CountsItsDrops)))
                .expect("this constructor succeeds");
            assert_eq!(drops(), before, "still alive inside its scope");
            drop(obj);
            assert_eq!(drops(), before + 1);
        }
        assert_eq!(drops(), before + 1, "and destroyed exactly once");
    }

    /// The slot can be given an explicit type, as `moveit::slot!` allows.
    #[test]
    fn a_stack_slot_may_name_its_type() {
        crate::stack_slot!(storage: i32);
        let n = storage
            .try_emplace(new::try_from::<i32, i64>(7))
            .expect("7 fits in an i32");
        assert_eq!(*n, 7);
    }

    /// Several slots at once, and slots named after the macro's own internal
    /// bindings - which macro hygiene keeps separate, so that a caller's
    /// `place` is their own.
    #[test]
    fn stack_slots_do_not_collide_with_each_other_or_with_the_caller() {
        let place = 1i64;
        crate::stack_slot!(trap: i32, storage: i32);
        let first = trap
            .try_emplace(new::try_from::<i32, i64>(place))
            .expect("1 fits in an i32");
        let second = storage
            .try_emplace(new::try_from::<i32, i64>(2))
            .expect("2 fits in an i32");
        assert_eq!((*first, *second), (1, 2));
    }

    #[test]
    fn a_failed_boxed_construction_frees_the_box_without_destroying_anything() {
        let before = drops();
        let outcome = refuses::<CountsItsDrops>().try_within_box();
        assert!(outcome.is_err());
        assert_eq!(drops(), before);
    }

    #[test]
    fn a_boxed_construction_which_succeeds_yields_the_object() {
        let before = drops();
        {
            let obj = new::try_by(|| Ok::<_, &str>(CountsItsDrops))
                .try_within_box()
                .expect("this constructor succeeds");
            assert_eq!(drops(), before);
            drop(obj);
        }
        assert_eq!(drops(), before + 1);
    }
}
