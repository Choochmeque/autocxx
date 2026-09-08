# C++ Exceptions

C++ exceptions are supported via the `throws!` directive. When a function is marked
with `throws!`, its Rust binding returns `Result<T, cxx::Exception>` instead of `T`,
allowing you to handle C++ exceptions that propagate across the FFI boundary.
Constructors take a slightly different shape, for reasons the
[Constructors](#constructors) section explains.

## Basic usage

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"
#include <stdexcept>
void do_risky_thing() {
    throw std::runtime_error(\"something went wrong\");
}
",
"
#include <stdexcept>
void do_risky_thing();
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("do_risky_thing")
    throws!("do_risky_thing")
}

fn main() {
    let result = ffi::do_risky_thing();
    assert!(result.is_err());
    // You can access the exception message:
    // println!("Error: {}", result.unwrap_err());
}
}
)
```

## Functions with return values

Functions that return values work the same way - the return type becomes
`Result<T, cxx::Exception>`:

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"
#include <stdexcept>
#include <cstdint>
uint32_t parse_number(const char* s) {
    if (!s || !*s) throw std::runtime_error(\"empty string\");
    return atoi(s);
}
",
"
#include <stdexcept>
#include <cstdint>
uint32_t parse_number(const char* s);
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("parse_number")
    throws!("parse_number")
}

fn main() {
    // Successful call - unwrap the Result.
    // The function takes a raw pointer, so it stays unsafe
    // even under safety!(unsafe_ffi).
    let value = unsafe { ffi::parse_number(c"42".as_ptr()) }.unwrap();
    assert_eq!(value, 42);

    // Exception is caught and converted to Err
    let result = unsafe { ffi::parse_number(std::ptr::null()) };
    assert!(result.is_err());
}
}
)
```

## Qualified names

The `throws!` directive supports qualified names for precise control over which
functions are marked as throwing:

| Pattern | Matches |
|---------|---------|
| `throws!("do_something")` | Any function named `do_something` |
| `throws!("MyClass::method")` | Method `method` on class `MyClass` |
| `throws!("ns::do_something")` | Function `do_something` in namespace `ns` |
| `throws!("ns::MyClass::method")` | Method on a namespaced class |
| `throws!("Outer::Inner::method")` | Method on a nested class |

A nested class may also be named by the flattened `Outer_Inner` spelling
autocxx knows it by, exactly as in `generate!`.

### Partial matching

Partial matching is supported: a shorter pattern will match functions in any
namespace. For example, `throws!("do_something")` will match both a top-level
`do_something` and `my_namespace::do_something`.

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"
#include <stdexcept>
namespace utils {
    void validate() {
        throw std::runtime_error(\"validation failed\");
    }
}
",
"
#include <stdexcept>
namespace utils {
    void validate();
}
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("utils::validate")
    throws!("validate")  // matches utils::validate
}

fn main() {
    let result = ffi::utils::validate();
    assert!(result.is_err());
}
}
)
```

## Constructors

A constructor is named in `throws!` the way C++ names it - `Class::Class`:

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"",
"
#include <stdexcept>
#include <cstdint>
class Goat {
public:
    Goat(uint32_t horns) {
        if (horns > 2) throw std::runtime_error(\"too many horns\");
        horns_ = horns;
    }
    uint32_t horns() const { return horns_; }
private:
    uint32_t horns_;
};
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("Goat")
    throws!("Goat::Goat")
}

fn main() {
    let goat = ffi::Goat::new(2).try_within_unique_ptr().ok().unwrap();
    assert_eq!(goat.horns(), 2);

    let complaint = ffi::Goat::new(9).try_within_unique_ptr().err().unwrap();
    assert_eq!(complaint.what(), "too many horns");
}
}
)
```

An ordinary constructor hands back an `impl New`, a recipe you finish with
`within_unique_ptr()`, `within_box()` or the `moveit!` macro. One named by
`throws!` hands back an `impl TryNew` instead, and each of those finishers has
a fallible counterpart:

| Infallible | Fallible | Gives you |
|---|---|---|
| `.within_unique_ptr()` | `.try_within_unique_ptr()` | `Result<UniquePtr<T>, cxx::Exception>` |
| `.within_box()` | `.try_within_box()` | `Result<Pin<Box<T>>, cxx::Exception>` |
| `.within_cpp_pin()` | `.try_within_cpp_pin()` | `Result<CppPin<T>, cxx::Exception>` |
| `moveit! { let x = ...; }` | `stack_slot!(storage);` then `storage.try_emplace(...)` | `Result<Pin<MoveRef<T>>, cxx::Exception>` |

There is deliberately no `Result<impl New, _>`: that would decide whether
construction failed before the constructor had run, whereas a C++ constructor
only throws once it is under way. `impl TryNew` says the right thing - the
place stays uninitialized when construction fails - and it is why the method
names differ rather than just the return types.

### On the stack

`moveit!` reserves stack storage and constructs into it in a single `let`,
which leaves nowhere for a failure to go. So the two halves are written
separately, and the half which can fail is an ordinary expression which takes
`?`:

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"",
"
#include <stdexcept>
#include <cstdint>
class Goat {
public:
    Goat(uint32_t horns) {
        if (horns > 2) throw std::runtime_error(\"too many horns\");
        horns_ = horns;
    }
    uint32_t horns() const { return horns_; }
private:
    uint32_t horns_;
};
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("Goat")
    throws!("Goat::Goat")
}

fn graze() -> Result<u32, cxx::Exception> {
    autocxx::stack_slot!(storage);
    let goat = storage.try_emplace(ffi::Goat::new(1))?;
    Ok(goat.horns())
}

