// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indoc::indoc;

/// A replacement for `std::move` used when handing a C++ function a parameter
/// by value out of storage which Rust owns and will destroy after the call.
///
/// Moving out of that storage is what we want: nobody will look at it again,
/// and it saves a copy. But `std::move` asks for the move constructor
/// specifically, and overload resolution prefers a *deleted* move constructor
/// to a perfectly good copy constructor - so `std::move` refuses to compile for
/// the many real C++ types which write `T(T&&) = delete;` alongside a working
/// `T(const T&)`. It also refuses for a type whose only copy constructor takes
/// `T&`, which an rvalue can't bind to at all. This hands the parameter over
/// as whichever of `T&&`, `const T&` and `T&` the type can be built from, in
/// that order: move if C++ lets us, otherwise copy without disturbing what
/// we're copying from, and only then offer the mutable lvalue which a
/// `T(T&)` demands. See <https://github.com/google/autocxx/issues/873>.
///
/// Callers must write `::autocxx_move_or_copy`: the argument brings its own
/// namespaces into the overload set, and a same-named function in any of them
/// would be a better match than this template.
pub(super) static MOVE_OR_COPY_PRELUDE: &str = indoc! {"
    #ifndef AUTOCXX_MOVE_OR_COPY_PRELUDE
    #define AUTOCXX_MOVE_OR_COPY_PRELUDE
    // Hand over a parameter as an rvalue if the type can be built from one,
    // and as an lvalue - const if that will do, mutable if it won't - if not.
    template <typename T>
    using autocxx_move_or_copy_t = typename std::conditional<
        std::is_constructible<T, T&&>::value, T&&,
        typename std::conditional<std::is_constructible<T, const T&>::value,
                                  const T&, T&>::type>::type;
    template <typename T> autocxx_move_or_copy_t<T> autocxx_move_or_copy(T& t) {
      return static_cast<autocxx_move_or_copy_t<T>>(t);
    }
    #endif // AUTOCXX_MOVE_OR_COPY_PRELUDE
"};
