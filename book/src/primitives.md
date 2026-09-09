# Built-in types

`autocxx` relies primarily on the [standard cxx types](https://cxx.rs/bindings.html).
In particular you should become familiar with [`cxx::UniquePtr`](https://docs.rs/cxx/latest/cxx/struct.UniquePtr.html) and [`cxx::CxxString`](https://docs.rs/cxx/latest/cxx/struct.CxxString.html).

There are a few additional integer types, such as [`c_int`](https://docs.rs/autocxx/latest/autocxx/struct.c_int.html),
which are not yet upstreamed to `cxx`. These are to support those pesky C/C++ integer types
which do not have a predictable number of bits on different machines.

```rust,ignore,autocxx
autocxx_integration_tests::doctest(
"",
"inline int do_math(int a, int b) { return a+b; }",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("do_math")
}

fn main() {
    assert_eq!(ffi::do_math(c_int(12), c_int(13)), c_int(25));
}
}
)
```

### Inside a `unique_ptr`

`cxx` will not put one of its own integer types inside a `UniquePtr`, so a
`std::unique_ptr<uint32_t>` arrives as a `UniquePtr<autocxx::c_u32>` rather
than a `UniquePtr<u32>`. There is one of these wrappers per fixed width -
`c_u8` through `c_u64` and `c_i8` through `c_i64` - and each is a transparent
newtype over the Rust integer of that width, so `.0` or `.into()` gets the
value back. Anywhere else - by value, in a `std::vector`, in a
`std::shared_ptr` - a `uint32_t` is a plain `u32` as before.

## Character types

C++'s `char16_t`, `char32_t`, `char8_t` and `wchar_t` are distinct types - a
`char32_t` is not a `uint32_t`, and a C++ compiler asked for the exact type of a
function says so - and Rust has no equivalent of any of them. `autocxx` gives
each one a transparent newtype:
[`c_char16_t`](https://docs.rs/autocxx/latest/autocxx/struct.c_char16_t.html),
[`c_char32_t`](https://docs.rs/autocxx/latest/autocxx/struct.c_char32_t.html),
[`c_char8_t`](https://docs.rs/autocxx/latest/autocxx/struct.c_char8_t.html) and
[`c_wchar_t`](https://docs.rs/autocxx/latest/autocxx/struct.c_wchar_t.html), each
wrapping the Rust integer of the same width, so a value crosses with `.0` or
`From`.

`wchar_t` is the one whose width and signedness are the target's to choose -
`unsigned short` on Windows, `int` on Linux and macOS, `unsigned int` under
AAPCS - so its payload is
[`autocxx::wchar_t`](https://docs.rs/autocxx/latest/autocxx/type.wchar_t.html),
which is picked per target. `char8_t` only exists from C++20 onwards.

`char16_t`, `char32_t` and `wchar_t` go inside a `UniquePtr`, a `SharedPtr`, a
`WeakPtr` and a `CxxVector` as their newtypes. `char8_t` does not: naming it
takes a `typedef char8_t ...` in C++ which `autocxx` compiles at C++14, where
the keyword does not exist, so a container of one is refused with the rest of
that function left alone.

`long double` has no such newtype and is not supported, for a reason which
differs by target: on MSVC and Apple Arm it is a `double` under another name, so
a Rust `f64` has the right layout but is still the wrong C++ type and `cxx`'s
signature check says so; on x86-64 Linux it is an 80-bit x87 float in 16 bytes,
and on AArch64 Linux an IEEE binary128, and Rust has no type for either. A
function whose signature mentions one is refused with an error saying that. A
`long double` *field* is fine - those bytes are carried around, not passed
between the languages - though a struct with one cannot be `generate_pod!`.

### 128-bit integers

A C++ `__int128` is [`c_i128`](https://docs.rs/autocxx/latest/autocxx/struct.c_i128.html)
and an `unsigned __int128` is
[`c_u128`](https://docs.rs/autocxx/latest/autocxx/struct.c_u128.html), both
transparent newtypes. Neither may go inside a `UniquePtr` or a `CxxVector`,
because MSVC has neither type and the glue which would make that work is
compiled everywhere.

`__float128` is not supported. `bindgen` renders it as a `u128` because that is
the right size and Rust has no 128-bit float, which is also how an
`unsigned __int128` arrives - so `autocxx` has `bindgen` mark the `__float128`
and refuses a function which mentions one by name, rather than binding it as the
integer beside it.

## Strings

`autocxx` uses [`cxx::CxxString`](https://docs.rs/cxx/latest/cxx/struct.CxxString.html). However, as noted above, we can't
just pass a C++ string by value, so we'll box and unbox it automatically
such that you're really dealing with `UniquePtr<CxxString>` on the Rust
side, even if the API just took or returned a plain old `std::string`.

However, to ease ergonomics, functions that accept a `std::string` will
actually accept anything that
implements a trait called `ffi::ToCppString`. That may either be a
`UniquePtr<CxxString>` or just a plain old Rust string - which will be
converted transparently to a C++ string.

This trait, and its implementations, are not present in the `autocxx`
documentation because they're dynamically generated in _your_ code
so that they can call through to a `make_string` implementation in
the C++ that we're injecting into your C++ build system.

(None of that happens if you use [`exclude_utilities`](https://docs.rs/autocxx/latest/autocxx/macro.exclude_utilities.html), so don't do that.)

```rust,ignore,autocxx
autocxx_integration_tests::doctest(
"",
"#include <string>
#include <cstdint>
inline uint32_t take_string(std::string a) { return static_cast<uint32_t>(a.size()); }",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("take_string")
}

fn main() {
    assert_eq!(ffi::take_string("hello"), 5)
}
}
)
```

If you need to create a blank `UniquePtr<CxxString>` in Rust, such that
(for example) you can pass its mutable reference or pointer into some
pre-existing C++ API, call `ffi::make_string("")` which will return
a blank `UniquePtr<CxxString>`.

If all you need is a _reference_ to a `CxxString`, you can alternatively use
[`cxx::let_cxx_string`](https://docs.rs/cxx/latest/cxx/macro.let_cxx_string.html).

## String views

A function which takes a `std::string_view` accepts anything implementing
`ffi::AsCppStringView` — a `&str`, a `String`, a `&[u8]`, a `Vec<u8>`, or a
`&CxxString`. Nothing is copied: the view is built in C++ over the bytes you
lend, for the duration of the call. A `const std::string_view&` parameter is the
same thing and takes the same argument — note that this is so even under
`safety!(unsafe_references_wrapped)`, where other reference parameters become
`CppRef` wrappers; a wrapper around a type Rust cannot construct would be no use
to anybody.

The bytes are lent for the call, and nothing promises they outlast it. C++ which
copies the view somewhere that outlives the call may be left holding a dangling
one — whether it is depends on what you passed, and passing a borrowed `&[u8]`
promises nothing beyond the call. It is the same bargain as letting C++ keep the
`const char*` out of a `const std::string&` parameter, except that `string_view`
is a type people do store. autocxx cannot see that happen, so it falls under what
you vouch for with `safety!`. If the C++ keeps what it is lent, give it an owned
`std::string` parameter instead.

The bytes are bytes. C++ asks nothing about the encoding of a `string_view`
and neither does this, which is why `&[u8]` is accepted alongside `&str` and
why a view may contain an interior NUL.

```rust,ignore,autocxx,cpp17
autocxx_integration_tests::doctest(
"",
"#include <string_view>
#include <cstddef>
inline size_t take_view(std::string_view v) { return v.size(); }",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("take_view")
}

fn main() {
    assert_eq!(ffi::take_view("hello"), 5);
    assert_eq!(ffi::take_view(&b"ab\0c"[..]), 4);
}
}
)
```

A `std::string_view` is never handed back the other way — not as a return value,
not read out of a C++ variable, not as a `unique_ptr` or `shared_ptr` payload.
Rust has no type which is a `std::string_view`, because a view borrows characters
something else owns and `autocxx` has nothing to tie that borrow to whose
lifetime it could check. Every such position is refused with an explanation
rather than bound unsafely. Hand over an owned `std::string` instead, which
arrives as a `UniquePtr<CxxString>`.

For the same reason, a `std::string_view` parameter of a `virtual` method can be
*called* from Rust but not *overridden* from Rust with `subclass!`: the way in
builds the view, and the way out would have to hand Rust an unchecked borrow.

`std::string_view` is C++17. `autocxx` is told the C++ standard twice — once
for parsing your headers and once for compiling the code it generates — so
remember to set it in both places, e.g. `cc::Build::std("c++17")` beside
`extra_clang_args(["-std=c++17"])`. Generated code which needs C++17 and does
not get it says so by name.
