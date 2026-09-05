This example runs the traffic the other way: C++ calling into Rust, using
`extern_rust_type` and `extern_rust_function`. That is not what autocxx is
mainly for and the support is immature, so reach for cxx or cbindgen if it's
the bulk of what you need. The example also shows the `throws!` directive,
which turns a C++ exception into a `Result` rather than an unwind into Rust.
