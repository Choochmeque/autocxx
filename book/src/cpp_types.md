# C++ structs, enums and classes

If you add a C++ struct, class or enum to the [allowlist](allowlist.md), Rust bindings will be generated to that type and to any methods it has.
Even if you don't add it to the allowlist, the type may be generated if it's required by some other function - but in this case
all its methods won't be generated.

Rust and C++ differ in an important way:

* In Rust, the compiler is free to pick up some data and move it to somewhere else (in a `memcpy` sense). The object is none the wiser.
* In C++, once created, an object stays where it is, until or unless it has its "move constructor" invoked.

This makes a big difference: C++ objects can have self-referential pointers, and any such pointer would be invalidated by Rust doing
a memcpy. Such self-referential pointers are common - even some implementations of `std::string` do it.

## POD and non-POD

When asking `autocxx` to generate bindings for a type, then, you have to make a choice.

* *This C++ type is trivial*. It has no destructor or move constructor (or they're trivial), and thus Rust is free to move it around the stack as it wishes. `autocxx` calls these types POD ("plain old data"). Alternatively,
* *This C++ type has a non-trivial destructor or move constructor, so we can't allow Rust to move this around*. `autocxx` calls these types non-POD.

POD types are nicer:

* You can just use them as regular Rust types.
* You get direct field access.
* No funny business.

Non-POD types are awkward:

* You can't just _have_ one as a Rust variable. Normally you hold them in a [`cxx::UniquePtr`](https://docs.rs/cxx/latest/cxx/struct.UniquePtr.html), though there are other options.
* Fields are read through generated accessors rather than directly. `autocxx` gives every public
  data member a method of the same name: a `b` field is read with `obj.b()`. A field Rust could
  hold by value comes back by value, anything else comes back as a reference borrowed from the
  object, and an array or reference member gets a documented refusal instead of an accessor.
  There are no setters yet.
* You can't even have a `&mut` reference to one, because then you might be able to use [`std::mem::swap`](https://doc.rust-lang.org/stable/std/mem/fn.swap.html) or similar. You can have a `Pin<&mut>` reference, which is more fiddly.

By default, `autocxx` generates non-POD types. You can request a POD type using [`generate_pod!`](https://docs.rs/autocxx/latest/autocxx/macro.generate_pod.html). Don't worry: you can't mess this up. If the C++ type doesn't in fact comply with the requirements for a POD type, your build will fail thanks to some static assertions generated in the C++. (If you're _really_ sure your type is freely relocatable, because you implemented the move constructor and destructor and you promise they're trivial, you can override these assertions using the C++ trait `IsRelocatable` per the instructions in [cxx.h](https://github.com/dtolnay/cxx/blob/master/include/cxx.h)).

See [the chapter on storage](storage.md) for lots more detail on how you can hold onto non-POD types.

## Construction

Constructing a POD object is simple: call its `new` associated function. [Bob's your uncle!](https://en.wikipedia.org/wiki/Bob%27s_your_uncle)

Multiple constructors (aka constructor overloading) follows the same [rules as other functions](cpp_functions.html#overloads---and-identifiers-ending-in-digits).

Constructing a non-POD object requires two steps.

* Call the `new` associated function in the same way. This will give you something implementing [`moveit::New`](https://docs.rs/moveit/latest/moveit/new/trait.New.html)/
* Use this to make the object on the heap or stack, in any of the following ways:

| Where you want to create it | How to create it | What you get | Example |
| --------------------------- | ---------------- | ------------ | ------- |
| C++ heap (*recommended for simplicity*) | [`Within.within_unique_ptr()`](https://docs.rs/autocxx/latest/autocxx/trait.Within.html) or [`UniquePtr::emplace`](https://docs.rs/moveit/latest/moveit/new/trait.EmplaceUnpinned.html#method.emplace) | [`cxx::UniquePtr<T>`](https://docs.rs/cxx/latest/cxx/struct.UniquePtr.html) | `let mut obj = ffi::Goldfish::new().within_unique_ptr()` or `let mut obj = UniquePtr::emplace(ffi::Goldfish::new())` |
| Rust heap | [`Within.within_box()`](https://docs.rs/autocxx/latest/autocxx/trait.Within.html) or [`Box::emplace`](https://docs.rs/moveit/latest/moveit/new/trait.Emplace.html#method.emplace) | `Pin<Box<T>>` | `let mut obj = ffi::Goldfish::new().within_box()` or `let mut obj = Box::emplace(ffi::Goldfish::new())` |
| Rust stack | [`moveit` macro](https://docs.rs/moveit/latest/moveit/macro.moveit.html) | `Pin<MoveRef<T>>` | `moveit! { let mut obj = ffi::Goldfish::new() }` |

For heap construction, the prefix (`emplace`) and postfix (`.within_...`) forms are exactly identical. Choose whichever suits your needs best.

Rust's own [`std::pin::pin!`](https://doc.rust-lang.org/std/pin/macro.pin.html) macro is *not* a substitute for `moveit!` here, even though both produce a pinned stack value. `pin!` pins a value you already own; `moveit::New::new` only ever writes into a place you hand it, and there's no way to obtain a non-POD object as a plain Rust value to hand to `pin!` in the first place - that's precisely the point of non-POD types. Writing `std::pin::pin!(ffi::Goldfish::new())` compiles, but pins the `New` recipe itself rather than a `Goldfish`, and the object never gets constructed.



### Should you construct on the Rust heap or the C++ heap?

Use `.within_unique_ptr()` to create objects on the C++ heap. This gives you a [`cxx::UniquePtr<T>`](https://docs.rs/cxx/latest/cxx/struct.UniquePtr.html) which works well with other autocxx and cxx APIs.

There is a small disadvantage - [`cxx::UniquePtr<T>`](https://docs.rs/cxx/latest/cxx/struct.UniquePtr.html) is able to store `NULL` values. Therefore, each time you use the resulting object, there is an `unwrap()` (explicit or implicit). If this bothers you, use the `Box` option instead which can never be `NULL`.

### Construction sounds complicated. Do you have a code example?

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"void A::set(uint32_t val) { a = val; }
uint32_t A::get() const { return a; }",
"#include <stdint.h>
#include <string>
struct A {
    A() {}
    void set(uint32_t val);
    uint32_t get() const;
    uint32_t a;
};
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("A")
}

fn main() {
    moveit! {
        let mut stack_obj = ffi::A::new();
    }
    stack_obj.as_mut().set(42);
    assert_eq!(stack_obj.get(), 42);

    let mut heap_obj = ffi::A::new().within_unique_ptr();
    heap_obj.pin_mut().set(42);
    assert_eq!(heap_obj.get(), 42);

    let mut another_heap_obj = ffi::A::new().within_box();
    another_heap_obj.as_mut().set(42);
    assert_eq!(another_heap_obj.get(), 42);
}
}
)
```

## Enums

A C++ `enum` becomes a native Rust `enum` by default. That's the right shape
for an enumeration whose values are a closed set - a colour, a state, an error
code - and it lets you `match` on it exhaustively.

It is the wrong shape for a flag enum. Writing `FLAG_A | FLAG_B` in C++ gives
you an `int` - the operands are promoted, unless the enum has an overloaded
`operator|` - but converting that back to the enum type is ordinary, deliberate
C++, and the result is a perfectly good enum object. A C++ enum object may hold
values no enumerator names: for an enum with a fixed underlying type, such as
`enum Flags : int`, every value that type can represent; for one without, every
value in the bit range its enumerators span. A Rust `enum` may not - a value
which is none of its variants is instant undefined behaviour - so the
combination has no representation at all.

Use [`enum_style!`](https://docs.rs/autocxx/latest/autocxx/macro.enum_style.html)
to pick a different representation for particular enums:

| Style | What you get | Good for |
| ----- | ------------ | -------- |
| `RustifiedEnum` | A native Rust `enum`. This is the default. | Closed sets of values |
| `RustifiedNonExhaustiveEnum` | The same, marked `#[non_exhaustive]`, so your `match`es need a catch-all arm | Closed sets which C++ may extend later |
| `NewtypeEnum` | An integer newtype whose enumerators are associated constants | Values which may be arbitrary integers |
| `BitfieldEnum` | The same newtype, plus `&`, `|`, `^` and `!` | Flags |

The integer the two newtype styles wrap is the enum's underlying type as the
C++ compiler sees it, so `flags.0` is a `c_int` for `enum Flags : int` and a
`c_uint` for `enum Flags : unsigned`. An unscoped enum with no fixed underlying
type has no portable answer: the compiler picks, and it may pick differently
from one target or set of flags to the next. On the targets autocxx tests, MSVC
gives it `int`, while gcc and clang give one whose enumerators are all
non-negative `unsigned int`. Name the underlying type in C++ if you intend to
write `.0`'s type down.

The directive takes the style first, then any number of enum names, and may be
repeated to give different styles to different enums:

```rust,ignore
enum_style!(BitfieldEnum, "FileFlags", "WindowFlags")
enum_style!(RustifiedNonExhaustiveEnum, "ErrorCode")
```

Name each enum exactly as you would in `generate!` - so a namespaced enum is
`ns::Thing`, and an enum nested inside a class is `Outer_Inner`, because that
is the name `bindgen` gives it. Asking for two different styles for the same
enum is an error, as is anything that isn't a plain name.

### `NewtypeEnum` and `BitfieldEnum` need `generate_pod!`

These two styles reach Rust as a `struct`, not an `enum`, and their
enumerators are associated constants on it. `autocxx` only re-exports the
generated type - constants and all - for types it holds by value, so those two
styles have to be asked for with
[`generate_pod!`](https://docs.rs/autocxx/latest/autocxx/macro.generate_pod.html).
A plain [`generate!`](https://docs.rs/autocxx/latest/autocxx/macro.generate.html)
gives you an opaque type with no constants on it, which is unlikely to be what
you wanted. The two rustified styles work with either, since a Rust `enum` is
POD regardless.

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"",
"enum FileFlags : int {
    READ = 1 << 0,
    WRITE = 1 << 1,
    EXECUTE = 1 << 2,
};
inline bool is_writable(FileFlags flags) { return flags & FileFlags::WRITE; }
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    enum_style!(BitfieldEnum, "FileFlags")
    generate_pod!("FileFlags")
    generate!("is_writable")
}

fn main() {
    let flags = ffi::FileFlags::READ | ffi::FileFlags::WRITE;
    assert!(ffi::is_writable(flags));
    assert!(!ffi::is_writable(ffi::FileFlags::READ));
}
}
)
```

## Extra derives

The Rust types `autocxx` generates carry only the derives it needs itself, so
by default you cannot print a generated struct or compare two of them. Use
[`derive!`](https://docs.rs/autocxx/latest/autocxx/macro.derive.html) to ask
for more:

```rust,ignore
derive!("Point", "Debug", "PartialEq")
```

Name the type exactly as you would in `generate!`, and write each trait as you
would inside `#[derive(..)]` - a path works too, so a derive macro from another
crate can be asked for as `"num_enum::TryFromPrimitive"`. It is then your job
to have that macro in scope, and to make sure the type can satisfy the trait:
`Clone` needs every field to be `Clone`, and so on. `autocxx` does not check,
so an impossible request comes back from `rustc` rather than from `autocxx`.

The trait goes onto the type `autocxx` re-exports, which means `derive!` works
for a `generate_pod!` type or an enum, and is refused for anything else -
everything else is an opaque type with no fields, precisely because Rust must
not look inside it, so there would be nothing for a derive to work from.
`Default` on an enum is refused too: nothing makes one enumerator of a C++ enum
the default, and a derived `Default` on an enum with no variant marked
`#[default]` does not compile.

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"",
"#include <cstdint>
struct Point {
    uint32_t x;
    uint32_t y;
};
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate_pod!("Point")
    derive!("Point", "Debug", "PartialEq")
}

fn main() {
    let a = ffi::Point { x: 1, y: 2 };
    let b = ffi::Point { x: 1, y: 2 };
    assert_eq!(a, b);
    assert_eq!(format!("{:?}", a), "Point { x: 1, y: 2 }");
}
}
)
```

## `std::array`

A `std::array<T, N>` crosses as a Rust `[T; N]`, by value, in either direction:
`cxx` spells a Rust array as `std::array<T, N>`, so what the bridge declares is
the type the header was written with, and there is no wrapper in between.

Two things do not:

* An element `cxx` will not hold in an array. It has to be one of `cxx`'s own
  atoms, which for a type written in a header means `uint8_t` or `int8_t`, a
  `float` or a `double`, a `bool`, or a `char`. A class is not one, and neither
  is an integer whose width the platform chooses, such as `int` or `unsigned` -
  which rules out the typedefs to them, `uint32_t` and `size_t` among them.
* A `std::array` behind a reference or a pointer. `const T (&)[N]` and `const
  std::array<T, N>&` reach `autocxx` as the same Rust type, and `cxx` writes
  the second for either, so binding one would silently be binding the other.
  By value there is no such pair: no C++ function takes or returns a plain
  array that way.

Both get a refusal which says so. `std::array<T, 0>` gets one too: C++ gives
the empty array a size and Rust's `[T; 0]` has none, so they are not the same
object.

## Forward declarations

A type which is incomplete in the C++ headers (i.e. represented only by a forward
declaration) can't be held in a `UniquePtr` within Rust (because Rust can't know
if it has a destructor that will need to be called if the object is dropped.)
Naturally, such an object can't be passed by value either; it can still be
referenced in Rust references.

## Generic (templated) types

If you're using one of the generic types which is supported natively by cxx,
e.g. `std::unique_ptr`, it should work as you expect. For other generic types,
we synthesize a concrete Rust type, corresponding to a C++ typedef, for each
concrete instantiation of the type. Such generated types are always opaque, so
by default that's enough to pass them
between return types and parameters of other functions within [`cxx::UniquePtr`](https://docs.rs/cxx/latest/cxx/struct.UniquePtr.html)s
but not really enough to do anything else with these types yet[^templated].

[^templated]: Future improvements tracked [here](https://github.com/google/autocxx/issues/349)

An
[`instantiable!`](https://docs.rs/autocxx/latest/autocxx/macro.instantiable.html)
directive naming one of them goes further: it gives the instantiation a `new()`
and binds the member functions its class template declares, so that an
`ffi::Boba` can be made and asked to do things. `bindgen` tells `autocxx`
nothing at all about a specialization, so that directive is you vouching for
what it generates and your C++ compiler is the arbiter; see its documentation
for what is claimed and for the members it cannot reach. Otherwise,
to make these types more useful, you might have to add extra C++ functions to
extract data or otherwise deal with them.

Usually, such concrete types are synthesized automatically because they're
parameters or return values from functions. Very rarely, you may
want to synthesize them yourself - you can do this using the
[`concrete!`](https://docs.rs/autocxx/latest/autocxx/macro.concrete.html)
directive. As noted, though, these types are currently opaque and fairly
useless without passing them back and forth to C++, so this is not a commonly
used facility. It does, however, allow you to give a more descriptive name
to the type in Rust:

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"",
"#include <string>
struct Tapioca {
  std::string yuck;
};
template<typename Floaters>
struct Tea {
  Tea() : floaters(nullptr) {}
  Floaters* floaters;
};
inline Tea<Tapioca> prepare() {
  Tea<Tapioca> mixture;
  // prepare...
  return mixture;
}
inline void drink(const Tea<Tapioca>&) {}
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("prepare")
    generate!("drink")
    concrete!("Tea<Tapioca>", Boba)
}

fn main() {
    let nicer_than_it_sounds: cxx::UniquePtr<ffi::Boba> = ffi::prepare();
    ffi::drink(&nicer_than_it_sounds);
}
}
)
```

## Implicit member functions

Most of the API of a C++ type is contained within the type, so `autocxx` can
understand what is available for Rust to call when that type is analyzed.
However, there is an important exception for the so-called special
member functions, which will be implicitly generated by the C++ compiler for
some types. `autocxx` makes use of these types of special members:
* Default constructor
* Destructor
* Copy constructor
* Move constructor

Explicitly declared versions of these special members are easy: `autocxx` knows
they exist and uses them.

`autocxx` currently uses its own analysis to determine when implicit versions of
these exist. This analysis tries to be conservative (avoid generating wrappers
that require the existence of C++ functions that don't exist), but sometimes
this goes wrong and understanding the details is necessary to get the correct
Rust wrappers generated.

In particular, determing whether an implicit version of any of these exists
requires analyzing the types of all bases and members. `autocxx` only analyzes
types when requested, because some may be un-analyzable. If the types of any
bases or members are not analyzed, `autocxx` will assume a public destructor
exists (in the absence of any other destructors), and avoid using any other
implicit special member functions. Notably this includes the default
constructor, so types with un-analyzed bases or members and no explicit
constructors will not get a `make_unique` or `new` generated. If `autocxx` isn't
generating a `make_unique` or `CopyNew` or `MoveNew` for a type which permits
the corresponding operations in C++, make sure the types of all bases and
members are analyzed or implement it explicitly.

`autocxx` currently does not take member initializers (`const int x = 5`) into
account when determining whether a default constructor
exists[^member-initializers]. Explicitly declared default destructors still
work though.

Currently, `autocxx` assumes that an explicitly defaulted (`= default`) member
function exists, although it is valid C++ for that to be
deleted[^explicitly-defaulted]. Clang's
[-Wdefaulted-function-deleted](https://clang.llvm.org/docs/DiagnosticsReference.html#wdefaulted-function-deleted)
flag (enabled by default) will warn about types like this.

A C++ type whose destructor is inaccessible - `private`, `protected`, or
`= delete`d, whether declared that way or made so by a base or member - is one
Rust could never destroy. `autocxx` therefore does not generate any way for
Rust to own one[^inaccessible-destructor]: no `new`, no `make_unique`, no
`CopyNew` or `MoveNew`. Its methods are still generated, so you can call them
through a reference or pointer that C++ hands you, which is how such types are
normally meant to be used. Asking for the constructor anyway gives a compile
error naming the reason.

Many of the special members may be overloaded in C++. This generally means
adding `const` or `volatile` qualifiers or extra arguments with defaults.
`autocxx` avoids using any overloaded special members because choosing which
one to call from Rust gets tricky.

[^member-initializers]: Handling of member initializers is tracked
[here](https://github.com/google/autocxx/issues/816).
[^explicitly-defaulted]: Fix for explicitly defaulted special member functions
that are deleted is tracked [here](https://github.com/google/autocxx/issues/815).
[^inaccessible-destructor]: Until this was fixed, such a type could be
constructed and put in a `Box` or on the stack, and its memory was then freed
without running any C++ destructor - leaking whatever resources the C++
implementation tracked. See
[here](https://github.com/google/autocxx/issues/829).

## Inherited member functions

C++ calls an inherited member on the object which inherits it - `derived.foo()`
- and `bindgen` reports nothing of the sort: a base class arrives as a field of
the derived class, and the base's members as functions over the base's own
type. So `autocxx` binds each public member of each public base a second time,
against the classes which inherit it, and the Rust method is where C++ puts it.

The call itself is made in C++, on the object cast to the base -
`static_cast<const Base&>(d).foo(args)`. That is what dispatches a virtual
member on the object's own dynamic type, what adjusts `this` for a base which
does not sit at the start of the derived class, and what makes the Rust method
call the member whose signature it was generated from rather than whatever the
deriving class might declare under the same name. Adding the base to your
`generate!` list is not needed for any of it - if you do, you also get the
base's own type and an `AsRef` upcast to it, and both routes work.

A member is bound this way only where `autocxx` can say that
`derived.foo(args)` would have named it, so several shapes are left alone: a
name the deriving class declares itself, or one a class between it and the base
declares, since either hides the inherited member; a name two bases both
declare, which C++ calls ambiguous rather than choosing; a base reached over
anything but public inheritance, which is not a conversion a caller may make;
a base reached by more than one path, which is more than one base subobject -
`autocxx` declines these even where C++ would allow the conversion, as it does
for a virtual base, because `bindgen` says which bases are virtual only for the
class declaring them; a base whose C++ name `bindgen` never reported, which the
cast could not write; a name the base merges with a `using` declaration of its
own; and a name the base overloads, since `bindgen` reports neither the default
arguments nor enough of the parameter types to say which overload C++ would
pick. Static members are not imported, a static call having no receiver to make
it on, and neither are data members.

Three kinds of declaration hide an inherited member in C++ which `bindgen` does
not describe well enough for `autocxx` to notice: a member of an anonymous
union, an unnamed `enum`'s enumerators, and a member function template. Where a
class hides an inherited `foo` with one of those, `autocxx` binds `foo` anyway,
and the binding calls the base's member - the name reads as C++'s would not,
but it does what it says.

An `enum class`'s enumerators are members of the enumeration rather than of the
class the enum is nested in, so one named after an inherited member hides
nothing and the member is bound as usual.

A member a class re-exports with `using Base::foo;` is bound too, by a pass of
its own. That is how C++ reaches a member of a *private* base and how it widens
the access of a protected one, so its call is made on the object itself - there
being no cast to a private base to make.

## Abstract types

`autocxx` does not allow instantiation of abstract types[^abstract] (aka types with pure virtual methods).

An abstract type also needs a virtual destructor before `autocxx` offers Rust a
way to own one. No object of an abstract class exists, so every pointer to one
points at an object of some derived class; if the destructor is not virtual,
`delete`ing through that pointer runs the wrong one, which is undefined
behaviour and which clang reports as `-Wdelete-abstract-non-virtual-dtor` and
MSVC as C5205.

The C++ `cxx` generates for `UniquePtr<T>`, and for `SharedPtr<T>`'s
raw-pointer constructor, deletes through a `T*` exactly that way.
`CxxVector<T>` is a different case with the same outcome: no element of an
abstract type could ever be constructed, so nothing is there to delete, but the
destructor which would still gets instantiated and the compiler still reports
it. So for an abstract type with no virtual destructor `autocxx` adds none of
those, nor `WeakPtr<T>`, which destroys nothing itself but is of no use without
`SharedPtr<T>`. The generated type's documentation says so. As with an inaccessible destructor, the type and its
methods are still generated and can be used through a reference or pointer C++
hands you. Adding `virtual ~T();` to the C++ class is what makes it ownable.

What `autocxx` actually knows is that it *found* no virtual destructor. If the
class inherits one from a base you did not ask `autocxx` to generate, adding
that base to your `generate!` list is what lets `autocxx` see it.

Only the support `autocxx` adds of its own accord comes back. A C++ function
which itself names a `std::unique_ptr<T>` for such a `T` is still bound, and
`cxx` still generates the deleting glue for it, so such a header can still fail
to compile. Refusing those signatures with a diagnostic instead is a job for a
future release.

[^abstract]: `autocxx`'s determination of abstract types is a bit approximate and
[could be improved](https://github.com/google/autocxx/issues/774).