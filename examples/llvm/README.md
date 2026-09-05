This example wraps `llvm::MemoryBuffer` and re-exports it, as a sketch of
putting a Rust library on top of a large pre-existing C++ one. `build.rs`
finds the headers by asking `llvm-config` - a versioned one such as
`llvm-config-18` if your distribution installed it that way, otherwise
whatever `llvm-config` is on `PATH`, or whatever `LLVM_CONFIG_PATH` names.
Install your distribution's `llvm-<version>-dev` package and `cargo build`
should find it.

It is not built in CI. The old job hardcoded `apt-get install llvm-13-dev`
and LLVM 13 is no longer packaged for the runner images, but the header
search is no longer the obstacle: the example does build against a current
LLVM, and then trips rustc's `unnecessary_transmutes` lint on the bitfield
accessors bindgen generates for `llvm::ErrorOr`. The examples job runs with
`-Dwarnings`, so that is a hard error there. A crate-local `allow` could
paper over it (the job denies warnings rather than forbidding them), but
the offending code comes out of bindgen, into the private `mod bindgen`
that autocxx generates - so the project's choice is to fix it once in the
engine rather than suppress it per consumer.
