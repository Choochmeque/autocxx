// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indoc::indoc;

/// This is logic to call either an overloaded operator new/delete
/// or the standard one.
/// The SFINAE magic here is: int is a better match than long,
/// and so the versions which match class-specific operator new/delete
/// will be used in preference to the general global ::operator new/delete.
///
/// The *global* fallbacks are chosen by alignment, because C++17 routes
/// `new T` for an over-aligned type to the `std::align_val_t` overloads while
/// the plain ones promise only `__STDCPP_DEFAULT_NEW_ALIGNMENT__`. The
/// symptom before that, on macOS with
/// `class alignas(64) Wide { virtual ~Wide(); uint64_t a; };`: the object
/// landed on a 16-aligned address and Rust's own debug assertion aborted on
/// the first dereference. Every `within_unique_ptr` comes through here, and
/// `moveit` turns the pointer straight into a `&mut MaybeUninit<T>`, so the
/// misalignment is undefined behaviour on the Rust side too. The aligned
/// deallocation goes only with the aligned allocation, so a block from the
/// class's own `operator new` is never handed to the aligned global.
///
/// The *class* half is deliberately not widened. The probes ask only what
/// this header has always asked - `T::operator new(size_t)` and
/// `T::operator delete(void*)` - because a probe for the `std::align_val_t`
/// or `std::size_t` overloads also matches a *placement* deallocation
/// function, which `delete p` correctly passes over and which frees nothing:
/// a class with `template <class A> operator delete(void*, A)` answers every
/// such probe. Expression SFINAE cannot tell a usual deallocation function
/// from a placement one.
///
/// So this does not reach a class whose only deallocation function is the
/// sized form, the aligned form, or C++20's destroying form; that storage
/// goes back to the global `operator delete`, which is wrong if the class
/// allocated from an arena. That gap predates this and is unchanged by it: an
/// exhaustive matrix over the class-scoped allocation and deallocation
/// functions, under clang and gcc, finds no shape whose selected functions
/// this gets more wrong than before, and none where its own allocation and
/// deallocation disagree about alignment.
///
/// Three shapes are refused instead, each one a pairing this cannot make:
/// an over-aligned type before C++17, which has no aligned global to fall
/// back to at all; a class whose only allocation function takes an alignment,
/// leaving nothing to say which deallocation matches it; and an over-aligned
/// type allocated from the aligned global whose class declares an
/// `operator delete` but no `operator new`, where C++ would hand that
/// deallocation function an aligned block and this will not do so behind its
/// author's back. `autocxx_has_aligned_new` can be led into the second by a
/// catch-all placement `operator new` template, which costs a build error and
/// never a wrong free.
pub(super) static NEW_AND_DELETE_PRELUDE: &str = indoc! {"
    #ifndef AUTOCXX_NEW_AND_DELETE_PRELUDE
    #define AUTOCXX_NEW_AND_DELETE_PRELUDE
    // Mechanics to call custom operator new and delete

    // The alignment a plain `::operator new` promises. C++17 names it; before
    // that the guarantee is whatever `std::max_align_t` needs.
    #if defined(__cpp_aligned_new)
    #define AUTOCXX_DEFAULT_NEW_ALIGNMENT __STDCPP_DEFAULT_NEW_ALIGNMENT__
    #else
    #define AUTOCXX_DEFAULT_NEW_ALIGNMENT alignof(::std::max_align_t)
    #endif
    template <typename T>
    struct autocxx_over_aligned
        : ::std::integral_constant<bool,
                                   (alignof(T) > AUTOCXX_DEFAULT_NEW_ALIGNMENT)> {
    };
    #undef AUTOCXX_DEFAULT_NEW_ALIGNMENT

    template <typename...> struct autocxx_void { typedef void type; };

    // Whether the class declares each of the two functions asked for below.
    // The overloads are selected on these same traits rather than on a repeat
    // of the expression, so that what gets called and what the rest of this
    // believes was called cannot come apart.
    template <typename T, typename = void>
    struct autocxx_has_plain_new : ::std::false_type {};
    template <typename T>
    struct autocxx_has_plain_new<
        T, typename autocxx_void<decltype(T::operator new(sizeof(T)))>::type>
        : ::std::true_type {};
    template <typename T, typename = void>
    struct autocxx_has_plain_delete : ::std::false_type {};
    template <typename T>
    struct autocxx_has_plain_delete<
        T, typename autocxx_void<decltype(T::operator delete(
               static_cast<T *>(nullptr)))>::type> : ::std::true_type {};

    #if defined(__cpp_aligned_new)
    // Only ever used to refuse; a catch-all placement template answers yes
    // and costs a build error rather than a mismatched free.
    template <typename T, typename = void>
    struct autocxx_has_aligned_new : ::std::false_type {};
    template <typename T>
    struct autocxx_has_aligned_new<
        T, typename autocxx_void<decltype(T::operator new(
               sizeof(T), ::std::align_val_t(alignof(T))))>::type>
        : ::std::true_type {};

    // Whether allocation goes to the global aligned form, which is the only
    // case in which deallocation may go to the global aligned form too.
    template <typename T>
    struct autocxx_uses_aligned_global
        : ::std::integral_constant<bool, autocxx_over_aligned<T>::value &&
                                             !autocxx_has_plain_new<T>::value> {
    };
    #endif

    #if defined(__cpp_aligned_new)
    // The global halves, which are the only ones alignment gets a say in.
    template <typename T>
    void autocxx_delete_globally(T *ptr, ::std::true_type) {
      ::operator delete(ptr, ::std::align_val_t(alignof(T)));
    }
    template <typename T>
    void autocxx_delete_globally(T *ptr, ::std::false_type) {
      ::operator delete(ptr);
    }
    template <typename T>
    void *autocxx_new_globally(::std::size_t count, ::std::true_type) {
      return ::operator new(count, ::std::align_val_t(alignof(T)));
    }
    template <typename T>
    void *autocxx_new_globally(::std::size_t count, ::std::false_type) {
      return ::operator new(count);
    }
    #endif

    template <typename T>
    typename ::std::enable_if<autocxx_has_plain_delete<T>::value>::type
    delete_imp(T *ptr, int) {
      T::operator delete(ptr);
    }
    template <typename T> void delete_imp(T *ptr, long) {
    #if defined(__cpp_aligned_new)
      autocxx_delete_globally(ptr, typename autocxx_uses_aligned_global<T>::type{});
    #else
      ::operator delete(ptr);
    #endif
    }
    template <typename T> void delete_appropriately(T *obj) {
    #if defined(__cpp_aligned_new)
      // The class's own deallocation function is reached only through the
      // probe above, which asks for `operator delete(void*)`. Where the
      // allocation came from the aligned global instead, that function is
      // being handed a block it may free with the plain `::operator delete` -
      // which C++ permits the class to be wrong about, but which this will
      // not do behind its author's back.
      static_assert(!(autocxx_uses_aligned_global<T>::value &&
                      autocxx_has_plain_delete<T>::value),
                    \"this type needs an over-aligned allocation, and its \"
                    \"class declares an operator delete which would be asked \"
                    \"to free one without an operator new to match - give the \"
                    \"class an operator new(std::size_t) as well\");
    #endif
      // 0 is a better match for the first 'delete_imp' so will match
      // preferentially.
      delete_imp(obj, 0);
    }

    template <typename T>
    typename ::std::enable_if<autocxx_has_plain_new<T>::value, void *>::type
    new_imp(::std::size_t count, int) {
      return T::operator new(count);
    }
    template <typename T> void *new_imp(::std::size_t count, long) {
    #if defined(__cpp_aligned_new)
      static_assert(!autocxx_has_aligned_new<T>::value,
                    \"this class declares an operator new taking an alignment \"
                    \"and none taking a size alone, and autocxx cannot tell \"
                    \"which operator delete would match it - give the class an \"
                    \"operator new(std::size_t)\");
      return autocxx_new_globally<T>(
          count, typename autocxx_uses_aligned_global<T>::type{});
    #else
      // Before C++17 there is no aligned `::operator new` to reach for, and
      // no way to invent one: whatever this returned would later be freed by
      // `delete p`, which calls `::operator delete`. A class with an
      // allocation function of its own is answered above and never arrives
      // here.
      static_assert(
          !autocxx_over_aligned<T>::value,
          \"this type wants more alignment than operator new gives, and only \"
          \"C++17's aligned new can provide it - compile as C++17 or later, or \"
          \"give the class an operator new(std::size_t) of its own\");
      return ::operator new(count);
    #endif
    }

    template <typename T> T *new_appropriately() {
      // 0 is a better match for the first 'new_imp' so will match
      // preferentially.
      void *storage = new_imp<T>(sizeof(T), 0);
      // A class may declare a `noexcept` operator new, which reports failure
      // by returning null. `new T` would then construct nothing; this has no
      // way to say so - its caller turns the pointer straight into a Rust
      // reference - so stop here rather than hand back a null one.
      if (storage == nullptr) {
        ::std::terminate();
      }
      return static_cast<T *>(storage);
    }
    #endif // AUTOCXX_NEW_AND_DELETE_PRELUDE
"};
