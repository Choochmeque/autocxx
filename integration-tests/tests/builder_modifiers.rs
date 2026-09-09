// Copyright 2021 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use autocxx_engine::Builder;

use autocxx_integration_tests::{BuilderModifier, BuilderModifierFns, TestBuilderContext};

/// A C++ standard for both halves of the build.
///
/// Not `make_clang_arg_adder`: that passes its flags to the C++ compiler
/// verbatim, and `-std=c++17` is not how cl.exe spells it - it ignored the
/// gcc spelling with warning D9002, quietly compiling these tests at its
/// default standard. bindgen is always clang and takes the flag as written;
/// the C++ compiler is told through `cc`'s `std`, which picks the spelling
/// for the tool family - and, being set after `configure_builder`'s `c++14`,
/// replaces it.
pub(crate) fn make_cpp17_adder() -> Option<BuilderModifier> {
    Some(Box::new(StdAdder("c++17")))
}

/// See [`make_cpp17_adder`].
pub(crate) fn make_cpp20_adder() -> Option<BuilderModifier> {
    Some(Box::new(StdAdder("c++20")))
}

struct StdAdder(&'static str);

impl BuilderModifierFns for StdAdder {
    fn modify_autocxx_builder<'a>(
        &self,
        builder: Builder<'a, TestBuilderContext>,
    ) -> Builder<'a, TestBuilderContext> {
        builder.extra_clang_args(&[&format!("-std={}", self.0)])
    }

    fn modify_cc_builder<'a>(&self, builder: &'a mut cc::Build) -> &'a mut cc::Build {
        builder.std(self.0)
    }
}

/// `char` is unsigned, told to each tool in its own spelling: clang and gcc
/// take `-funsigned-char`; cl.exe calls it `/J` and ignored the gcc spelling
/// with warning D9002, so on MSVC the test formerly ran with `char` signed -
/// which is that test's whole subject.
pub(crate) fn make_unsigned_char_adder() -> Option<BuilderModifier> {
    Some(Box::new(UnsignedCharAdder))
}

struct UnsignedCharAdder;

impl BuilderModifierFns for UnsignedCharAdder {
    fn modify_autocxx_builder<'a>(
        &self,
        builder: Builder<'a, TestBuilderContext>,
    ) -> Builder<'a, TestBuilderContext> {
        builder.extra_clang_args(&["-funsigned-char"])
    }

    fn modify_cc_builder<'a>(&self, builder: &'a mut cc::Build) -> &'a mut cc::Build {
        let msvc = builder
            .try_get_compiler()
            .map(|c| c.is_like_msvc())
            .unwrap_or(false);
        builder.flag(if msvc { "/J" } else { "-funsigned-char" })
    }
}

/// Both modifiers, applied in order. For a test which needs, say, a C++
/// standard *and* a warning scoped off.
///
/// `Builder::extra_clang_args` assigns rather than appends, so of two modifiers
/// which both give clang arguments only the second's survive. No pair used here
/// does; a pair that did would have to combine the lists itself.
pub(crate) fn combine_modifiers(
    a: Option<BuilderModifier>,
    b: Option<BuilderModifier>,
) -> Option<BuilderModifier> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Box::new(CombinedModifier(a, b))),
        (one, None) | (None, one) => one,
    }
}

struct CombinedModifier(BuilderModifier, BuilderModifier);

impl BuilderModifierFns for CombinedModifier {
    fn modify_autocxx_builder<'a>(
        &self,
        builder: Builder<'a, TestBuilderContext>,
    ) -> Builder<'a, TestBuilderContext> {
        self.1
            .modify_autocxx_builder(self.0.modify_autocxx_builder(builder))
    }

    fn modify_cc_builder<'a>(&self, builder: &'a mut cc::Build) -> &'a mut cc::Build {
        self.1.modify_cc_builder(self.0.modify_cc_builder(builder))
    }
}

struct ClangArgAdder(Vec<String>);

pub(crate) fn make_clang_arg_adder(args: &[&str]) -> Option<BuilderModifier> {
    Some(Box::new(ClangArgAdder(
        args.iter().map(|a| a.to_string()).collect(),
    )))
}

impl BuilderModifierFns for ClangArgAdder {
    fn modify_autocxx_builder<'a>(
        &self,
        builder: Builder<'a, TestBuilderContext>,
    ) -> Builder<'a, TestBuilderContext> {
        let refs: Vec<_> = self.0.iter().map(|s| s.as_str()).collect();
        builder.extra_clang_args(&refs)
    }

    fn modify_cc_builder<'a>(&self, mut builder: &'a mut cc::Build) -> &'a mut cc::Build {
        for f in &self.0 {
            builder = builder.flag(f);
        }
        builder
    }
}

/// Clang arguments for bindgen alone, leaving the C++ compiler as it was.
///
/// [`make_clang_arg_adder`] hands its flags to both, which is what a flag the
/// two have to agree on needs - a C++ standard, a `-D`. A `--target` is not
/// one of those: a test about a type only some targets have needs bindgen to
/// parse as one of them, and the host's C++ compiler cannot be asked to become
/// it. Such a test skips the build; see `make_checks_without_building`.
pub(crate) fn make_bindgen_only_clang_arg_adder(args: &[&str]) -> Option<BuilderModifier> {
    Some(Box::new(BindgenOnlyClangArgAdder(
        args.iter().map(|a| a.to_string()).collect(),
    )))
}

struct BindgenOnlyClangArgAdder(Vec<String>);

impl BuilderModifierFns for BindgenOnlyClangArgAdder {
    fn modify_autocxx_builder<'a>(
        &self,
        builder: Builder<'a, TestBuilderContext>,
    ) -> Builder<'a, TestBuilderContext> {
        let refs: Vec<_> = self.0.iter().map(|s| s.as_str()).collect();
        builder.extra_clang_args(&refs)
    }
}

pub(crate) struct SetSuppressSystemHeaders;

impl BuilderModifierFns for SetSuppressSystemHeaders {
    fn modify_autocxx_builder<'a>(
        &self,
        builder: Builder<'a, TestBuilderContext>,
    ) -> Builder<'a, TestBuilderContext> {
        builder.suppress_system_headers(true)
    }
}

/// Generate a C++ wrapper function for every call, as the
/// `AUTOCXX_FORCE_WRAPPER_GENERATION` CI job does for the whole suite. A test
/// which is *about* what a wrapper contains wants this whatever the
/// environment says.
pub(crate) struct ForceWrapperGeneration;

impl BuilderModifierFns for ForceWrapperGeneration {
    fn modify_autocxx_builder<'a>(
        &self,
        builder: Builder<'a, TestBuilderContext>,
    ) -> Builder<'a, TestBuilderContext> {
        builder.force_wrapper_generation(true)
    }
}

pub(crate) struct EnableAutodiscover;

impl BuilderModifierFns for EnableAutodiscover {
    fn modify_autocxx_builder<'a>(
        &self,
        builder: Builder<'a, TestBuilderContext>,
    ) -> Builder<'a, TestBuilderContext> {
        builder.auto_allowlist(true)
    }
}
