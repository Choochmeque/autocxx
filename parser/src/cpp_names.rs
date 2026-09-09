// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Shapes of C++ names which directives accept.

/// Whether `name` is a `::`-separated run of plain Rust/C++ identifiers, and
/// so means only itself when `bindgen` reads it as a regex.
///
/// Directives which have to recognize the same type again later - to know not
/// to synthesize constructors for something which is really an enum, or which
/// bindgen definition to put a `derive!`'s traits on - match the name
/// literally. Rather than let the two readings drift apart silently, they
/// insist on a name that means the same thing either way.
pub(crate) fn is_plain_qualified_name(name: &str) -> bool {
    !name.is_empty()
        && name.split("::").all(|segment| {
            let mut chars = segment.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_qualified_names() {
        assert!(is_plain_qualified_name("Flags"));
        assert!(is_plain_qualified_name("_Flags2"));
        assert!(is_plain_qualified_name("ns::Flags"));
        assert!(is_plain_qualified_name("Holder_Inner"));
        assert!(!is_plain_qualified_name(""));
        assert!(!is_plain_qualified_name("Flags.*"));
        assert!(!is_plain_qualified_name(".*"));
        assert!(!is_plain_qualified_name("ns::"));
        assert!(!is_plain_qualified_name("2Flags"));
    }
}
