// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Recovery of C++ *ref-qualifiers* (`void foo() &` / `void foo() &&`) from the
//! mangled symbol names which `bindgen` puts into `#[link_name]` attributes.
//!
//! # Why this lives here rather than in bindgen
//!
//! `bindgen` models a member function's implicit object parameter as nothing
//! more than a `this` pointer, and it decides the constness of that pointer
//! using `clang_CXXMethod_isConst`. It never calls
//! `clang_Type_getCXXRefQualifier`, and it has no `ParseCallbacks` hook which
//! would report a ref-qualifier to us. Consequently, in the Rust which bindgen
//! hands to autocxx, these two declarations are byte-for-byte identical:
//!
//! ```cpp
//! void foo() &;
//! void foo();
//! ```
//!
//! ```rust,ignore
//! pub fn A_foo(this: *mut root::A);
//! ```
//!
//! The one place the distinction survives is the mangled name, because every
//! C++ ABI encodes the ref-qualifier as part of the function's type. bindgen
//! emits that as `#[link_name = "..."]`, so we recover the qualifier by
//! inspecting it. See google/autocxx#837.
//!
//! # Why this matters
//!
//! `cxx` calls a method by forming a pointer-to-member-function, and a
//! pointer-to-member type cannot express a ref-qualifier, so the generated C++
//! does not compile:
//!
//! ```text
//! error: cannot initialize a variable of type 'void (A::*)()' with an rvalue
//!        of type 'void (A::*)() &'
//!   void (::A::*foo$)() = &::A::foo;
//! ```
//!
//! Knowing the qualifier lets us generate a C++ wrapper which calls the method
//! directly (fine for `&`), or decline to generate anything at all (`&&`).

/// The *ref-qualifier* of a C++ member function; that is, what restriction the
/// function places on the value category of the object it is called on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CppRefQualifier {
    /// No ref-qualifier: callable on lvalues and rvalues alike. This is also
    /// what we assume whenever we can't tell (see the module docs for the
    /// cases in which detection is not possible).
    #[default]
    None,
    /// `void foo() &` (possibly also cv-qualified, e.g. `void foo() const &`).
    /// Callable only on lvalues, which is exactly what autocxx has anyway.
    LValue,
    /// `void foo() &&` (possibly also cv-qualified). Callable only on rvalues.
    RValue,
}

/// Work out the ref-qualifier of a member function from the mangled name which
/// bindgen gave us in a `#[link_name]` attribute.
///
/// Returns [`CppRefQualifier::None`] for anything we don't recognise, on the
/// basis that assuming "no ref-qualifier" leaves behaviour exactly as it was
/// before we started doing this.
pub(crate) fn ref_qualifier_from_mangled_name(mangled_name: &str) -> CppRefQualifier {
    // bindgen prefixes link names with \u{1} to stop rustc applying its own
    // mangling on top.
    let mangled_name = mangled_name.strip_prefix('\u{1}').unwrap_or(mangled_name);
    match mangled_name.as_bytes().first() {
        Some(b'?') => msvc_ref_qualifier(mangled_name),
        _ => itanium_ref_qualifier(mangled_name),
    }
}

