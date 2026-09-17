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

Each of these integer newtypes implements `Display`, printing as the integer it
wraps, so `println!("{}", ffi::some_count())` works without reaching for `.0`.
A character type such as `c_uchar` prints its value as a number, which is what
the wrapped type is.

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
`From`. Like the integer newtypes above, each implements `Display` and prints as
the integer it wraps - a number, not a character.

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
is a type people do store. The generator cannot see that happen, so it falls under what
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

## Maps

`cxx` has no map type, and nothing standing for every possible `std::map` could
be declared in a crate your generated code does not own. So `autocxx` does for
a map what it already does for any other template instantiation: each
specialization your headers actually use becomes a generated opaque type of its
own, with a set of methods on it.

```rust,ignore,autocxx
autocxx_integration_tests::doctest(
"",
"#include <map>
#include <string>
#include <cstdint>
inline std::map<std::string, uint32_t> settings() {
    std::map<std::string, uint32_t> m;
    m.emplace(\"width\", 80);
    return m;
}
inline uint32_t lookup(const std::map<std::string, uint32_t>& m) { return m.at(\"width\"); }",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("settings")
    generate!("lookup")
    concrete!("std::map<std::string, uint32_t>", Settings)
}

fn main() {
    let mut m = ffi::Settings::new();
    cxx::let_cxx_string!(height = "height");
    m.pin_mut().insert(&height, 25);
    assert_eq!(m.len(), 1);

    let m = ffi::settings();
    assert_eq!(ffi::lookup(&m), 80);
    cxx::let_cxx_string!(width = "width");
    assert_eq!(m.get(&width), Some(&80));
}
}
)
```

The type is opaque, like every C++ class `autocxx` cannot lay out: you reach one
through a `UniquePtr`, a `&`, or a `Pin<&mut>`, never by value. A map returned
by value from C++ arrives as `UniquePtr<Settings>`; one returned by reference as
`&Settings`; a `const std::map<K, V>&` parameter takes `&Settings`, and a
`std::map<K, V>&` parameter takes `Pin<&mut Settings>` and C++ writes through it.

