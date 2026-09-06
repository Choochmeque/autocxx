This example wraps `llvm::MemoryBuffer` and re-exports it, as a sketch of
putting a Rust library on top of a large pre-existing C++ one. `build.rs`
finds the headers by asking `llvm-config` - a versioned one such as
`llvm-config-18` if your distribution installed it that way, otherwise
whatever `llvm-config` is on `PATH`, or whatever `LLVM_CONFIG_PATH` names.
Install your distribution's `llvm-<version>-dev` package and `cargo build`
should find it.

CI builds this example on the Ubuntu legs against the distribution's
`llvm-dev`. It spent some years out of CI: the old job hardcoded
`apt-get install llvm-13-dev` until that stopped existing, and once the
header search was rewritten to ask `llvm-config`, the build tripped
rustc's `unnecessary_transmutes` lint on the bitfield accessors bindgen
generated for `llvm::ErrorOr` - which is exactly the kind of thing this
example exists to catch, being the only one that pushes autocxx through
a large real-world header set. The engine now generates those accessors
with casts instead, and the example is back on duty.
