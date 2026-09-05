This example deals with plain-old-data types. `Rect` and `Point` are simple
enough that autocxx can lay them out identically on both sides, so
`generate_pod!` gives you an ordinary Rust struct: build one with a struct
literal, read its fields, pass it to C++ by value, no `UniquePtr` anywhere.