Give the type a readable name with a
[`concrete!`](https://docs.rs/autocxx/latest/autocxx/macro.concrete.html)
directive, as above. Without one it still exists, under the name `autocxx`
derives from the C++ spelling — `std::map<std::string, uint32_t>` becomes
`std_map_std_string_uint32_t_AutocxxConcrete` — so a header change which renames
the key or value type shows up as a Rust name change.

The methods are `std::map`'s own:

| Rust | C++ |
| --- | --- |
| `new() -> UniquePtr<Self>` | default construction |
| `len()`, `is_empty()` | `size`, `empty` |
| `contains(key)`, `get(key) -> Option<&V>` | `find` |
| `insert(key, value) -> bool` | `insert` — the first value for a key wins |
| `insert_or_assign(key, value) -> bool` | `insert_or_assign` — the one which overwrites |
| `erase(key) -> bool` | `erase` |
| `keys()`, `values()` | snapshots, as `UniquePtr<CxxVector<_>>` |

Mutation goes through `Pin<&mut Self>`; nothing hands out a `&mut` into the map.
`get` borrows the map for as long as the reference lives, which is what keeps an
insertion from happening underneath it. `keys` and `values` are owned snapshots
taken when called, each in the map's iteration order — for a `std::map` that is
key order — so the two line up entry by entry as long as the map has not changed
between the calls; take both before mutating where the pairing matters. An atom key or value crosses by value
and a `std::string` one as `&CxxString`, exactly as anywhere else in `autocxx`.
All the glue is `noexcept`: a C++ exception crossing into Rust is undefined
behaviour, so an allocation failure terminates rather than unwinding, as in
`cxx`'s own container glue.

Everything outside this is refused with an explanation rather than bound:

- The key and the value must each be a type `cxx` will put in a `std::vector`:
  an integer, a character type, or `std::string`, plus `float` and `double` as
  values. `keys()` and `values()` hand back `CxxVector`s, so a key or value
  without one is a type two of the methods could not be written for. A map of
  classes is refused rather than half-supported, and is the next piece of work.
- A floating-point *key* is refused whichever map it is: `std::less` owes the
  map a strict weak ordering and NaN gives it none, and safe Rust must not be
  able to hand a C++ container an argument outside its contract.
- `std::map` and `std::unordered_map` only, with the default comparator and
  allocator. A `std::map` which fixes either to something else is refused.
  Two things escape that refusal, because `autocxx` cannot see them: a
  transparent `std::less<>` comparator, and `std::unordered_map`'s hash,
  equality predicate and allocator.
- A superclass constructor taking a map cannot serve a
  [`subclass!`](https://docs.rs/autocxx/latest/autocxx/macro.subclass.html);
  a `virtual` *method* taking one can be overridden like any other.

The two escapes end at the same wall, one declaration at a time. A lone
`f(const std::map<K, V, std::less<>>&)`, or a lone custom-hash
`std::unordered_map` parameter, is a different C++ type from the one the
generated typedef names, so the generated C++ fails to compile — loudly, naming
the function — rather than calling anything else. Where one C++ name has *two*
declarations `autocxx` cannot tell apart — `std::less<>` beside the default
comparator, or two hashes of one `std::unordered_map` shape — both are refused
outright: there is no way to know which one a call was meant to reach. The
comparison sees the map through whatever spells it — a typedef, a pointer, a
by-value parameter — so writing one twin through an alias changes nothing.

That wall stands whichever route a call takes. Every function `cxx` declares
itself is bound by assigning its address to a function pointer of the declared
type, so the call can only ever reach a function of exactly that signature —
never an overload a conversion could carry the map into. A signature which
needs a wrapper of `autocxx`'s own, because something else in it does — a
static method, an rvalue-reference parameter, a `std::string_view` — gets the
same exactness by hand: the wrapper calls through a pointer of the declared
type too, so `f(Bait)` beside a map-taking `f` cannot quietly capture the call
through `Bait`'s converting constructor, and a pair its neighbours tell apart
only by spelling — `f(std::string_view)` beside `f(const std::string_view&)`,
`f(T&&)` beside `f(const T&&)` — still reaches the declaration each binding
was built from.

A function template of the same name is the third competitor, and the one no
argument type fences out on its own: where the bound declaration's shape was
erased, the plain declaration is not an exact match, the template's
specialization is, and the call would compile and quietly reach the template.
bindgen parses no item for a function template; a report `autocxx` takes while
the headers are parsed is what sees them, and a map-taking function which shares
its name with one — declared in its own namespace or class, or merged into its
class by a `using Base::pick;` — is refused whole. A `using` whose source
`autocxx` cannot audit for templates — a template-instantiation base, say —
refuses the same way, unjudged.

Constructors are the one call C++ gives no way to name exactly, so a map
parameter is served in a constructor only where the class declares no other
constructor which arguments could reach at all — the copy and move constructors
do not count, and nor does another constructor taking only a reference to a map
*provably* of another shape. Provably, because bindgen spells one C++ type more
than one way — `uint32_t` comes out as `u32` where `unsigned int` comes out as
`c_uint` — so only a different map template, or a key or value of another width
or signedness on every supported platform, says the sibling's reference cannot
bind the map the binding hands over. A reference `autocxx` cannot prove
different counts wherever its kind binds what the binding presents: any
reference beside a map taken by value or by non-`const` reference — the
argument presented is an rvalue or a mutable lvalue — and a `const` reference
even beside a map taken by `const` reference, a const lvalue being exactly what
it binds. An `M&&` or `M&` sibling beside that `const` reference stays
harmless — neither binds a const lvalue. Anything else,
a `T(Bait)` beside the map among them, is refused with an explanation. With
nothing else for direct-initialization to choose, a difference
`autocxx` cannot see — that transparent `std::less<>` again — is a loud build
failure rather than a quiet conversion into some other constructor.

One escape does not end at a wall, and is recorded here instead of promised
away: the template report sees what a scope *declares*, not what is visible in
it. A function template pulled into the function's own namespace by a
namespace-scope `using other::pick;` joins the overload set without declaring
anything the report can see — and where the bound declaration's shape was
erased, that transparent `std::less<>` once more, the generated call would
compile and quietly reach the template. A header which pairs a map-taking
function with a same-named template behind such a `using` is the one map shape
to keep away from `autocxx` by hand.
