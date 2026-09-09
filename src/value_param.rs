// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::{AsCppMutRef, CppPin, CppUniquePtrPin};
use cxx::{memory::UniquePtrTarget, UniquePtr};
use moveit::{AsMove, CopyNew, MoveNew, New};
use std::{marker::PhantomPinned, mem::MaybeUninit, ops::Deref, pin::Pin};

/// A trait representing a parameter to a C++ function which is received
/// by value.
///
/// Rust has the concept of receiving parameters by _move_ or by _reference_.
/// C++ has the concept of receiving a parameter by 'value', which means
/// the parameter gets copied.
///
/// To make it easy to pass such parameters from Rust, this trait exists.
/// It is implemented both for references `&T` and for `UniquePtr<T>`,
/// subject to the presence or absence of suitable copy and move constructors.
/// This allows you to pass in parameters by copy (as is ergonomic and normal
/// in C++) retaining the original parameter; or by move semantics thus
/// destroying the object you're passing in. Simply use a reference if you want
/// copy semantics, or the item itself if you want move semantics.
///
/// The same goes for the owning C++ reference wrappers,
/// [`crate::CppPin`] and [`crate::CppUniquePtrPin`]: hand one over and C++
/// takes the object itself, leaving you nothing. To keep the object, copy out
/// of it explicitly - see below.
///
/// It is not recommended that you implement this trait, nor that you directly
/// use its methods, which are for use by `autocxx` generated code only.
///
/// # Use of `moveit` traits
///
/// Most of the implementations of this trait require the type to implement
/// [`CopyNew`], which is simply the `autocxx`/`moveit` way of saying that
/// the type has a copy constructor in C++.
///
/// # Being explicit
///
/// If you wish to explicitly force either a move or a copy of some type,
/// use [`as_mov`] or [`as_copy`].
///
/// [`as_copy`] is also how you pass by value out of a [`crate::CppPin`] or a
/// [`crate::CppRef`] while keeping the original, since neither of those hands
/// out a Rust reference for free: write `as_copy(unsafe { pin.as_ref() })`,
/// and in doing so promise that C++ won't mutate the referent while the copy
/// constructor runs.
///
/// # Performance
///
/// At present, some additional copying occurs for all implementations of
/// this trait other than that for [`cxx::UniquePtr`]. In the future it's
/// hoped that the implementation for `&T where T: CopyNew` can also avoid
/// this extra copying.
///
/// # Panics
///
/// The implementations of this trait which take a [`cxx::UniquePtr`], or a
/// [`crate::CppUniquePtrPin`] holding one, will panic if the pointer is NULL.
///
/// # Safety
///
/// Implementers must guarantee that the pointer returned by `get_ptr`
/// is of the correct size and alignment of `T`.
pub unsafe trait ValueParam<T> {
    /// Any stack storage required. If, as part of passing to C++,
    /// we need to store a temporary copy of the value, this will be `T`,
    /// otherwise `()`.
    #[doc(hidden)]
    type StackStorage;
    /// Populate the stack storage given as a parameter.
    ///
    /// # Safety
    ///
    /// Callers must guarantee that this object will not move in memory
    /// between this call and any subsequent `get_ptr` call or drop.
    #[doc(hidden)]
    unsafe fn populate_stack_space(self, this: Pin<&mut Option<Self::StackStorage>>);
    /// Retrieve the pointer to the underlying item, to be passed to C++.
    /// Note that on the C++ side this is currently passed to `std::move`
    /// and therefore may be mutated.
    ///
    /// # Safety
    ///
    /// The storage must hold a value and must not have moved since it was
    /// built: for the `MaybeUninit` implementations that means
    /// `populate_stack_space` returned and nothing has since destroyed or
    /// replaced what it left there.
    #[doc(hidden)]
    unsafe fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut T;
    #[doc(hidden)]
    /// Any special drop steps required for the stack storage. This is not
    /// necessary if the `StackStorage` type is something self-dropping
    /// such as `UniquePtr`; it's only necessary if it's something where
    /// manual management is required such as `MaybeUninit`.
    ///
    /// # Safety
    ///
    /// As for `get_ptr`, and this must be the only such call: an
    /// implementation which needs one destroys the value in place, so nothing
    /// may touch the storage afterwards.
    unsafe fn do_drop(_stack: Pin<&mut Self::StackStorage>) {}
}

