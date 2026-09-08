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
/// The fallbacks name the *unaligned* global overloads, so a type whose
/// alignment exceeds `__STDCPP_DEFAULT_NEW_ALIGNMENT__` gets storage which
/// does not satisfy it - which a `new T` expression would not, since C++17
/// routes those to the `std::align_val_t` overloads. Reproduced on macOS with
/// `class alignas(64) Wide { virtual ~Wide(); uint64_t a; };`: the object
/// lands on a 16-aligned address and Rust's own debug assertion aborts on the
/// first dereference. Every `within_unique_ptr` goes through here, so this is
/// not particular to any one kind of type. Fixing it means calling the aligned
/// overloads under `#if __cpp_aligned_new`, matched on both sides; the pair
/// must stay matched, because C++ requires an aligned allocation to be freed
/// by an aligned deallocation.
pub(super) static NEW_AND_DELETE_PRELUDE: &str = indoc! {"
    #ifndef AUTOCXX_NEW_AND_DELETE_PRELUDE
    #define AUTOCXX_NEW_AND_DELETE_PRELUDE
    // Mechanics to call custom operator new and delete
    template <typename T>
    auto delete_imp(T *ptr, int) -> decltype((void)T::operator delete(ptr)) {
      T::operator delete(ptr);
    }
    template <typename T> void delete_imp(T *ptr, long) { ::operator delete(ptr); }
    template <typename T> void delete_appropriately(T *obj) {
      // 0 is a better match for the first 'delete_imp' so will match
      // preferentially.
      delete_imp(obj, 0);
    }
    template <typename T>
    auto new_imp(size_t count, int) -> decltype(T::operator new(count)) {
      return T::operator new(count);
    }
    template <typename T> void *new_imp(size_t count, long) {
      return ::operator new(count);
    }
    template <typename T> T *new_appropriately() {
      // 0 is a better match for the first 'delete_imp' so will match
      // preferentially.
      return static_cast<T *>(new_imp<T>(sizeof(T), 0));
    }
    #endif // AUTOCXX_NEW_AND_DELETE_PRELUDE
"};