fn main() {
    assert_eq!(graze().unwrap(), 1);
}
}
)
```

When a constructor throws, the storage which had been set aside for the object
is released without any destructor running on it - the object never existed.
Whichever of its members had already been constructed are destroyed by C++
itself as the exception unwinds, exactly as they would be in a C++ program.

### What a designation covers

`throws!("Goat::Goat")` marks every constructor of `Goat`, because all of them
share that C++ name; there is no spelling which picks out one overload. A
constructor which is marked but which never actually throws is fallible all the
same - `throws!` describes what C++ is allowed to do, not what it did - and its
`Result` is simply always `Ok`. Constructors nobody has named are untouched:
they keep the infallible `impl New` they always had.

Naming the class alone - `throws!("Goat")` - also marks its constructors, since
`throws!` matches a trailing qualified name. Prefer `Goat::Goat`: the short form
would equally match a free function called `Goat`.

One kind of constructor the designation cannot reach is a copy or move
constructor the class declares for itself. Those become `moveit`'s `CopyNew`
and `MoveNew`, whose methods return nothing and so have nowhere to put an
exception; `throws!("Goat::Goat")` names them along with the rest, but leaves
them as they were, and a copy or move constructor which throws still terminates
the process. The same goes for a destructor, which becomes `Drop` - and which
should not throw in C++ either. Only the value constructors of the class become
fallible.

## Functions which return a class by value

A `throws!` function which returns a non-POD type by value is built the same
way as a constructor - C++ writes the result into a place Rust provides - so it
hands back an `impl TryNew` and is finished with the same `try_` methods:

```rust,ignore
let made = ffi::make_goat(2).try_within_unique_ptr()?;
```

## Subclasses

If the C++ superclass's constructor can throw, so can the constructor of the
peer object `autocxx` generates for your subclass, and that peer constructor is
the one to name: `throws!("MySubclassCpp::MySubclassCpp")`. Implement
[`CppPeerConstructor::try_make_peer`](https://docs.rs/autocxx/latest/autocxx/subclass/trait.CppPeerConstructor.html)
as well as `make_peer`, and construct the subclass with `try_new_rust_owned`,
`try_new_cpp_owned` or `try_new_self_owned`. Designating the peer's constructor
also withholds the implementation `autocxx` writes for itself where the
superclass has a single no-argument constructor, so the implementation below is
what a subclass of any superclass needs:

```rust,ignore
impl CppPeerConstructor<ffi::MySubclassCpp> for MySubclass {
    fn make_peer(&mut self, holder: CppSubclassRustPeerHolder<Self>)
        -> cxx::UniquePtr<ffi::MySubclassCpp> {
        self.try_make_peer(holder).expect("the superclass constructor threw")
    }

    fn try_make_peer(&mut self, holder: CppSubclassRustPeerHolder<Self>)
        -> Result<cxx::UniquePtr<ffi::MySubclassCpp>, cxx::Exception> {
        ffi::MySubclassCpp::new(holder, self.arg)
    }
}
```

Note that there is no finisher on that last line. A peer's `new` hands back the
`cxx::UniquePtr` itself rather than something to place, so the fallible one
hands back the `Result` and there is nothing left to do to it.
[The subclass chapter](rust_calls.md#subclasses-without-a-safety-policy) says
why. The constructor of a concrete template instantiation behaves the same way,
for the same reason - see [`instantiable!`](https://docs.rs/autocxx/latest/autocxx/macro.instantiable.html).

## How it works

Under the hood, `autocxx` leverages [cxx's native exception handling](https://cxx.rs/binding/result.html).
When a function is marked with `throws!`, the generated cxx bridge declaration
uses `Result<T>` as the return type. This causes cxx to automatically wrap
the C++ call in a try-catch block and convert any caught `std::exception`
(or derived types) to `cxx::Exception`. The exception is caught on the C++ side
of the boundary and never unwinds into Rust.

The `cxx::Exception` type provides:
- `Display` implementation to get the exception message (`what()`)
- Conversion to `std::io::Error` via `From` trait

## Limitations

* **Non-std::exception types**: only exceptions derived from `std::exception`
  are caught and converted, because that is what cxx's `catch` clause names.
  Something else - an `int`, a bare string literal, a class of your own which
  derives from nothing - passes straight through it and out of the `noexcept`
  shim around it, which calls `std::terminate`. The process dies; it is not
  undefined behaviour, but the `Result` never arrives either.

* **Undesignated throwing functions**: if C++ throws out of a function no
  `throws!` names, the exception reaches a `noexcept` boundary in the same way
  and the process terminates. That is true of constructors too, so a
  constructor which can throw needs designating even if you intend to treat the
  exception as fatal.

* **`as_new` with a throwing constructor**: `as_new` takes a `New`, so a
  `throws!` constructor cannot be handed straight to a C++ function which takes
  its argument by value - the call would have nowhere to report a failure that
  happened while assembling its arguments. Construct the object first, with
  `try_within_unique_ptr()` or a `stack_slot!`, and pass what you get with
  `as_mov` or `as_copy`.

* **Performance**: Exception handling adds minimal runtime overhead - the
  cost is only incurred when an exception actually occurs.

* **MSVC**: an exception model must be selected, or cl.exe emits no
  unwind tables: exceptions still propagate and get caught, but
  destructors in intervening frames are skipped, so every throwing call
  leaks its owning temporaries. The `cc::Build` returned by
  `autocxx-build`'s `Builder` passes `/EHsc` for the code it compiles.
  Two things stay outside its reach: C++ you compile separately (via
  `autocxx-gen`, CMake, or another `cc::Build`) needs a compatible
  exception model of its own, and the `cxx` crate's own runtime
  (`cxx.cc`) is compiled by cxx's build script, which currently passes
  no `/EH` flag — set `CXXFLAGS=/EHsc` in your build environment to
  cover it until that is fixed upstream.
