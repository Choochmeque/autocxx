To build this example:
* `git submodule update --init s2geometry abseil-cpp`
* `cargo run`

There are two submodules because s2geometry no longer vendors a copy of
Abseil and expects to find a real one on the include path. `s2geometry` is
pinned to v0.14.0 and `abseil-cpp` to LTS 20250814.2 - the newest patch of
the 20250814 LTS line, which is the release family that version
of s2geometry asks for. Neither library is built here: the example compiles a
single s2geometry source file and takes everything else from headers, so
`build.rs` compiles the assertions out with `NDEBUG` and `STRIP_LOG` rather
than linking Abseil's log library. It takes both, for reasons the comment
there sets out - read it before copying the arrangement.

Thanks to @nside for inspiring this example.
