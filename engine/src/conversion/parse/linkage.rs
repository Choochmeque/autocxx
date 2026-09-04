// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Recovery of a C++ variable's *linkage* from the mangled symbol name which
//! `bindgen` puts into a `#[link_name]` attribute.
//!
//! # Why we care
//!
//! We expose a C++ variable to Rust by re-exporting the `extern "C"` static
//! which `bindgen` declared for it, which works only if there is a symbol for
//! the linker to resolve. A variable with internal linkage - `static` at
//! namespace scope, `const` at namespace scope (which is implicitly `static`
//! in C++), or anything in an anonymous namespace - has no such symbol. It is
//! a separate object in every translation unit which includes the header, and
//! in a translation unit which doesn't use it the compiler emits nothing at
//! all. Re-exporting it produces an undefined symbol error at link time, which
//! is a poor way to learn that autocxx can't do this. See google/autocxx#93.
//!
//! # Why the mangled name
//!
//! `bindgen` has no `ParseCallbacks` hook reporting linkage, and its Rust
//! output for these two declarations is otherwise identical:
//!
//! ```cpp
//! extern const Bob BOB;  // external linkage: symbol `BOB`
//! const Bob BOB{10};     // internal linkage: symbol `_ZL3BOB`, or nothing
//! ```
//!
//! The Itanium ABI does encode the distinction, though, by prefixing `L` to
//! the unqualified name of an entity with internal linkage. That reaches us in
//! the `#[link_name]` attribute, so that's where we look.

/// The linkage of a C++ variable, as far as we were able to work it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CppLinkage {
    /// There is a symbol which Rust can link against.
    External,
    /// There is no symbol which Rust can link against, because the variable
    /// exists separately in each translation unit.
    Internal,
    /// We couldn't tell. The Microsoft ABI, in particular, decorates a
    /// variable with internal linkage exactly as it decorates one with
    /// external linkage, so on MSVC targets this is the answer for every
    /// variable, and an internal-linkage variable will fail at link time
    /// rather than being diagnosed here.
    Unknown,
}

/// Work out the linkage of a C++ variable from the mangled name which bindgen
/// gave us in a `#[link_name]` attribute, or from the absence of one.
///
/// bindgen omits the attribute when the symbol name matches the Rust
/// identifier, which under the Itanium ABI happens only for a variable at
/// global scope with external linkage - a namespace or class scope, or
/// internal linkage, all force a mangled name.
pub(crate) fn linkage_from_link_name(link_name: Option<&str>) -> CppLinkage {
    match link_name {
        None => CppLinkage::External,
        Some(link_name) => {
            // bindgen prefixes link names with \u{1} to stop rustc applying
            // its own mangling on top.
            let mangled_name = link_name.strip_prefix('\u{1}').unwrap_or(link_name);
            if mangled_name.starts_with('?') {
                // Microsoft ABI - see `CppLinkage::Unknown`.
                return CppLinkage::Unknown;
            }
            itanium_linkage(mangled_name)
        }
    }
}

/// The prefix which clang and gcc give to the namespace component standing for
/// an anonymous namespace. Everything within one has internal linkage.
const ANONYMOUS_NAMESPACE: &str = "_GLOBAL__N_";

/// Itanium C++ ABI (Linux, macOS, MinGW, and everything else which isn't
/// MSVC).
///
/// Entities with internal linkage are mangled as if they had external linkage
/// except that `L` is prefixed to their unqualified name:
///
/// ```text
/// static Bob BOB;                        -> _ZL3BOB
/// namespace ns { static Bob BOB; }       -> _ZN2nsL3BOBE
/// namespace ns { extern const Bob BOB; } -> _ZN2ns3BOBE
/// struct Anna { static Bob BOB; };       -> _ZN4Anna3BOBE
/// ```
///
/// so we look for that `L`. We only understand names made up of plain
/// `<source-name>` components (`<length><identifier>`), which is all a
/// variable's name can be unless it's a member of a template specialization;
/// anything else yields [`CppLinkage::Unknown`] so that we fall back to
/// letting the linker have the last word.
fn itanium_linkage(mangled_name: &str) -> CppLinkage {
    // Mach-O prepends an extra underscore to every symbol, so the same name is
    // `_ZN...` on Linux and `__ZN...` on macOS.
    let rest = match mangled_name
        .strip_prefix("__Z")
        .or_else(|| mangled_name.strip_prefix("_Z"))
    {
        Some(rest) => rest,
        // Not an Itanium mangled name at all, so we have no idea.
        None => return CppLinkage::Unknown,
    };
    let mut rest = match rest.strip_prefix('N') {
        // `<nested-name>`: a variable within a namespace or a class.
        Some(rest) => rest.as_bytes(),
        // Otherwise the whole thing should be the unqualified name of a
        // variable at global scope, so an `L` here is the internal linkage
        // marker.
        None => {
            let rest = rest.as_bytes();
            return match rest.split_first() {
                Some((b'L', tail)) if starts_with_source_name(tail) => CppLinkage::Internal,
                _ if starts_with_source_name(rest) => CppLinkage::External,
                _ => CppLinkage::Unknown,
            };
        }
    };
    // `<CV-qualifiers> ::= [r] [V] [K]`, always in that order.
    for cv in b"rVK" {
        if rest.first() == Some(cv) {
            rest = &rest[1..];
        }
    }
    // Then a sequence of `<source-name>` components, terminated by `E`.
    loop {
        match rest.split_first() {
            // The nested name ended without our having seen an `L`.
            Some((b'E', _)) => return CppLinkage::External,
            // `L` prefixed to the entity's own unqualified name.
            Some((b'L', tail)) if starts_with_source_name(tail) => return CppLinkage::Internal,
            Some((b'0'..=b'9', _)) => match take_source_name(rest) {
                Some((name, tail)) => {
                    if name.starts_with(ANONYMOUS_NAMESPACE.as_bytes()) {
                        return CppLinkage::Internal;
                    }
                    rest = tail;
                }
                None => return CppLinkage::Unknown,
            },
            // A substitution, template arguments, an operator name...  none of
            // which we can walk over, so stop guessing.
            _ => return CppLinkage::Unknown,
        }
    }
}

