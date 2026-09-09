# Naming

## Namespaces

The C++ namespace structure is reflected in mods within the generated
ffi mod. However, at present there is an internal limitation that
autocxx can't handle multiple types with the same identifier, even
if they're in different namespaces. This will be fixed in future.

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"
void generations::hey_boomer() {}
void submarines::hey_boomer() {}",
"
namespace generations {
  void hey_boomer();
}
namespace submarines {
  void hey_boomer();
}
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate!("submarines::hey_boomer")
    generate!("generations::hey_boomer")
}

fn main() {
    ffi::generations::hey_boomer(); // insults your elders and betters
    ffi::submarines::hey_boomer(); // launches missiles
}
}
)
```

## Anonymous namespaces

An anonymous namespace is transparent: C++ defines `namespace { ... }` as a
namespace whose name nothing can write, followed by a using-directive for it, so
its members are found by lookup in the namespace which encloses them. autocxx
names them the same way, so a type declared in an anonymous namespace at the top
of a header appears in the root of the `ffi` mod, and one inside
`namespace foo { namespace { ... } }` appears in `ffi::foo`. `generate!` names
them the same way too.

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"",
"
#include <cstdint>
namespace {
struct Badger { uint32_t stripes; };
}
inline uint32_t count_stripes(Badger b) { return b.stripes; }
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate_pod!("Badger")
    generate!("count_stripes")
}

fn main() {
    assert_eq!(ffi::count_stripes(ffi::Badger { stripes: 3 }), 3);
}
}
)
```

An anonymous namespace may declare a name the namespace around it also declares,
which is legal because the two are different scopes. Those are two different
entities, and C++ resolves the qualified name to the enclosing one -
`::LIMIT` below is `2` - because a using-directive is consulted only where direct
lookup found nothing:

```cpp
namespace { constexpr int LIMIT = 1; }
constexpr int LIMIT = 2;
```

autocxx answers the same way. The enclosing declaration keeps the name, and the
one in the anonymous namespace stays where `bindgen` put it, under a name
nothing can write - which is what C++ says about it too. Two *types* under one
name are the exception: `bindgen` files a type under whichever namespace first
mentioned it, so it cannot say which of the two really declared the name, and
both are turned down rather than one of them guessed at.

Two more things do not follow. A *function* declared in an anonymous namespace
has internal linkage, and so is not generated at all - there is no symbol outside
the translation unit which defines it for Rust to call. And an anonymous
namespace declares a type which is a *different type in every translation unit*
that includes the header; the binding is to the one in the C++ autocxx
generates, so C++ of your own which passes such an object to a generated
function is handing over a type which is formally not the same one, even though
it is laid out identically. A type you intend to share between your C++ and Rust
belongs in a named namespace.

## Nested types

There is support for generating bindings of nested types, with some
restrictions. Currently the C++ type `A::B` will be given the Rust name
`A_B` in the same module as its enclosing namespace.

```rust,ignore,autocxx,hidecpp
autocxx_integration_tests::doctest(
"",
"
struct Turkey {
    struct Duck {
        struct Hen {
            int wings;
        };
    };
};
",
{
use autocxx::prelude::*;

include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)
    generate_pod!("Turkey_Duck_Hen")
}

fn main() {
    let _turducken = ffi::Turkey_Duck_Hen::new().within_box();
}
}
)
```

## Overloads

See [the chapter on C++ functions](cpp_functions.md).
