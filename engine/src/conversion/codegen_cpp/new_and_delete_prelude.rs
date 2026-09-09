// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indoc::indoc;

/// Allocation and deallocation for storage which holds no object: the block
/// `moveit` hands to a constructor, and the block it gives back when a
/// constructor fails or when the caller moves out of a `UniquePtr`.
///
/// Both halves have to agree with the C++ that surrounds them, and there are
/// two partners to agree with. `new_appropriately` allocates the block whose
/// eventual `delete p` - inside cxx's `std::unique_ptr` - the class author
/// chose their allocation functions for, so it calls the function the
/// *new-expression* would have called. `delete_appropriately` frees a block
/// which never became an object, so it calls the deallocation half of the
/// *delete-expression* without the destructor call. Selecting anything else
/// pairs one allocator's block with another's free.
///
/// The selection rules, which are C++'s own and are measured rather than
/// assumed:
///
/// * A new-expression for an over-aligned type passes `std::align_val_t` and
///   drops it again if the class has no overload taking one, so the class's
///   aligned `operator new` wins where it exists and the plain one otherwise -
///   here, where the class owns the deallocation as well, since otherwise the
///   block would go to a global chosen by alignment alone.
/// * A delete-expression for an over-aligned type prefers an alignment-aware
///   deallocation function, and class scope then takes the unsized one of
///   whatever is left - so `operator delete(void*)` beats
///   `operator delete(void*, size_t)`, and a class whose only deallocation
///   function is the sized form gets that one.
/// * The *global* fallbacks are chosen by alignment for the same reason:
///   `new T` for an over-aligned type routes to the `std::align_val_t`
///   overloads while the plain ones promise only
///   `__STDCPP_DEFAULT_NEW_ALIGNMENT__`. The symptom before that, on macOS
///   with `class alignas(64) Wide { virtual ~Wide(); uint64_t a; };`: the
///   object landed on a 16-aligned address and Rust's own debug assertion
///   aborted on the first dereference. Every `within_unique_ptr` comes
///   through here, and `moveit` turns the pointer straight into a
///   `&mut MaybeUninit<T>`, so the misalignment is undefined behaviour on the
///   Rust side too. The aligned deallocation goes only with the aligned
///   allocation, so a block from the class's own `operator new` is never
///   handed to the aligned global.
///
/// Before C++17 there is no `std::align_val_t` for any of this to ask about:
/// the class's own functions are selected exactly as above, and an
/// over-aligned type with none of its own is refused, because the aligned
/// global it would need does not exist to be called.
///
/// The probes take the two expressions' own shapes. A new-expression picks
/// its allocation function by overload resolution over exactly the arguments
/// it passes, so the allocation probes are calls and a conversion counts. A
/// delete-expression does no overload resolution at all: it takes the *usual*
/// deallocation functions, the ones whose parameters after the first are
/// exactly `std::size_t`, `std::align_val_t` or both, so the deallocation
/// probes name a signature. A call there would let
/// `operator delete(void*, int)` answer for the sized form and free nothing.
///
/// The deallocation function is then reached through the very pointer its
/// probe formed, which does two things. It cannot go ambiguous where the
/// probe did not, and where a class declares both a real function and an
/// `operator delete` *template* at the same signature it takes the real one -
/// the same preference a delete-expression has, which passes over every
/// template because a template instance is never a usual deallocation
/// function.
///
/// The hole that leaves is a class whose *only* function at a usual signature
/// is a template, whether that is a catch-all `template <class A>
/// operator delete(void*, A)` or one written at the signature itself. Nothing
/// expression SFINAE can ask separates it from a real one, so it is called
/// where a delete-expression would have passed over it and taken another
/// function of the class. That costs a leak, and there is no reading of such
/// a class this could get right.
///
/// Three shapes are refused, each one a pairing this cannot make: an
/// over-aligned type before C++17; a class whose only allocation function
/// takes an alignment and which does not own its deallocation, leaving
/// nothing to say what the block is freed with; and an over-aligned type
/// allocated from the aligned global whose class declares a deallocation
/// function that is not alignment-aware, where C++ would hand that function
/// an aligned block and this will not do so behind its author's back.
///
/// Two systematic differences from a delete-expression remain. The global sized
/// deallocation functions are never called - which of the two a
/// delete-expression picks is decided by the compiler's
/// `-fsized-deallocation`, and the default sized one is defined to call the
/// unsized one. And an over-aligned type whose class declares an
/// `operator new` but no `operator delete` is left alone: C++ pairs that
/// class's own allocation with the *global aligned* deallocation, a mismatch
/// no choice here can repair, and the pairing this makes instead is the one
/// it has always made.
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

    // Which functions the class declares. The selections below are made on
    // these traits rather than on a repeat of the expression, so that what
    // gets called and what the rest of this believes was called cannot come
    // apart.
    //
    // The allocation probes are calls, because a new-expression picks its
    // allocation function by overload resolution over exactly those arguments
    // and a conversion is fair game. The deallocation probes name a signature
    // instead, because a delete-expression does not do overload resolution:
    // it takes the *usual* deallocation functions, which are the ones whose
    // parameters after the first are exactly `std::size_t`, `std::align_val_t`
    // or both, and a call would let `operator delete(void*, int)` answer for
    // the sized form and free nothing.
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
    template <typename T, typename = void>
    struct autocxx_has_sized_delete : ::std::false_type {};
    template <typename T>
    struct autocxx_has_sized_delete<
        T, typename autocxx_void<decltype(static_cast<void (*)(
               void *, ::std::size_t)>(&T::operator delete))>::type>
        : ::std::true_type {};

    #if defined(__cpp_aligned_new)
    template <typename T, typename = void>
    struct autocxx_has_aligned_new : ::std::false_type {};
    template <typename T>
    struct autocxx_has_aligned_new<
        T, typename autocxx_void<decltype(T::operator new(
               sizeof(T), ::std::align_val_t(alignof(T))))>::type>
        : ::std::true_type {};
    template <typename T, typename = void>
    struct autocxx_has_aligned_delete : ::std::false_type {};
    template <typename T>
    struct autocxx_has_aligned_delete<
        T, typename autocxx_void<decltype(static_cast<void (*)(
               void *, ::std::align_val_t)>(&T::operator delete))>::type>
        : ::std::true_type {};
    template <typename T, typename = void>
    struct autocxx_has_sized_aligned_delete : ::std::false_type {};
    template <typename T>
    struct autocxx_has_sized_aligned_delete<
        T, typename autocxx_void<decltype(static_cast<void (*)(
               void *, ::std::size_t, ::std::align_val_t)>(
               &T::operator delete))>::type> : ::std::true_type {};
    #else
    // Before C++17 there are no aligned forms to find, so the selections
    // below read the same at every standard.
    template <typename T, typename = void>
    struct autocxx_has_aligned_new : ::std::false_type {};
    template <typename T, typename = void>
    struct autocxx_has_aligned_delete : ::std::false_type {};
    template <typename T, typename = void>
    struct autocxx_has_sized_aligned_delete : ::std::false_type {};
    #endif

    // Which deallocation function a delete-expression would select.
    // An over-aligned type takes an alignment-aware candidate and any other
    // type takes one which is not - each only where such a candidate exists,
    // because the preference eliminates nothing when nothing is preferred.
    template <typename T>
    struct autocxx_deletes_alignment_aware
        : ::std::integral_constant<
              bool, (autocxx_has_aligned_delete<T>::value ||
                     autocxx_has_sized_aligned_delete<T>::value) &&
                        (autocxx_over_aligned<T>::value ||
                         !(autocxx_has_plain_delete<T>::value ||
                           autocxx_has_sized_delete<T>::value))> {};

    // Class scope then takes the one without a `std::size_t`.
    template <typename T>
    struct autocxx_deletes_via_class_aligned
        : ::std::integral_constant<bool,
                                   autocxx_deletes_alignment_aware<T>::value &&
                                       autocxx_has_aligned_delete<T>::value> {
    };
    template <typename T>
    struct autocxx_deletes_via_class_sized_aligned
        : ::std::integral_constant<
              bool, autocxx_deletes_alignment_aware<T>::value &&
                        !autocxx_has_aligned_delete<T>::value &&
                        autocxx_has_sized_aligned_delete<T>::value> {};
    template <typename T>
    struct autocxx_deletes_via_class_plain
        : ::std::integral_constant<
              bool, !autocxx_deletes_alignment_aware<T>::value &&
                        autocxx_has_plain_delete<T>::value> {};
    template <typename T>
    struct autocxx_deletes_via_class_sized
        : ::std::integral_constant<
              bool, !autocxx_deletes_alignment_aware<T>::value &&
                        !autocxx_has_plain_delete<T>::value &&
                        autocxx_has_sized_delete<T>::value> {};
    template <typename T>
    struct autocxx_deletes_globally
        : ::std::integral_constant<
              bool, !autocxx_deletes_via_class_aligned<T>::value &&
                        !autocxx_deletes_via_class_sized_aligned<T>::value &&
                        !autocxx_deletes_via_class_plain<T>::value &&
                        !autocxx_deletes_via_class_sized<T>::value> {};

    // Which allocation function a new-expression would call. It passes an
    // alignment for an over-aligned type and removes it again if nothing
    // matches, so the class's aligned form wins where the class has one and
    // its plain form otherwise - but only where the class owns the
    // deallocation too. A block from the class paired with the global
    // deallocation is the mismatch the paragraph below refuses, and taking
    // the aligned form would newly create one.
    template <typename T>
    struct autocxx_news_via_class_aligned
        : ::std::integral_constant<bool,
                                   autocxx_over_aligned<T>::value &&
                                       autocxx_has_aligned_new<T>::value &&
                                       !autocxx_deletes_globally<T>::value> {};
    template <typename T>
    struct autocxx_news_via_class_plain
        : ::std::integral_constant<
              bool, autocxx_has_plain_new<T>::value &&
                        !autocxx_news_via_class_aligned<T>::value> {};
    template <typename T>
    struct autocxx_news_globally
        : ::std::integral_constant<
              bool, !autocxx_has_plain_new<T>::value &&
                        !autocxx_news_via_class_aligned<T>::value> {};
    // Whether allocation goes to the global aligned form, which is the only
    // case in which deallocation may go to the global aligned form too.
    template <typename T>
    struct autocxx_uses_aligned_global
        : ::std::integral_constant<bool, autocxx_over_aligned<T>::value &&
                                             autocxx_news_globally<T>::value> {};

    #if defined(__cpp_aligned_new)
    // The global halves, whose alignment follows the allocation's.
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

    // One of these is enabled for any T, and never more than one.
    //
    // Each deallocation function is reached through the pointer its probe
    // formed rather than by writing the call again: a second overload can
    // make the written call ambiguous where the probe was not, and what gets
    // called is then certainly the function the selection read.
    #if defined(__cpp_aligned_new)
    template <typename T>
    typename ::std::enable_if<autocxx_deletes_via_class_aligned<T>::value>::type
    delete_imp(T *ptr) {
      static_cast<void (*)(void *, ::std::align_val_t)>(&T::operator delete)(
          ptr, ::std::align_val_t(alignof(T)));
    }
    template <typename T>
    typename ::std::enable_if<
        autocxx_deletes_via_class_sized_aligned<T>::value>::type
    delete_imp(T *ptr) {
      static_cast<void (*)(void *, ::std::size_t, ::std::align_val_t)>(
          &T::operator delete)(ptr, sizeof(T),
                               ::std::align_val_t(alignof(T)));
    }
    #endif
    template <typename T>
    typename ::std::enable_if<autocxx_deletes_via_class_plain<T>::value>::type
    delete_imp(T *ptr) {
      T::operator delete(ptr);
    }
    template <typename T>
    typename ::std::enable_if<autocxx_deletes_via_class_sized<T>::value>::type
    delete_imp(T *ptr) {
      static_cast<void (*)(void *, ::std::size_t)>(&T::operator delete)(
          ptr, sizeof(T));
    }
    template <typename T>
    typename ::std::enable_if<autocxx_deletes_globally<T>::value>::type
    delete_imp(T *ptr) {
    #if defined(__cpp_aligned_new)
      autocxx_delete_globally(ptr, typename autocxx_uses_aligned_global<T>::type{});
    #else
      ::operator delete(ptr);
    #endif
    }

    template <typename T> void delete_appropriately(T *obj) {
    #if defined(__cpp_aligned_new)
      // Where the allocation came from the aligned global, a deallocation
      // function of the class's own is being handed a block it may free with
      // the plain `::operator delete` - which C++ permits the class to be
      // wrong about, but which this will not do behind its author's back.
      static_assert(!(autocxx_uses_aligned_global<T>::value &&
                      (autocxx_deletes_via_class_plain<T>::value ||
                       autocxx_deletes_via_class_sized<T>::value)),
                    \"this type needs an over-aligned allocation, and its \"
                    \"class declares an operator delete which would be asked \"
                    \"to free one without an operator new to match - give the \"
                    \"class an operator new(std::size_t) as well\");
    #endif
      delete_imp(obj);
    }

    #if defined(__cpp_aligned_new)
    template <typename T>
    typename ::std::enable_if<autocxx_news_via_class_aligned<T>::value,
                              void *>::type
    new_imp(::std::size_t count) {
      return T::operator new(count, ::std::align_val_t(alignof(T)));
    }
    #endif
    template <typename T>
    typename ::std::enable_if<autocxx_news_via_class_plain<T>::value, void *>::type
    new_imp(::std::size_t count) {
      return T::operator new(count);
    }
    template <typename T>
    typename ::std::enable_if<autocxx_news_globally<T>::value, void *>::type
    new_imp(::std::size_t count) {
    #if defined(__cpp_aligned_new)
      // Reached with an aligned operator new only where the type is not
      // over-aligned, so a new-expression passes no alignment - and clang and
      // gcc both find nothing to call for one.
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
      void *storage = new_imp<T>(sizeof(T));
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
