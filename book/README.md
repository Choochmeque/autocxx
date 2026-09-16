Published automatically to https://autocxx.dev/ from the main branch.

To build and view locally:

- Install [mdBook] and some preprocessors, at the versions CI uses:
  `cargo install mdbook --version 0.5.2 && cargo install mdbook-mermaid --version 0.17.1 && cargo install mdbook-linkcheck2 --version 0.13.0`.
- Build our custom preprocessor, and the helper it builds the book's code
  examples in: `cargo build -p autocxx-mdbook-preprocessor -p autocxx-integration-tests`
- Run `mdbook build` in this directory.
- Open the generated *build/html/index.html*.

[mdBook]: https://github.com/rust-lang/mdBook
