// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Tests specific to reference wrappers.

use autocxx_integration_tests::{directives_from_lists, do_run_test};
use indoc::indoc;
use proc_macro2::TokenStream;
use quote::quote;

const fn arbitrary_self_types_supported() -> bool {
    rustversion::cfg!(nightly)
}

/// A positive test, we expect to pass.
fn run_cpprefs_test(
    cxx_code: &str,
    header_code: &str,
    rust_code: TokenStream,
    generate: &[&str],
    generate_pods: &[&str],
) {
    if !arbitrary_self_types_supported() {
        // "unsafe_references_wrapped" requires arbitrary_self_types, which requires nightly.
        return;
    }
    do_run_test(
        cxx_code,
        header_code,
        rust_code,
        directives_from_lists(generate, generate_pods, None),
        None,
        None,
        None,
        "unsafe_references_wrapped",
        Some(quote! {
            #![feature(arbitrary_self_types_pointers)]
        }),
    )
    .unwrap()
}

#[test]
fn test_method_call_mut() {
    run_cpprefs_test(
        "",
        indoc! {"
        #include <string>
        #include <sstream>
        #include <cstdint>

        class Goat {
            public:
                Goat() : horns(0) {}
                void add_a_horn();
            private:
                uint32_t horns;
        };

        inline void Goat::add_a_horn() { horns++; }
    "},
        quote! {
            let goat = ffi::Goat::new().within_unique_ptr();
            let mut goat = autocxx::CppUniquePtrPin::new(goat);
            goat.as_cpp_mut_ref().add_a_horn();
        },
        &["Goat"],
        &[],
    )
}

#[test]
fn test_method_call_const() {
    run_cpprefs_test(
        "",
        indoc! {"
        #include <string>
        #include <sstream>
        #include <cstdint>

        class Goat {
            public:
                Goat() : horns(0) {}
                std::string describe() const;
            private:
                uint32_t horns;
        };

        inline std::string Goat::describe() const {
            std::ostringstream oss;
            std::string plural = horns == 1 ? \"\" : \"s\";
            oss << \"This goat has \" << horns << \" horn\" << plural << \".\";
            return oss.str();
        }
    "},
        quote! {
            let goat = ffi::Goat::new().within_unique_ptr();
            let goat = autocxx::CppUniquePtrPin::new(goat);
            goat.as_cpp_ref().describe();
        },
        &["Goat"],
        &[],
    )
}

/// A mutable reference parameter spelled with a typedef, which is how C++
/// libraries usually spell an output parameter. Whether the typedef is
/// understood to be a reference at all decides how the parameter crosses the
/// bridge, and this mode is where it decides the most: a reference is handed
/// over as a wrapper rather than as a Rust reference, so getting it wrong is
/// not a matter of taste. Plain mode covers the same header in
/// `test_typedef_to_mutable_reference_parameter`; this is the half of it that
/// only nightly can run. See google/autocxx#1363.
#[test]
fn test_typedef_to_mutable_reference_parameter_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Sink { uint32_t a; };
        typedef fx_Sink& fx_SinkRef;
        inline void fx_fill(fx_SinkRef sink) { sink.a = 42; }
    "};
    let rs = quote! {
        let mut sink = ffi::fx_Sink { a: 0 };
        ffi::fx_fill(std::pin::Pin::new(&mut sink));
        assert_eq!(sink.a, 42);
    };
    run_cpprefs_test("", hdr, rs, &["fx_fill"], &["fx_Sink"]);
}

/// The same header with the reference written out, so that the two spellings
/// are pinned to agree in this mode as well as in the plain one. They agree on
/// `Pin<&mut T>`: this mode wraps a *const* reference parameter into a
/// `CppRef` and a method's receiver into a `CppMutRef`, and leaves a mutable
/// reference parameter as a Rust reference - see the TODO in
/// `argument_conversion_details`. Whatever that comes to be, both spellings
/// have to arrive at it together, which is what this pair is here to catch.
#[test]
fn test_mutable_reference_parameter_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Sink2 { uint32_t a; };
        inline void fx_fill2(fx_Sink2& sink) { sink.a = 42; }
    "};
    let rs = quote! {
        let mut sink = ffi::fx_Sink2 { a: 0 };
        ffi::fx_fill2(std::pin::Pin::new(&mut sink));
        assert_eq!(sink.a, 42);
    };
    run_cpprefs_test("", hdr, rs, &["fx_fill2"], &["fx_Sink2"]);
}

#[test]
fn test_return_reference_cpprefs() {
    let cxx = indoc! {"
        const Bob& give_bob(const Bob& input_bob) {
            return input_bob;
        }
    "};
    let hdr = indoc! {"
        #include <cstdint>
        struct Bob {
            uint32_t a;
            uint32_t b;
        };
        const Bob& give_bob(const Bob& input_bob);
    "};
    let rs = quote! {
        let b = CppPin::new(ffi::Bob { a: 3, b: 4 });
        let b_ref = b.as_cpp_ref();
        let bob = ffi::give_bob(b_ref);
        let val = unsafe { bob.as_ref() };
        assert_eq!(val.b, 4);
    };
    run_cpprefs_test(cxx, hdr, rs, &["give_bob"], &["Bob"]);
}
