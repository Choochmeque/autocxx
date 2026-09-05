Published automatically to https://google.github.io/autocxx/ from master branch.

To build and view locally:

- Install [mdBook] and some preprocessors: `cargo install mdbook mdbook-mermaid mdbook-linkcheck`.
- Build our custom preprocessor, and the helper it builds the book's code
  examples in: `cargo build -p autocxx-mdbook-preprocessor -p autocxx-integration-tests`
- Run `mdbook build` in this directory.
- Open the generated *build/html/index.html*.

[mdBook]: https://github.com/rust-lang/mdBook
