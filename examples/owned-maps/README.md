# Owned `std::map`

A C++ settings store whose API puts a `std::map` in every position one can
appear in: returned by value, returned by `const` reference, taken by `const`
reference, and taken by mutable reference so C++ writes through it.

Two instantiations are involved — `std::map<std::string, std::string>` and
`std::map<uint32_t, uint32_t>` — and each becomes a generated opaque Rust type
of its own. The `concrete!` directives give them the names `Settings` and
`Limits`; without those they would still exist, under the names autocxx derives
from the C++ spellings.

```sh
cargo run
```

See the [built-in types chapter](https://autocxx.dev/book/primitives.html) for
what those types can do and which maps are refused.
