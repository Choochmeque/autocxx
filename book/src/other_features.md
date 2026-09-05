# Other C++ features

You can make Rust subclasses of C++ classes - as these are mostly used to
implement the Observer pattern, they're documented under [calls from C++ to Rust](rust_calls.md).

## Preprocessor symbols

`#define` and other preprocessor symbols will appear as constants.
At present there is no way to do compile-time disablement of code
(equivalent of `#ifdef`)[^ifdef].

[^ifdef]: [This feature](https://github.com/google/autocxx/issues/57) should add ifdef support.

## Variables and constants

A C++ variable whose value the compiler knows - `const int kMax = 4;`,
`constexpr double kPi = 3.14;`, or a `static const int` member of a class -
appears as a Rust `const`, so you can use it anywhere.

A variable which instead lives at some address, such as one of
[POD](cpp_types.md) struct type, appears as a Rust `static`. Reading it is
`unsafe`, because C++ may be changing it at the same time:

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"
const Point ORIGIN = Point { 0, 0 };
",
"#include <cstdint>

struct Point {
    uint32_t x;
    uint32_t y;
};

extern const Point ORIGIN;
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate_pod!("Point")
    generate!("ORIGIN")
}

fn main() {
    assert_eq!(unsafe { ffi::ORIGIN.x }, 0);
}
}
)
```

Two restrictions apply.

The variable must have external linkage, which is why the header above says
`extern` and the definition lives in a `.cc` file. A namespace-scope variable
which is `static`, or which is `const` without `extern`, or which sits in an
anonymous namespace, is a *different object in every translation unit* that
includes the header - and in a translation unit which never uses it, the
compiler emits nothing at all. There is no single symbol for Rust to link
against, so `autocxx` reports the problem instead of generating code which
fails to link. (On MSVC, where the decorated name is the same either way,
`autocxx` can't tell, and you get the link error.)

Its type must also be POD, or one which `bindgen` writes directly in Rust such
as `int`. `autocxx` exposes a non-POD type as an opaque object which is only
usable behind a pointer, so there is nothing useful it could hand you for a
variable of such a type.

A static data member of a class works too, and needs no `extern` because
defining it gives it external linkage anyway. Its name is flattened into the
enclosing namespace, so `struct Anna { static Point ORIGIN; };` asks for
`generate!("Anna_ORIGIN")` and appears as `ffi::Anna_ORIGIN`.

One assumption comes with the `unsafe`. Rust requires that an object does not
change while Rust holds a reference to it, and `autocxx` has no way to enforce
that on the C++ side. Reading a variable which C++ mutates concurrently is
undefined behaviour, and so is reading one whose value C++ changes through a
`mutable` member of an otherwise `const` object - legal C++, but not something
you can expose this way. Use a getter function for anything C++ writes to.

## String constants

Whether from a preprocessor symbol or from a C++ `char*` constant,
strings appear as `[u8]` with a null terminator. To get a Rust string,
do this:

```cpp
#define BOB "Hello"
```

```
# mod ffi { pub static BOB: [u8; 6] = [72u8, 101u8, 108u8, 108u8, 111u8, 0u8]; }
assert_eq!(std::str::from_utf8(&ffi::BOB).unwrap().trim_end_matches(char::from(0)), "Hello");
```

## `std::function`

`bindgen` has no way to describe a `std::function` in Rust, and `cxx` cannot
bind one either; its function support stops at
[function pointers](https://cxx.rs/binding/fn.html). `autocxx` therefore
generates nothing which takes or returns one. Only the members which mention
`std::function` are lost: the rest of the enclosing class is generated as usual.

A shim which takes a plain C function pointer does not help, because `bindgen`
writes those as `Option<extern "C" fn(..)>` and `autocxx` has no binding for
`Option` either. To have C++ call into Rust, either subclass a C++ observer
class from Rust or hand C++ a named Rust function; both are described under
[callbacks into Rust](rust_calls.md).

Where the explanation appears depends on your standard library, because the two
put `std::function` beyond `bindgen` differently. With libstdc++ and libc++ it
is reduced to an opaque blob of bytes, and the doc comment of the stub standing
in for that blob carries the explanation - as it does for any other type
`bindgen` reduces the same way, which in practice means templated types whose
parameters it cannot model. With MSVC's standard library the type keeps its
name, and the explanation is attached to `std::function` itself; a
class-scoped `using` alias of it is reported less precisely, as a forward
declaration whose target could not be generated.