unsafe impl<T> ValueParam<T> for &T
where
    T: CopyNew,
{
    type StackStorage = MaybeUninit<T>;

    unsafe fn populate_stack_space(self, mut stack: Pin<&mut Option<Self::StackStorage>>) {
        // Safety: we won't move/swap things within the pin.
        let slot = unsafe { Pin::into_inner_unchecked(stack.as_mut()) };
        *slot = Some(MaybeUninit::uninit());
        // Safety: the slot was uninitialized a line ago, which is the storage
        // `New::new` requires, and stays where it is for as long as our caller
        // promised not to move it.
        unsafe { crate::moveit::new::copy(self).new(Pin::new_unchecked(slot.as_mut().unwrap())) }
    }
    unsafe fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut T {
        // Safety: it's OK to (briefly) create a reference to the T because we
        // populated it within `populate_stack_space`. It's OK to unpack the pin
        // because we're not going to move the contents.
        unsafe { Pin::into_inner_unchecked(stack).assume_init_mut() as *mut T }
    }

    unsafe fn do_drop(stack: Pin<&mut Self::StackStorage>) {
        // Switch to MaybeUninit::assume_init_drop when stabilized
        // Safety: per caller guarantees of populate_stack_space, we know this hasn't moved.
        unsafe { std::ptr::drop_in_place(Pin::into_inner_unchecked(stack).assume_init_mut()) };
    }
}

unsafe impl<T> ValueParam<T> for CppPin<T> {
    type StackStorage = CppPin<T>;

    unsafe fn populate_stack_space(self, mut stack: Pin<&mut Option<Self::StackStorage>>) {
        // Safety: we will not move the contents of the pin.
        unsafe { *Pin::into_inner_unchecked(stack.as_mut()) = Some(self) }
    }

    unsafe fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut T {
        // Safety: we won't move/swap the contents of the outer pin. The
        // pointer is to the `T` inside the `CppPin`'s own `Box`, which we now
        // own and keep alive until after the call, so it's non-null, aligned
        // and points to an initialized `T`. It comes from `as_mut_ptr` and not
        // from the `CppMutRef` the `CppPin` caches beside the box, because
        // `CppPin`'s `DerefMut` lets safe code overwrite that cached reference
        // with any pointer at all. No Rust reference to the `T` is created
        // along the way (see the note on `CppPin::as_mut_ptr`), so C++ moving
        // out of it can't collide with Rust's aliasing rules.
        unsafe { Pin::into_inner_unchecked(stack).as_mut_ptr() }
    }
}

unsafe impl<T> ValueParam<T> for CppUniquePtrPin<T>
where
    T: UniquePtrTarget,
{
    type StackStorage = CppUniquePtrPin<T>;

    unsafe fn populate_stack_space(self, mut stack: Pin<&mut Option<Self::StackStorage>>) {
        // Safety: we will not move the contents of the pin.
        unsafe { *Pin::into_inner_unchecked(stack.as_mut()) = Some(self) }
    }

    unsafe fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut T {
        // Safety: we won't move/swap the contents of the outer pin, nor of the
        // type stored within the UniquePtr, which we own and keep alive until
        // after the call. Here the cached `CppMutRef` is the pointer to use:
        // it was taken from the `UniquePtr` on construction, and unlike
        // `CppPin` this type vends no `DerefMut`, so no safe code can have
        // replaced it. Nothing dereferences the `UniquePtr` into a `&T`.
        let ptr = unsafe { Pin::into_inner_unchecked(stack) }
            .as_cpp_mut_ref()
            .as_mut_ptr();
        assert!(
            !ptr.is_null(),
            "Passed a NULL CppUniquePtrPin as a C++ value parameter"
        );
        ptr
    }
}

// A borrowed `CppPin` or a `CppRef` deliberately gets no implementation here,
// though a copy constructor is all it would take. Either would have to build
// its copy out of a Rust `&T`, which is the one thing these types exist to
// avoid: `CppPin::as_ref` is `unsafe` precisely because C++ may hold aliasing
// mutable references to the contents, and a `CppRef` is a raw pointer which
// `CppRef::from_ptr` will make out of anything at all, null included.
// Generated code calls `populate_stack_space` on behalf of safe callers, so an
// implementation here would put that dereference beyond the reach of anybody's
// `unsafe`. Passing by value out of one of these without consuming it
// therefore stays spelled `as_copy(unsafe { pin.as_ref() })`, where the caller
// can see what they are promising.

