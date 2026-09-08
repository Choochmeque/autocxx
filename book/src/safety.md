# Safety

## Unsafety policies

By default, every `autocxx` function is `unsafe`. That means you can only call C++ functions from `unsafe` blocks, and it's up to you to be sure that the C++ code upholds the invariants the Rust compiler expects.

You can optionally specify:

`safety!(unsafe)`

within your `include_cpp!` macro invocation. If you do this, you are promising the Rust compiler that _all_ your C++ function calls are upholding the invariants which `rustc` expects, and thus each individual function is no longer `unsafe`.

See [`safety!`](https://docs.rs/autocxx/latest/autocxx/macro.safety.html) in the documentation for more details.

## Examples with and without `safety!(unsafe)`

Without a `safety!` directive:

```rust,ignore,autocxx
autocxx_integration_tests::doctest(
"",
"#include <cstdint>
inline uint32_t do_math(uint32_t a, uint32_t b) { return a+b; }",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    generate!("do_math")
}

fn main() {
    assert_eq!(unsafe { ffi::do_math(12, 13) }, 25);
}
}
)
```

With a `safety!` directive:

```rust,ignore,autocxx
autocxx_integration_tests::doctest(
"",
"#include <cstdint>
inline uint32_t do_math(uint32_t a, uint32_t b) { return a+b; }",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe)
    generate!("do_math")
}

fn main() {
    assert_eq!(ffi::do_math(12, 13), 25);
}
}
)
```

## Pragmatism in a complex C++ codebase

This crate mostly intends to follow the lead of the `cxx` crate in where and when `unsafe` is required. But, this crate is opinionated. It believes some unsafety requires more careful review than other bits, along the following spectrum:

* Rust unsafe code (requires most review)
* Rust code calling C++ with raw pointers
* Rust code calling C++ with shared pointers, or anything else where there can be concurrent mutation
* Rust code calling C++ with unique pointers, where the Rust single-owner model nearly always applies (but we can't _prove_ that the C++ developer isn't doing something weird)
* Rust safe code (requires least review)

If your project is 90% Rust code, with small bits of C++, _don't use this crate_. You need something where all C++ interaction is marked with big red "this is terrifying" flags. This crate is aimed at cases where there's 90% C++ and small bits of Rust, and so we want the Rust code to be pragmatically reviewable without the signal:noise ratio of `unsafe` in the Rust code becoming so bad that `unsafe` loses all value.

## Worked example

Imagine you have this C++:

```cpp
struct Thing;
void print_thing(const Thing& thing);
```

By using `autocxx` (or `cxx`), you're promising the Rust compiler that the `print_thing` function does sensible things with that
reference:

* It doesn't store a pointer to the thing anywhere and pass it back to Rust later.
* It doesn't mutate it.
* It doesn't delete it.
* or any of the other things that you're not permitted to do in unsafe Rust.

## Soundness

This crate shares the general approach to safety and soundness pioneered by cxx, but has two important differences:

* cxx requires you to specify your interface in detail, and thus think through all aspects of the language boundary. autocxx doesn't, and may autogenerate footguns.
* cxx may allow multiple conflicting Rust references to exist to 'trivial' data types ("plain old data" or POD in autocxx parlance), but they're rare. autocxx may allow conflicting Rust references to exist even to 'opaque' (non-POD) data, and they're more common. This difference exists because opaque data is zero-sized in cxx, and zero-sized references cannot conflict. (In autocxx, we tell Rust about the size in order that we can allocate such types on the stack.)

There are preliminary explorations to avoid this problem by using a C++ reference wrapper type. See `examples/reference-wrappers`.

## Thread safety

`autocxx` assumes nothing about the thread safety of your C++ types, so the non-POD types it generates are neither [`Send`](https://doc.rust-lang.org/std/marker/trait.Send.html) nor [`Sync`](https://doc.rust-lang.org/std/marker/trait.Sync.html). This follows cxx, which says the same of its opaque types.

Nothing in a C++ class declaration reveals whether its objects may cross a thread boundary. A class might hold a lock it has taken, an index into a thread-local pool, or a destructor which has to run on the thread that constructed the object. `Send` is exactly the claim that moving a value to another thread is allowed, and `autocxx` is not in a position to make that claim on your behalf.

This extends to the pointers you hold them in: `cxx` grants `UniquePtr<T>: Send` only where `T: Send`, so a `UniquePtr` to a non-POD type won't cross a thread boundary either.

If you know a particular C++ type really is thread safe, you can say so. `include_cpp!` expands into the crate that invokes it, so in that crate the generated type is local and the impl is yours to write:

```rust,ignore
// SAFETY: a MyType may be used and destroyed on any one thread.
unsafe impl Send for ffi::MyType {}

// SAFETY: MyType's const methods may additionally be called concurrently
// through shared references.
unsafe impl Sync for ffi::MyType {}
```

The `unsafe` is the point: those claims are yours to justify, and they are two different claims. `Send` says an object may be handed from one thread to another - so audit the destructor as well as the methods, since it runs wherever the value is finally dropped. `Sync` says two threads may use one object *at the same time* through `&`, which is a stronger thing to promise: a C++ `const` method is free to update a `mutable` cache without synchronising, and that races even though nothing in Rust looks mutable.

Audit the whole safe API the impl exposes, not merely the calls you have in mind today. Once written, the impl licenses every caller, including your own downstream users.

Generated types with type or lifetime parameters can have the impl too, but there the claim must hold for *every* instantiation it admits, which usually means a bound:

```rust,ignore
// SAFETY: MyContainer owns its element and shares nothing else, so it may go
// wherever the element may go.
unsafe impl<T: Send> Send for ffi::MyContainer<T> {}
```

`T: Send` is not a universal answer - it suits a container which owns its elements, whereas one handing out shared access to them would want `T: Sync`, and a container which is thread-affine for reasons of its own is unsuitable whatever `T` is. Work out which the C++ actually is.

### If the bindings come from someone else's crate

Only the crate containing the `include_cpp!` can write that impl. If you depend on a library which generates bindings and re-exports them, the type is foreign to you and Rust's orphan rule refuses the impl - re-exporting or aliasing it doesn't change that:

```rust,ignore
use their_bindings::MyType;
unsafe impl Send for MyType {} // error[E0117]
```

Ask that library to make the claim, since its author is the one who knows the C++ type. Failing that, wrap it in a type of your own and make the claim about the wrapper:

```rust,ignore
pub struct SendMyType(cxx::UniquePtr<their_bindings::MyType>);

// SAFETY: audited - MyType may be used and destroyed on any thread.
unsafe impl Send for SendMyType {}

impl SendMyType {
    pub fn count(&self) -> u32 { self.0.count() }
}
```

Note this doesn't make `their_bindings::MyType` itself `Send`, so it won't satisfy an API which demands that bound, and you'll be forwarding any methods you need.

### POD types

POD types are unaffected: they're plain data, and they're `Send` and `Sync` on the same terms as any other Rust struct with the same fields. Be aware that this is a statement about the data, not a promise about the C++ methods. A POD type which is merely an integer handle into thread-local state on the C++ side is `Send` as far as Rust is concerned, and keeping its methods on the right thread is still yours to arrange.

