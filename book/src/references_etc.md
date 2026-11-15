# Pointers, references, values

`autocxx` knows how to deal with C++ APIs which take C++ types:
* By value
* By reference (const or not)
* By raw pointer
* By `std::unique_ptr`
* By `std::shared_ptr`
* By `std::weak_ptr`
* By rvalue reference (that is, as a move parameter)

(all of this is because the underlying [`cxx`](https://cxx.rs) crate has such versatility).
Some of these have some quirks in the way they're exposed in Rust, described below.

## Passing between C++ and Rust by value

See the section on [C++ types](cpp_types.md) for the distinction between POD and non-POD types.
POD types can be passed around however you like. Non-POD types can be passed into functions
in various ways - see [calling C++ functions](cpp_functions.md) for more details.

## References and pointers

We follow [`cxx`](https://cxx.rs) norms here. Specifically:

* A C++ reference becomes a Rust reference
* A C++ pointer becomes a Rust pointer.
* If a reference is returned with an ambiguous lifetime, we don't generate
  code for the function
* Pointers require use of `unsafe`, references don't necessarily.

That last point is key. If your C++ API takes pointers, you're going
to have to use `unsafe`. Similarly, if your C++ API returns a pointer,
you'll have to use `unsafe` to do anything useful with the pointer in Rust.
This is intentional: a pointer from C++ might be subject to concurrent
mutation, or it might have a lifetime that could disappear at any moment.
As a human, you must promise that you understand the constraints around
use of that pointer and that's what the `unsafe` keyword is for.

Exactly the same issues apply to C++ references _in theory_, but in practice,
they usually don't. Therefore [`cxx`](https://cxx.rs) has taken the view that we can "trust"
a C++ reference to a higher degree than a pointer, and autocxx follows that
lead (in fact we 'trust' references even slightly more than cxx).
In practice, of course, references are rarely return values from C++
APIs so we rarely have to navel-gaze about the trustworthiness of a
reference.

(See also the discussion of [`safety`](safety.md) - if you haven't specified
an unsafety policy, _all_ C++ APIs require `unsafe` so the discussion is moot.

If you're given a C++ object by pointer, and you want to interact with it,
you'll need to figure out the guarantees attached to the C++ object - most
notably its lifetime. To see some of the decision making process involved
see the [Steam example](https://github.com/Choochmeque/autocxx/tree/main/examples/steam-mini/src/main.rs).

## Saying which parameter a returned reference borrows from

"Ambiguous lifetime" above means: more than one input reference. A returned
reference borrows from one of the inputs, and where there are several, nothing
in the C++ declaration says which - so `autocxx` declines the function rather
than guess.

The receiver counts as an input reference, which is why the chainable setter
gets no binding at all:

```cpp
class Settings {
public:
    Settings& set(const Key& key, const Value& value);  // returns *this
};
```

Three input references - `this`, `key` and `value` - so `set` is declined, and
so is every other setter on the class. The builder idiom runs into this in
general.

You know which one it is. Say so:

```rust,ignore
returns_borrow_from!("Settings::set", "self")
```

and the setter binds, with the lifetime it should have:

```rust,ignore
pub fn set<'a>(self: Pin<&'a mut Settings>, key: &Key, value: &Value)
    -> Pin<&'a mut Settings>;
```

Only the receiver's lifetime comes back out. The key and the value are lent for
the call alone, so a chain can be written over temporaries, and the reference
the chain ends with cannot outlive the `Settings` it came from - which is what
the C++ means.

The parameter is named as `"self"` (or `"this"`) for the receiver, by the name
C++ gives it, or by position - `"#0"` for the first declared parameter, not
counting the receiver. Positions are there for the parameter C++ declared
without a name: such a parameter does reach Rust, as `arg1` or `arg2`, but that
number counts the unnamed parameters rather than the declared ones, so it is
not the position and must not be written as though it were.

### It is a promise, not a deduction

`autocxx` cannot read the C++ body. This directive is your word about what the
function does, and it carries the same weight as
[`safety!(unsafe_ffi)`](safety.md): if the reference actually points into the
*other* parameter, then safe Rust - with no `unsafe` block anywhere near the
call - is holding a reference which may already dangle, and the compiler has
been told that is fine.

So write it for a function you have read, or whose documentation says where the
returned reference points. What `autocxx` does check is whether the promise is
expressible at all, and it refuses rather than emit a signature which lies:

* the named parameter has to be a reference itself - one passed by value, as a
  pointer, or as an rvalue reference has no lifetime to give away;
* it has to reach C++ as the caller's own borrow. A parameter `autocxx`
  rebuilds for the call - a `const std::string_view&`, which arrives as
  anything viewable as bytes and lends C++ a temporary view over them - has
  no lifetime that outlives the call for the returned reference to take;
* a returned mutable reference may not be promised out of a `const` parameter,
  which would be a `&mut` derived from a `&`.

The directive claims the whole overload set of the name, as `block_functions!`
does, and it is monotone: an overload with no parameter answering to the name
is analysed exactly as if the directive were not written, so one that bound on
its own still binds, and one still declined gets a stub saying the directive
didn't cover it. `"self"` resolves on nothing a static member overload has, so
such an overload is likewise left alone - but naming `"self"` for a free
function is refused outright, since no overload of one could ever have a
receiver. An overload returning something other than a reference has no
lifetime for the directive to be about, and is bound as it would have been
without one.

A directive which reaches no reference-returning function at all is a build
error - whether the name matched nothing, or nothing it matched returns a
reference.

Under `safety!(unsafe_references_wrapped)` the directive does nothing. That mode
hands a returned reference back as a `CppLtRef` with a lifetime of its own and
passes reference parameters as the lifetime-free `CppRef`, so there is no
parameter lifetime for a promise to name, and the functions that mode declines
stay declined.

## [`cxx::UniquePtr`](https://docs.rs/cxx/latest/cxx/struct.UniquePtr.html)s tips

We use [`cxx::UniquePtr`](https://docs.rs/cxx/latest/cxx/struct.UniquePtr.html) in completely the normal way, but there are a few
quirks which you're more likely to run into with `autocxx`.

* You'll need to use [`.pin_mut()`](https://docs.rs/cxx/latest/cxx/struct.UniquePtr.html#method.pin_mut) a lot -
  see [the example at the bottom of C++ functions](cpp_functions.md).
* If you need to pass a raw pointer to a function, lots of unsafety is required - something like this:
  ```rust,ignore
     let mut a = ffi::A::make_unique();
     unsafe { ffi::TakePointerToA(std::pin::Pin::<&mut ffi::A>::into_inner_unchecked(a.pin_mut())) };
  ```
  This may be simplified in future.
