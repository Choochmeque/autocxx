# Examples

* [Demo](https://github.com/google/autocxx/tree/main/demo) - simplest possible demo
* [POD example](https://github.com/Choochmeque/autocxx/tree/main/examples/pod) - plain-old-data types, passed by value
* [Non-trivial type on the stack](https://github.com/Choochmeque/autocxx/tree/main/examples/non-trivial-type-on-stack) - `moveit!` and C++ objects on the Rust stack
* [S2 example](https://github.com/google/autocxx/tree/main/examples/s2) - example using S2 geometry library
* [Steam example](https://github.com/google/autocxx/tree/main/examples/steam-mini) - example using (something like) the Steam client library
* [LLVM example](https://github.com/Choochmeque/autocxx/tree/main/examples/llvm) - wrapping part of LLVM, a large real-world header set
* [Subclass example](https://github.com/google/autocxx/tree/main/examples/subclass) - example using subclasses
* [Chromium RenderFrameHost example](https://github.com/Choochmeque/autocxx/tree/main/examples/chromium-fake-render-frame-host) - a memory-safe handle to an object C++ owns
* [C++ calling Rust](https://github.com/Choochmeque/autocxx/tree/main/examples/cpp_calling_rust) - the other direction, plus C++ exceptions as `Result`
* [Reference wrappers](https://github.com/Choochmeque/autocxx/tree/main/examples/reference-wrappers) - the experimental `unsafe_references_wrapped` safety policy; needs nightly
* [Integration tests](https://github.com/google/autocxx/blob/main/integration-tests/tests/integration_test.rs)
  - hundreds of small snippets

Contributions of more examples to the `examples` directory are much appreciated!
