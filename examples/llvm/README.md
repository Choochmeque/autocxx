This example wraps `llvm::MemoryBuffer` and re-exports it, as a sketch of
putting a Rust library on top of a large pre-existing C++ one. It is not
built in CI: the job used to `apt-get install llvm-13-dev`, and LLVM 13 is no
longer packaged for the Ubuntu images the runners use. To build it locally
you will need LLVM development headers, and an edit to `build.rs`, which
hardcodes `/usr/include/llvm-13` and `/usr/include/llvm-c-13`.
