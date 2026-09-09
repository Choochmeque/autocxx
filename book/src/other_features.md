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

A variable of non-POD type is reached differently, because there is no
non-POD value `autocxx` can hand Rust at all. Such a variable appears as a
*function* of its own name - `ffi::BOB()` - which returns an opaque holder
standing for a `const` reference to it. The holder's `get()` gives you the
address of the variable itself; nothing is copied, so the type need not be
copy-constructible and what you read is whatever C++ has most recently written
there.

```rust,ignore
let bob = ffi::BOB();
let bob = unsafe { &*bob.as_ref().unwrap().get() };
assert_eq!(bob.get().as_ref().unwrap().to_str().unwrap(), "hello");
```

A static data member of a class works too, and needs no `extern` because
defining it gives it external linkage anyway. Its name is flattened into the
enclosing namespace, so `struct Anna { static Point ORIGIN; };` asks for
`generate!("Anna_ORIGIN")` and appears as `ffi::Anna_ORIGIN`. It must be POD:
the flattened name is not one C++ has, so there is nothing for the getter a
non-POD variable needs to call it by, and `autocxx` says so rather than
generating C++ which does not compile.

One assumption comes with the `unsafe`. Rust requires that an object does not
change while Rust holds a reference to it, and `autocxx` has no way to enforce
that on the C++ side. Reading a variable which C++ mutates concurrently is
undefined behaviour, and so is reading one whose value C++ changes through a
`mutable` member of an otherwise `const` object - legal C++, but not something
you can expose this way. Use a getter function for anything C++ writes to.

## `volatile`