/// Itanium C++ ABI (Linux, macOS, MinGW, and everything else which isn't MSVC).
///
/// Section 5.1.3 of the ABI says:
///
/// ```text
/// <nested-name>   ::= N [<CV-qualifiers>] [<ref-qualifier>] <prefix> <unqualified-name> E
///                 ::= N [<CV-qualifiers>] [<ref-qualifier>] <template-prefix> <template-args> E
/// <CV-qualifiers> ::= [r] [V] [K]
/// <ref-qualifier> ::= R   # & ref-qualifier
///                 ::= O   # && ref-qualifier
/// ```
///
/// Non-static member functions are always mangled as a `<nested-name>`, so we
/// look for `_ZN`, step over any cv-qualifiers, and see what's next.
fn itanium_ref_qualifier(mangled_name: &str) -> CppRefQualifier {
    // Mach-O prepends an extra underscore to every symbol, so the same name is
    // `_ZN...` on Linux and `__ZN...` on macOS.
    let rest = match mangled_name
        .strip_prefix("__Z")
        .or_else(|| mangled_name.strip_prefix("_Z"))
    {
        Some(rest) => rest,
        None => return CppRefQualifier::None,
    };
    let mut rest = match rest.strip_prefix('N') {
        Some(rest) => rest.as_bytes(),
        // Not a <nested-name>, so not a member function, so no ref-qualifier.
        None => return CppRefQualifier::None,
    };
    // `<CV-qualifiers> ::= [r] [V] [K]`, always in that order.
    for cv in b"rVK" {
        if rest.first() == Some(cv) {
            rest = &rest[1..];
        }
    }
    // An `R` or `O` here is unambiguously a ref-qualifier: the `<prefix>` which
    // would otherwise appear at this point begins with a digit (a
    // `<source-name>`), a lowercase letter (an `<operator-name>`), or one of
    // `C`, `D`, `L`, `S`, `T`, `U` or `Z`. None of those is `R` or `O`.
    match rest.first() {
        Some(b'R') => CppRefQualifier::LValue,
        Some(b'O') => CppRefQualifier::RValue,
        _ => CppRefQualifier::None,
    }
}

/// Microsoft ABI.
///
/// A member function is mangled as:
///
/// ```text
/// ?<name>@<enclosing scopes, innermost first>@@<function class><ext qualifiers>[<ref-qualifier>]<cv><calling convention>...
/// ```
///
/// where the ref-qualifier is `G` for `&` and `H` for `&&`. For example, on
/// x86-64, `void A::foo() &` is `?foo@A@@QEGAAXXZ`: `Q` (public non-static),
/// `E` (`__ptr64`), `G` (`&`), `A` (no cv-qualifiers), `A` (`__cdecl`).
///
/// # Limitation
///
/// We locate the end of the qualified name by looking for the first `@@`,
/// which is only sound if no part of that name is itself a mangled name.
/// Templated classes (`?tm@?$C@H@@...`) and special member/operator names
/// (`??0A@@...`, `??RA@@...`) break that assumption, so we decline to guess
/// for any name containing a `?` before the first `@@`, and report
/// [`CppRefQualifier::None`]. A ref-qualified `operator()` on MSVC therefore
/// still generates C++ which doesn't compile; methods of templated classes are
/// discarded by autocxx as generic types long before we get here, so the gap
/// there is theoretical.
fn msvc_ref_qualifier(mangled_name: &str) -> CppRefQualifier {
    let rest = match mangled_name.strip_prefix('?') {
        Some(rest) => rest,
        None => return CppRefQualifier::None,
    };
    let rest = match rest.split_once("@@") {
        // See the limitation documented above.
        Some((qualified_name, _)) if qualified_name.contains('?') => return CppRefQualifier::None,
        Some((_, rest)) => rest.as_bytes(),
        None => return CppRefQualifier::None,
    };
    // The function class is a single letter in the range `A`..=`X`; the four
    // pairs which denote static or global functions can't be ref-qualified and
    // aren't followed by qualifiers at all.
    let mut rest = match rest.split_first() {
        Some((b'C' | b'D' | b'K' | b'L' | b'S' | b'T', _)) => return CppRefQualifier::None,
        Some((b'A'..=b'X', rest)) => rest,
        _ => return CppRefQualifier::None,
    };
    // Then any number of pointer/reference extended qualifiers: `E` for
    // `__ptr64`, `I` for `__restrict`, `F` for `__unaligned`.
    while let Some((b'E' | b'F' | b'I', remainder)) = rest.split_first() {
        rest = remainder;
    }
    match rest.first() {
        Some(b'G') => CppRefQualifier::LValue,
        Some(b'H') => CppRefQualifier::RValue,
        _ => CppRefQualifier::None,
    }
}

#[cfg(test)]
mod tests {
    use super::{ref_qualifier_from_mangled_name, CppRefQualifier};

    /// Every mangled name below was produced by clang for the corresponding
    /// C++ declaration, so that these tests pin real ABI output rather than
    /// our idea of it.
    fn check(mangled_name: &str, expected: CppRefQualifier) {
        assert_eq!(
            ref_qualifier_from_mangled_name(mangled_name),
            expected,
            "{mangled_name}"
        );
        // bindgen always hands us the name with a \u{1} prefix.
        assert_eq!(
            ref_qualifier_from_mangled_name(&format!("\u{1}{mangled_name}")),
            expected,
            "\\u{{1}}{mangled_name}"
        );
    }