unsafe impl<T> ValueParam<T> for UniquePtr<T>
where
    T: UniquePtrTarget,
{
    type StackStorage = UniquePtr<T>;

    unsafe fn populate_stack_space(self, mut stack: Pin<&mut Option<Self::StackStorage>>) {
        // Safety: we will not move the contents of the pin.
        unsafe { *Pin::into_inner_unchecked(stack.as_mut()) = Some(self) }
    }

    unsafe fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut T {
        // Safety: we won't move/swap the contents of the outer pin, nor of the
        // type stored within the UniquePtr.
        unsafe {
            (Pin::into_inner_unchecked(
                (*Pin::into_inner_unchecked(stack))
                    .as_mut()
                    .expect("Passed a NULL UniquePtr as a C++ value parameter"),
            )) as *mut T
        }
    }
}

unsafe impl<T> ValueParam<T> for Pin<Box<T>> {
    type StackStorage = Pin<Box<T>>;

    unsafe fn populate_stack_space(self, mut stack: Pin<&mut Option<Self::StackStorage>>) {
        // Safety: we will not move the contents of the pin.
        unsafe { *Pin::into_inner_unchecked(stack.as_mut()) = Some(self) }
    }

    unsafe fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut T {
        // Safety: we won't move/swap the contents of the outer pin, nor of the
        // type stored within the UniquePtr.
        unsafe {
            (Pin::into_inner_unchecked((*Pin::into_inner_unchecked(stack)).as_mut())) as *mut T
        }
    }
}