/// Whether these bytes begin a `<source-name> ::= <length> <identifier>`.
fn starts_with_source_name(mangled_name: &[u8]) -> bool {
    matches!(mangled_name.first(), Some(b'1'..=b'9'))
}

/// Split off a leading `<source-name>`, returning the identifier and whatever
/// follows it.
fn take_source_name(mangled_name: &[u8]) -> Option<(&[u8], &[u8])> {
    let digits = mangled_name
        .iter()
        .position(|c| !c.is_ascii_digit())
        .unwrap_or(mangled_name.len());
    let len: usize = std::str::from_utf8(&mangled_name[..digits])
        .ok()?
        .parse()
        .ok()?;
    let rest = &mangled_name[digits..];
    if rest.len() < len {
        return None;
    }
    Some(rest.split_at(len))
}

#[cfg(test)]
mod tests {
    use super::{linkage_from_link_name, CppLinkage};

    /// Every mangled name below was produced by clang for the corresponding
    /// C++ declaration, so that these tests pin real ABI output rather than
    /// our idea of it.
    fn check(mangled_name: &str, expected: CppLinkage) {
        assert_eq!(
            linkage_from_link_name(Some(mangled_name)),
            expected,
            "{mangled_name}"
        );
        // bindgen always hands us the name with a \u{1} prefix, and Mach-O
        // adds an underscore on top of that.
        assert_eq!(
            linkage_from_link_name(Some(&format!("\u{1}{mangled_name}"))),
            expected,
            "\\u{{1}}{mangled_name}"
        );
        assert_eq!(
            linkage_from_link_name(Some(&format!("\u{1}_{mangled_name}"))),
            expected,
            "\\u{{1}}_{mangled_name}"
        );
    }

    #[test]
    fn test_no_link_name_is_external() {
        // bindgen only omits the attribute for an unmangled name, which under
        // the Itanium ABI means global scope and external linkage.
        assert_eq!(linkage_from_link_name(None), CppLinkage::External);
    }

    #[test]
    fn test_external() {
        // namespace ns { extern const Bob e_ns; }
        check("_ZN2ns4e_nsE", CppLinkage::External);
        // struct Anna { static Bob mem; };
        check("_ZN4Anna3memE", CppLinkage::External);
        // namespace a { namespace b { extern Bob deep; } }
        check("_ZN1a1b4deepE", CppLinkage::External);
    }

    #[test]
    fn test_internal() {
        // static Bob s_global;
        check("_ZL8s_global", CppLinkage::Internal);
        // const Bob c_global = Bob{2};
        check("_ZL8c_global", CppLinkage::Internal);
        // namespace ns { static Bob s_ns; }
        check("_ZN2nsL4s_nsE", CppLinkage::Internal);
        // namespace ns { const Bob c_ns = Bob{4}; }
        check("_ZN2nsL4c_nsE", CppLinkage::Internal);
        // namespace { Bob anon; }
        check("_ZN12_GLOBAL__N_14anonE", CppLinkage::Internal);
        // namespace { const Bob c_anon = Bob{7}; }
        check("_ZN12_GLOBAL__N_16c_anonE", CppLinkage::Internal);
    }

    #[test]
    fn test_unknown() {
        // Microsoft ABI: `const Bob BOB` and `extern const Bob BOB` are
        // decorated identically, so we can never tell.
        check("?BOB@@3UBob@@B", CppLinkage::Unknown);
        // A member of a template specialization involves template arguments,
        // which we make no attempt to walk over.
        check("_ZN1AIiE3memE", CppLinkage::Unknown);
        // A static local inside a function is mangled as `_ZZ<function>E...`,
        // which is neither of the shapes we understand.
        check("_ZZ4funcvE3var", CppLinkage::Unknown);
        // Not a mangled name at all.
        check("some_c_symbol", CppLinkage::Unknown);
    }
}