    #[test]
    fn test_itanium() {
        // void A::foo() &
        check("_ZNR1A3fooEv", CppRefQualifier::LValue);
        // void A::bar() &&
        check("_ZNO1A3barEv", CppRefQualifier::RValue);
        // void A::baz() const &
        check("_ZNKR1A3bazEv", CppRefQualifier::LValue);
        // void A::qux() const &&
        check("_ZNKO1A3quxEv", CppRefQualifier::RValue);
        // void A::plain()
        check("_ZN1A5plainEv", CppRefQualifier::None);
        // void A::plain_const() const
        check("_ZNK1A11plain_constEv", CppRefQualifier::None);
        // static void A::stat()
        check("_ZN1A4statEv", CppRefQualifier::None);
        // void ns::B::m() &&
        check("_ZNO2ns1B1mEv", CppRefQualifier::RValue);
        // void D::args(C<int>*) &
        check("_ZNR1D4argsEP1CIiE", CppRefQualifier::LValue);
        // void C<int>::tm() &
        check("_ZNR1CIiE2tmEv", CppRefQualifier::LValue);
        // Mach-O spelling of void A::foo() &
        check("__ZNR1A3fooEv", CppRefQualifier::LValue);
        // A free function, which can never be ref-qualified.
        check("_Z4freev", CppRefQualifier::None);
        // A free function in a namespace: `2` starts a <source-name>.
        check("_ZN2ns4freeEv", CppRefQualifier::None);
    }

    #[test]
    fn test_msvc() {
        // void A::foo() &, x86-64 then x86
        check("?foo@A@@QEGAAXXZ", CppRefQualifier::LValue);
        check("?foo@A@@QGAEXXZ", CppRefQualifier::LValue);
        // void A::bar() &&
        check("?bar@A@@QEHAAXXZ", CppRefQualifier::RValue);
        check("?bar@A@@QHAEXXZ", CppRefQualifier::RValue);
        // void A::baz() const &
        check("?baz@A@@QEGBAXXZ", CppRefQualifier::LValue);
        check("?cfoo@A@@QGBEXXZ", CppRefQualifier::LValue);
        // void A::qux() const &&
        check("?qux@A@@QEHBAXXZ", CppRefQualifier::RValue);
        // void A::plain()
        check("?plain@A@@QEAAXXZ", CppRefQualifier::None);
        check("?plain@A@@QAEXXZ", CppRefQualifier::None);
        // void A::plain_const() const
        check("?plain_const@A@@QEBAXXZ", CppRefQualifier::None);
        // static void A::stat()
        check("?stat@A@@SAXXZ", CppRefQualifier::None);
        // private void A::reallypriv() &
        check("?reallypriv@A@@AEGAAXXZ", CppRefQualifier::LValue);
        check("?reallypriv@A@@AGAEXXZ", CppRefQualifier::LValue);
        // virtual void A::virt() &
        check("?virt@A@@UEGAAXXZ", CppRefQualifier::LValue);
        // void ns::B::m() &&
        check("?m@B@ns@@QEHAAXXZ", CppRefQualifier::RValue);
        // void D::args(C<int>*) & - a template in the *arguments* is fine,
        // because it comes after the first "@@".
        check("?args@D@@QEGAAXPEAU?$C@H@@@Z", CppRefQualifier::LValue);
        // void C<int>::tm() & - a template in the *name* is the documented
        // limitation; we decline to guess.
        check("?tm@?$C@H@@QEGAAXXZ", CppRefQualifier::None);
    }

    #[test]
    fn test_unrecognised_names_are_unqualified() {
        check("", CppRefQualifier::None);
        check("plain_c_function", CppRefQualifier::None);
        check("_Z", CppRefQualifier::None);
        check("_ZN", CppRefQualifier::None);
        check("?", CppRefQualifier::None);
        check("?foo@A@@", CppRefQualifier::None);
        check("?foo@A@@Q", CppRefQualifier::None);
    }
}
