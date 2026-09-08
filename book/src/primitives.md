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

`long double` has no such newtype and is not supported, for a reason which
differs by target: on MSVC and Apple Arm it is a `double` under another name, so
a Rust `f64` has the right layout but is still the wrong C++ type and `cxx`'s
signature check says so; on x86-64 Linux it is an 80-bit x87 float in 16 bytes,
and on AArch64 Linux an IEEE binary128, and Rust has no type for either. A
function whose signature mentions one is refused with an error saying that. A
`long double` *field* is fine - those bytes are carried around, not passed
between the languages - though a struct with one cannot be `generate_pod!`.

### 128-bit integers

A C++ `__int128` is [`c_i128`](https://docs.rs/autocxx/latest/autocxx/struct.c_i128.html),
another transparent newtype. There is no `c_u128`: `bindgen` cannot tell an
`unsigned __int128` from a 16-byte `long double` or a `__float128`, so
`autocxx` refuses any function which mentions one rather than guess. Neither
128-bit type may go inside a `UniquePtr` or a `CxxVector`, because MSVC has no
`__int128` and the glue which would make that work is compiled everywhere.

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
