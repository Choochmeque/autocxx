This example demonstrates the experimental C++ reference wrappers, enabled by
`safety!(unsafe_references_wrapped)`. They exist because C++ references may
alias and Rust references may not, which makes handing a C++ reference to
Rust as `&T` unsound; a wrapper is a pointer underneath and sidesteps the
problem, at the cost of being fiddlier to hold. Building this needs nightly
Rust, for the `arbitrary_self_types` feature.
