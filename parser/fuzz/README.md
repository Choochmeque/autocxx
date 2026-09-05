Fuzz target for `autocxx-parser`, tracking [issue #1244](https://github.com/google/autocxx/issues/1244).

`fuzz_targets/parse_include_cpp.rs` feeds arbitrary strings to
`autocxx_parser::IncludeCppConfig`'s `syn::parse::Parse` implementation - the
code that parses the directives inside `include_cpp! { ... }` (`generate!`,
`safety!`, `include!`, and so on). That's the first thing arbitrary macro
input reaches, and it's pure in-process token-tree parsing (no clang/bindgen).

What it primarily looks for is parser panics and resource exhaustion - a proc
macro that panics gives the user an opaque "proc macro panicked" instead of
this crate's normal, specific diagnostics, and one that hangs or allocates
without bound is just as much a bug. `autocxx-parser` itself forbids unsafe
code, so AddressSanitizer adds little inside that crate; it can still catch
memory errors in `syn`, `proc_macro2`, the allocator and the rest of the
dependency graph, which is why the CI job leaves cargo-fuzz's default ASan
on. Safe Rust is no guarantee of no crash either way: OOM, stack overflow
and hangs are all still reachable from here.

Parsing alone would be a thin target, though: `IncludeCppConfig`'s `Parse`
impl reports every failure as a `syn::Error` and never panics. The parser's
panics are all one step later, in the accessors the engine reads the config
through - `bindgen_allowlist`, `is_on_allowlist` and the `ToTokens` impl each
`unreachable!()`/`panic!()` when the allowlist is still `Unspecified`, which
is the state an `include_cpp!` with no `generate!` of any kind parses into.
`IncludeCppConfig::confirm_complete` is what settles that, and
`engine/src/parse_file.rs` calls it on every parsed macro before anything
else looks at the config.

So after a successful parse the target does the same: `confirm_complete`,
then the accessors the engine really calls, in the order it calls them. That
ordering is the point. A panic reached that way is one a user could reach by
writing an `include_cpp!` block; a panic reached by calling those accessors
without `confirm_complete` first would just be the harness abusing the API,
and would say nothing about anyone's real code. Following the engine's call
pattern is what makes a crash here worth acting on.

## Running it

This needs [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz)
(`cargo install cargo-fuzz`) and a nightly toolchain, per cargo-fuzz's own
requirements. From `parser/fuzz`:

```
cargo fuzz run parse_include_cpp
```

`.github/workflows/fuzz.yml` does exactly that: weekly, and on demand via
`workflow_dispatch`, where the `duration` input takes the number of seconds to
fuzz for (set it to something like 60 for a smoke test). Before fuzzing it
replays the committed corpus with `-runs=0`, so a seed that stops parsing
fails as itself rather than as a crash mid-campaign. A crashing input is
uploaded as a build artifact so it can be turned into a regression test.

`fuzz` deliberately isn't a member of the main workspace (see the `Cargo.toml`
here), so it's untouched by `cargo build --workspace`/`cargo test
--workspace`/CI and only comes into play if you cd into this directory.

## The seed corpus

`corpus/parse_include_cpp` is committed, so every run - local or CI - starts
from the same inputs. It holds 337 files, ~30KB in total, all but one of them a
directive body that `IncludeCpp::parse` sees somewhere in this repository, and
each named after the SHA-1 of its contents, which is how libFuzzer names corpus
entries itself. They come from:

* every distinct `include_cpp! { ... }` written out in full anywhere in the
  tree - the examples, the demo, `gen/cmd`'s test data, the book, and the
  worked examples in doc comments;
* every distinct body the integration-test harness synthesises, which is where
  the bulk of them come from: `integration-tests/src/lib.rs` wraps each test's
  `generate!`/`generate_pod!` list, or its explicit directive token stream, in
  `#include "input.h"` and `safety!(...)`, and that whole thing is what the
  parser is handed.

All 337 parse successfully, which is the point of seeding from real usage: the
mutator starts from inputs that reach the far side of the parser rather than
from inputs that die in the first token.

Three directives are in the parser but appear nowhere in the tree as anything
a user would write: `block_constructors!`, `rust_type!` and
`extern_rust_function!` (the last is only ever emitted by autocxx itself, into
a reproduction case). One hand-written seed covers those, using the signatures
their documentation gives.

Run locally, `cargo fuzz run` writes newly-discovered inputs into whichever
corpus directory you point it at. Those are untracked; commit one only if
it's worth carrying. The CI job points it at `work-corpus/` instead, so that
`cargo fuzz cmin` can never rewrite tracked files, and lets its cache carry
the rest between runs.

## The dictionary

`parse_include_cpp.dict` is a libFuzzer dictionary of the directive grammar:
every directive registered in `get_directives()` in
`parser/src/directives.rs`, spelled the way the parser expects to see it,
plus the safety policies and the punctuation a directive is built from. This
grammar is keyword-driven and the keywords are long, so a mutator without the
dictionary spends its budget rediscovering `extern_cpp_opaque_type` one byte
at a time. Pass it with `-dict=parse_include_cpp.dict`, as the CI job does.
Keep it in sync when a directive is added or renamed.

## Fuzzing without cargo-fuzz

`cargo build` here with plain stable Rust produces a working libFuzzer binary
at `target/release/parse_include_cpp`, and libFuzzer's flags work on it. It
isn't coverage-instrumented, though - that instrumentation is what `cargo fuzz
run` adds - and libFuzzer without coverage feedback is close to useless: it
discards the entire seed corpus at startup (no input registers as
interesting), keeps a single one-byte input, and mutates that. `-keep_seed=1`
at least keeps the seeds and mutates from them, but there's still no feedback
telling it which mutations got anywhere.

Real coverage-guided fuzzing doesn't actually need cargo-fuzz or nightly:
SanitizerCoverage is reachable through stable `rustc` flags. Naming the host
target explicitly is what keeps `RUSTFLAGS` off the build scripts, which would
otherwise be instrumented with no libFuzzer runtime to link against:

```
RUSTFLAGS="-Cpasses=sancov-module \
  -Cllvm-args=-sanitizer-coverage-level=4 \
  -Cllvm-args=-sanitizer-coverage-inline-8bit-counters \
  -Cllvm-args=-sanitizer-coverage-pc-table \
  -Cllvm-args=-sanitizer-coverage-trace-compares" \
  cargo build --release --target "$(rustc -vV | sed -n 's/^host: //p')"
```

Then run the binary directly, giving it a scratch directory to write to and
the seed corpus to read, with the same dictionary and length cap the CI job
uses - the grammar is keyword-driven, so the dictionary saves the mutator
from rediscovering `extern_cpp_opaque_type` a byte at a time:

```
mkdir /tmp/fuzz-out
./target/<host>/release/parse_include_cpp /tmp/fuzz-out corpus/parse_include_cpp \
  -dict=parse_include_cpp.dict \
  -max_len=1024 \
  -timeout=25 \
  -rss_limit_mb=2048 \
  -max_total_time=900
```

What this recipe does *not* give you is AddressSanitizer, which is the part
that genuinely needs nightly. For this target that mostly costs coverage of
the dependencies - `autocxx-parser` forbids unsafe code itself - but it is a
real gap, not a free one, so treat a clean run here as weaker evidence than a
clean `cargo fuzz run`.
