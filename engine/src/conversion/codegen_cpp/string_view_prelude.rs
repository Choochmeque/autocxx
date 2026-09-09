// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use indoc::indoc;

/// What a wrapper naming `std::string_view` needs before it can be compiled.
///
/// `std::string_view` arrived in C++17, and autocxx's two halves are told the
/// C++ standard separately: the headers were parsed by libclang with whatever
/// `extra_clang_args` said, and this file is compiled by whatever the caller
/// set on the `cc::Build`. Nothing makes the two agree, so a build whose
/// clang args say C++17 and whose compiler is still at the C++14 default
/// reaches here with `std::string_view` naming nothing - and would report it
/// as an unknown identifier inside generated code the user did not write.
///
/// The standard is asked about before `<string_view>` is included rather than
/// after, so that the answer does not depend on what the header does when it
/// is unavailable - which is a `#pragma message` on MSVC, an empty header on
/// libstdc++ and libc++, and not promised by anything. `_MSVC_LANG` before
/// `__cplusplus` because cl.exe reports the latter as `199711L` whatever
/// `/std:` says unless `/Zc:__cplusplus` is passed, while the former is
/// always the truth.
pub(super) static STRING_VIEW_PRELUDE: &str = indoc! {"
    #if (defined(_MSVC_LANG) && _MSVC_LANG < 201703L) || \
    (!defined(_MSVC_LANG) && __cplusplus < 201703L)
    #error autocxx generated a std::string_view here, which needs C++17, but \
    this file is being compiled as something older. autocxx is told the C++ \
    standard twice and separately - once for parsing the headers and once for \
    compiling: pass it to the C++ compiler too, e.g. cc::Build::std(\"c++17\") \
    beside extra_clang_args([\"-std=c++17\"]).
    #endif
    #include <string_view>
"};