Rust has no `volatile` type. It has volatile *accesses* -
[`read_volatile`](https://doc.rust-lang.org/std/ptr/fn.read_volatile.html) and
[`write_volatile`](https://doc.rust-lang.org/std/ptr/fn.write_volatile.html) -
and nothing in a type or a signature carries the promise that a read may not be
cached, reordered or elided. So a C++ declaration mapped straight onto a Rust
type loses it: `autocxx` would have to generate a volatile *access*, and where
it cannot, the binding does not keep what the qualifier says.

The dividing line is *where the read happens*. C++ performing the access keeps
the promise; an ordinary Rust load of a plainly mapped type does not. Rust can
keep it too, but only through a type which says so - which is what the handle
below is for, in the one position where Rust is the side doing the reading.

**Refused, by name.** A `volatile` variable, a field of a `generate_pod!`
struct, and a template argument. In each of these Rust would end up doing the
access itself - a re-exported `static` is read by ordinary Rust loads, a POD
struct's fields are ordinary Rust fields - or, for a template argument, the
qualifier would simply be dropped from the C++ `autocxx` writes, naming a
different specialization. Each refusal says what to do instead, and it is
usually the same thing: write a C++ function which performs the volatile access,
and bind that.

The variable case is the one worth knowing about, because binding it looks
harmless and is not. `extern const volatile int status;` is how a read-only
hardware register is declared, and the `const` alone would make it an immutable
Rust `static` - which the compiler may constant-fold reads of, the exact
opposite of what `volatile` asked for.

**Bound, and honestly - where the value is a scalar.** A `volatile` data member
of built-in, enumeration or pointer type keeps its getter. The C++ `autocxx`
generates for it is `obj.member`, and reading a `volatile` glvalue *is* a
volatile access - so the read happens in C++, once per call, and what crosses to
Rust is the copy it produced.

A member of class type is refused instead, whichever shape its getter would
take. C++ copies a class by calling a constructor, and an implicitly declared
copy constructor takes `const T&` or `T&` - neither of which a `volatile T`
binds to - so `obj.member` does not compile for one however copyable it
otherwise is. A borrowed getter is no better: it hands Rust a reference and lets
Rust do the reading.

A `volatile` **return** of scalar type is bound through a wrapper. A
cv-qualified return type is part of a function's type and `cxx` declares a
function by taking its address, so `int (*f$)() = ::f;` does not compile for a
`volatile int f()`. `autocxx` already meets this for a `const` return and
answers it the same way: its own wrapper returns the unqualified type and calls
through.

A class-type `volatile` return is refused. The wrapper would have to
copy-initialize a `T` from a `volatile T`, which needs a constructor C++ does
not implicitly declare, and so does not compile at C++14 - the standard
`autocxx` generates its C++ against. C++17 initializes the result directly and
would accept it, but the refusal is pinned to that floor rather than to whichever
standard you happen to compile with.

A **by-value parameter** binds as if the qualifier were not there, because as
far as C++ is concerned it is not: a top-level cv-qualifier on a parameter is
no part of a function's type, so `void f(volatile int)` and `void f(int)` are
one function.

A struct with a `volatile` member is still usable, whichever of these applies;
it simply cannot be `generate_pod!`.

**Bound through a handle - a pointer or reference *to* something `volatile`.**
`volatile int*` and `volatile int&` are where the qualifier is most often used
in real headers, and they are the one position in which *Rust* performs the
access. There is no Rust pointer type which carries the promise, so `autocxx`
hands over one which does: `autocxx::VolatilePtr<T>`, whose `read` and `write`
are `read_volatile` and `write_volatile`.

```cpp
volatile uint32_t* uart_status();
void configure(volatile uint32_t* reg);
```

```rust,ignore
let reg = unsafe { ffi::uart_status() };
let flags = unsafe { reg.read() };
unsafe { reg.write(flags | 1) };
unsafe { ffi::configure(reg) };
```

The handle is the currency in both directions, so what one function returns
goes straight into another which takes it. `autocxx::VolatilePtr::new` makes
one from an address Rust already knows - `0x4000_0000 as *mut u32` - and
`as_raw` gets the address back out.

A `const volatile` pointee - how a status register the program may read but not
write is declared - becomes `autocxx::VolatileConstPtr<T>`, which has no
`write`.

Three things are worth knowing about the handle. It is deliberately not `&T`:
a Rust shared reference promises the referent does not change while it lives,
which is the one thing a hardware register is guaranteed to do, so a reference
here would be a false statement to the compiler rather than merely a lost
guarantee. `read` and `write` are `unsafe`, because whether the address is
valid is C++'s business and `autocxx` cannot check it. And a volatile access is
*not* an atomic one and carries no ordering relative to anything but other
volatile accesses: concurrent access from another thread is a data race exactly
as it would be otherwise, and
[`core::sync::atomic`](https://doc.rust-lang.org/core/sync/atomic/) is what to
reach for when synchronization is what you want.

The handle appears only where the pointee is of built-in type, because that is
where Rust is the side reading it. `read` is a `read_volatile`, which needs a
`Copy` type: a class is not, and neither is a C++ enumeration, whose generated
Rust counterpart does not implement `Copy`. (A `volatile` enumeration *member*
is bound, because there C++ does the copying.) A pointer-valued pointee is left
out for a second reason - a qualifier written on a pointer binds to the
declarator rather than reading left to right, so `T* const volatile` is not
`const volatile T*`, and the generated C++ cannot name it by putting the
qualifiers in front.

Any other pointee is left exactly as it was rather than turned down, because
where C++ performs the access, handing it the address is already right - a copy
constructor taking `const volatile T&` binds as it always did.

**Not yet handled.** A volatile-qualified *method* (`void f() volatile`).
`libclang` exposes no way to ask, so where a class has both a plain and a
volatile-qualified overload of one name, both bind to the plain one.

A qualifier which reaches a signature through a type alias - `typedef volatile
uint32_t vu32;`, or `using P = volatile uint32_t*;` - is also not seen.
`bindgen` resolves an alias to its target before recording the qualifier, so
nothing downstream knows the type was `volatile`, and the same is true of
`const`. Such a signature fails to build inside the generated C++ rather than
being reported against the function you asked for; it is never bound as though
the qualifier were absent. Writing the qualifier at the point of use rather
than in the alias is the way round it.

`volatile` nested deeper than the immediately pointed-at type - `volatile
uint32_t*&`, a reference to a pointer to something volatile - is likewise not
handled, and fails the same loud way. The handle covers one level, which is the
level register code is written at.

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

A shim which takes a plain C function pointer does not help either: `cxx` has
no function pointer type, so `autocxx` has nothing to declare to it, and a
function with one in its signature is refused saying so. A *struct field* of
function pointer type is a different matter and does work: a field is data
whose layout `autocxx` copies rather than a type crossing the language
boundary, so a struct holding one can even be POD, and Rust can put one of its
own `extern "C"` functions there for C++ to call. It is passing or returning
one that has nowhere to go.

To have C++ call into Rust without that, either subclass a C++ observer class
from Rust or hand C++ a named Rust function; both are described under
[callbacks into Rust](rust_calls.md).

The explanation reaches you through the doc comment of the stub standing in for
whatever could not be generated, whichever standard library you build against,
though the two put `std::function` beyond `bindgen` differently. With libstdc++
and libc++ it is reduced to an opaque blob of bytes, and the stub stands in for
that blob - as it does for any other type `bindgen` reduces the same way, which
in practice means templated types whose parameters it cannot model. With MSVC's
standard library the type keeps its name, and the explanation is attached to
`std::function` itself; a class-scoped `using` alias of it is reported as an
alias to something which could not be generated, and repeats that thing's own
explanation.