unsafe impl<'a, T: 'a> ValueParam<T> for &'a UniquePtr<T>
where
    T: UniquePtrTarget + CopyNew,
{
    type StackStorage = <&'a T as ValueParam<T>>::StackStorage;

    unsafe fn populate_stack_space(self, stack: Pin<&mut Option<Self::StackStorage>>) {
        // Safety: the promise this call needs is the one our own caller made.
        unsafe {
            self.as_ref()
                .expect("Passed a NULL &UniquePtr as a C++ value parameter")
                .populate_stack_space(stack)
        }
    }

    unsafe fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut T {
        // Safety: the promise this call needs is the one our own caller made.
        unsafe { <&'a T as ValueParam<T>>::get_ptr(stack) }
    }

    unsafe fn do_drop(stack: Pin<&mut Self::StackStorage>) {
        // Safety: the promise this call needs is the one our own caller made.
        unsafe { <&'a T as ValueParam<T>>::do_drop(stack) }
    }
}

unsafe impl<'a, T: 'a> ValueParam<T> for &'a Pin<Box<T>>
where
    T: CopyNew,
{
    type StackStorage = <&'a T as ValueParam<T>>::StackStorage;

    unsafe fn populate_stack_space(self, stack: Pin<&mut Option<Self::StackStorage>>) {
        // Safety: the promise this call needs is the one our own caller made.
        unsafe { self.as_ref().get_ref().populate_stack_space(stack) }
    }

    unsafe fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut T {
        // Safety: the promise this call needs is the one our own caller made.
        unsafe { <&'a T as ValueParam<T>>::get_ptr(stack) }
    }

    unsafe fn do_drop(stack: Pin<&mut Self::StackStorage>) {
        // Safety: the promise this call needs is the one our own caller made.
        unsafe { <&'a T as ValueParam<T>>::do_drop(stack) }
    }
}

/// Explicitly force a value parameter to be taken using any type of [`crate::moveit::new::New`],
/// i.e. a constructor.
pub fn as_new<N: New>(constructor: N) -> impl ValueParam<N::Output> {
    ByNew(constructor)
}

/// Explicitly force a value parameter to be taken by copy.
pub fn as_copy<P: Deref>(ptr: P) -> impl ValueParam<P::Target>
where
    P::Target: CopyNew,
{
    ByNew(crate::moveit::new::copy(ptr))
}

/// Explicitly force a value parameter to be taken using C++ move semantics.
pub fn as_mov<P: AsMove>(ptr: P) -> impl ValueParam<P::Target>
where
    P::Target: MoveNew,
{
    ByNew(crate::moveit::new::mov(ptr))
}

#[doc(hidden)]
pub struct ByNew<N: New>(N);

unsafe impl<N: New> ValueParam<N::Output> for ByNew<N> {
    type StackStorage = MaybeUninit<N::Output>;

    unsafe fn populate_stack_space(self, mut stack: Pin<&mut Option<Self::StackStorage>>) {
        // Safety: we won't move/swap things within the pin.
        let slot = unsafe { Pin::into_inner_unchecked(stack.as_mut()) };
        *slot = Some(MaybeUninit::uninit());
        // Safety: the slot was uninitialized a line ago, which is the storage
        // `New::new` requires, and stays where it is for as long as our caller
        // promised not to move it.
        unsafe { self.0.new(Pin::new_unchecked(slot.as_mut().unwrap())) }
    }
    unsafe fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut N::Output {
        // Safety: it's OK to (briefly) create a reference to the N::Output because we
        // populated it within `populate_stack_space`. It's OK to unpack the pin
        // because we're not going to move the contents.
        unsafe { Pin::into_inner_unchecked(stack).assume_init_mut() as *mut N::Output }
    }

    unsafe fn do_drop(stack: Pin<&mut Self::StackStorage>) {
        // Switch to MaybeUninit::assume_init_drop when stabilized
        // Safety: per caller guarantees of populate_stack_space, we know this hasn't moved.
        unsafe { std::ptr::drop_in_place(Pin::into_inner_unchecked(stack).assume_init_mut()) };
    }
}

/// Implementation detail for how we pass value parameters into C++.
/// This type is instantiated by auto-generated autocxx code each time we
/// need to pass a value parameter into C++, and will take responsibility
/// for extracting that value parameter from the [`ValueParam`] and doing
/// any later cleanup.
#[doc(hidden)]
pub struct ValueParamHandler<T, VP: ValueParam<T>> {
    // We can't populate this on 'new' because the object may move.
    // Hence this is an Option - it's None until populate is called.
    space: Option<VP::StackStorage>,
    // `space` being `Some` records that storage was reserved; this records
    // that a value was built in it. The two differ when a constructor unwinds.
    populated: bool,
    _pinned: PhantomPinned,
}

impl<T, VP: ValueParam<T>> ValueParamHandler<T, VP> {
    /// Populate this stack space if needs be. Note safety guarantees
    /// on [`get_ptr`].
    ///
    /// # Safety
    ///
    /// Callers must call [`populate`] exactly once prior to calling
    /// [`get_ptr`], and it must have returned: [`get_ptr`] must not be called
    /// after a constructor unwound.
    pub unsafe fn populate(self: Pin<&mut Self>, param: VP) {
        // Safety: `space` is pinned structurally, as documented in
        // [`std::pin`] - this type is `PhantomPinned`, nothing here moves the
        // field after it is populated, and `do_drop` drops it in place - which
        // is the promise `populate_stack_space` asks of us. Being called exactly
        // once is the caller's promise above.
        let this = unsafe { self.get_unchecked_mut() };
        unsafe { param.populate_stack_space(Pin::new_unchecked(&mut this.space)) };
        // Not reached if the constructor unwound.
        this.populated = true;
    }

    /// Return a pointer to the underlying value which can be passed to C++.
    ///
    /// Per the unsafety contract of [`populate`], [`populate`] has been called exactly once
    /// prior to this call.
    pub fn get_ptr(self: Pin<&mut Self>) -> *mut T {
        // Structural pinning, as documented in [`std::pin`]. `map_unchecked_mut` doesn't play
        // nicely with `unwrap`, so we have to do it manually.
        // Safety: `VP::get_ptr` asks that `populate_stack_space` returned for
        // this storage and that it hasn't moved since; both are the promise
        // made by whoever called the unsafe `populate` above.
        unsafe {
            VP::get_ptr(Pin::new_unchecked(
                self.get_unchecked_mut().space.as_mut().unwrap(),
            ))
        }
    }
}

impl<T, VP: ValueParam<T>> Default for ValueParamHandler<T, VP> {
    fn default() -> Self {
        Self {
            space: None,
            populated: false,
            _pinned: PhantomPinned,
        }
    }
}

impl<T, VP: ValueParam<T>> Drop for ValueParamHandler<T, VP> {
    fn drop(&mut self) {
        // `do_drop` is the hand-written destruction of a `MaybeUninit`
        // storage, so it may only run over a value that was built.
        // `populate_stack_space` reserves first and constructs second, and
        // `New::new` promises initialization only when it returns: from out
        // here a `New` which constructed and then unwound - `New::with`'s
        // callback panicking, say - cannot be told from one which never
        // constructed at all. Abandoning the storage loses that object's
        // destructor, which a self-registering C++ object needs; destroying it
        // unconditionally runs a destructor over uninitialized storage on the
        // crate's own null-`UniquePtr` panic, which ordinary safe code
        // reaches. We abandon. The self-dropping storages are freed by the
        // field's own drop glue either way.
        if self.populated {
            if let Some(space) = self.space.as_mut() {
                unsafe { VP::do_drop(Pin::new_unchecked(space)) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cxx::CxxString;

    /// A null [`CppUniquePtrPin`] has no object for C++ to take by value, so
    /// it has to fail as loudly as a null [`UniquePtr`] does rather than hand
    /// C++ a null pointer to move out of.
    #[test]
    #[should_panic(expected = "Passed a NULL CppUniquePtrPin")]
    fn null_cpp_unique_ptr_pin_is_rejected() {
        let pin = CppUniquePtrPin::new(UniquePtr::<CxxString>::null());
        let mut handler = ValueParamHandler::<CxxString, CppUniquePtrPin<CxxString>>::default();
        // Safety: the handler is a local which nothing moves hereafter, and
        // `populate` is called exactly once before `get_ptr`.
        let mut handler = unsafe { Pin::new_unchecked(&mut handler) };
        unsafe { handler.as_mut().populate(pin) };
        let _ = handler.get_ptr();
    }

    /// Drive a handler exactly as generated code does - a local, pinned in
    /// place and then shadowed, so an unwind out of `populate` drops it - and
    /// report whether the constructor unwound.
    fn populate_and_drop<T, VP: ValueParam<T>>(param: VP) -> std::thread::Result<()> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut handler = ValueParamHandler::<T, VP>::default();
            // Safety: the handler is a local which nothing moves hereafter,
            // and `populate` is called exactly once.
            let handler = unsafe { Pin::new_unchecked(&mut handler) };
            unsafe { handler.populate(param) };
        }))
    }

    static BY_NEW_DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Zero-sized, so a destructor run over storage which was never
    /// constructed reads nothing and the miscount is the only symptom.
    struct CountsItsDrops;

    impl Drop for CountsItsDrops {
        fn drop(&mut self) {
            BY_NEW_DROPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    static COPY_DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// As above, with a copy constructor which fails the way a C++ one would.
    struct FailsToCopy;

    impl Drop for FailsToCopy {
        fn drop(&mut self) {
            COPY_DROPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    unsafe impl CopyNew for FailsToCopy {
        unsafe fn copy_new(_src: &Self, _this: Pin<&mut MaybeUninit<Self>>) {
            panic!("copy constructor failed")
        }
    }

    /// A constructor which unwinds leaves the reserved stack storage empty, so
    /// the handler must not destroy what was never built.
    #[test]
    fn unwinding_new_leaves_nothing_to_destroy() {
        BY_NEW_DROPS.store(0, std::sync::atomic::Ordering::SeqCst);
        let outcome = populate_and_drop::<CountsItsDrops, _>(ByNew(crate::moveit::new::by(
            || -> CountsItsDrops { panic!("constructor failed") },
        )));
        assert!(outcome.is_err());
        assert_eq!(
            BY_NEW_DROPS.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "destructor ran over storage which was reserved but never constructed"
        );
    }

    /// The same for the copy-constructing implementation for `&T`.
    #[test]
    fn unwinding_copy_leaves_nothing_to_destroy() {
        COPY_DROPS.store(0, std::sync::atomic::Ordering::SeqCst);
        let src = FailsToCopy;
        let outcome = populate_and_drop::<FailsToCopy, &FailsToCopy>(&src);
        assert!(outcome.is_err());
        assert_eq!(
            COPY_DROPS.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "destructor ran over storage which was reserved but never constructed"
        );
    }
}
