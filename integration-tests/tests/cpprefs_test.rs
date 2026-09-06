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
        let mut sink = CppPin::new(ffi::fx_Sink { a: 0 });
        ffi::fx_fill(sink.as_cpp_mut_ref());
        assert_eq!(unsafe { sink.as_ref() }.a, 42);
    };
    run_cpprefs_test("", hdr, rs, &["fx_fill"], &["fx_Sink"]);
}

/// The same header with the reference written out, so that the two spellings
/// are pinned to agree in this mode as well as in the plain one. They agree on
/// `CppMutRef`: every C++ reference which crosses the bridge in this mode is a
/// wrapper, whether it is const or mutable and whichever way it is spelled.
/// A mutable one has to be, most of all - a `&mut T` would be a Rust mutable
/// reference to an object C++ is free to hold other references to, which is
/// the aliasing the mode exists to rule out.
#[test]
fn test_mutable_reference_parameter_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Sink2 { uint32_t a; };
        inline void fx_fill2(fx_Sink2& sink) { sink.a = 42; }
    "};
    let rs = quote! {
        let mut sink = CppPin::new(ffi::fx_Sink2 { a: 0 });
        ffi::fx_fill2(sink.as_cpp_mut_ref());
        assert_eq!(unsafe { sink.as_ref() }.a, 42);
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

/// The mutable twin of [`test_return_reference_cpprefs`]. A returned mutable
/// reference has always come back as a `CppMutLtRef`; now the parameter which
/// it borrows from is a wrapper too, so the whole signature is C++ references
/// and no Rust reference is in sight.
#[test]
fn test_return_mutable_reference_cpprefs() {
    let cxx = indoc! {"
        Bob2& give_bob2(Bob2& input_bob) {
            input_bob.a += 1;
            return input_bob;
        }
    "};
    let hdr = indoc! {"
        #include <cstdint>
        struct Bob2 {
            uint32_t a;
        };
        Bob2& give_bob2(Bob2& input_bob);
    "};
    let rs = quote! {
        let mut b = CppPin::new(ffi::Bob2 { a: 3 });
        let mut bob = ffi::give_bob2(b.as_cpp_mut_ref());
        let mut bob = bob.lifetime_cast();
        assert_eq!(unsafe { bob.as_mut() }.a, 4);
    };
    run_cpprefs_test(cxx, hdr, rs, &["give_bob2"], &["Bob2"]);
}

/// Two mutable references to the same object, alive at the same time and both
/// passed to one C++ function. This is ordinary C++ and instant undefined
/// behaviour in Rust, so it is the thing the mode is for: with the parameters
/// as `Pin<&mut T>` it could not be written at all, and writing it with two
/// `&mut` would be exactly the aliasing the wrappers exist to keep out of
/// Rust's model.
#[test]
fn test_aliasing_mutable_reference_parameters_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct Cell { uint32_t a; };
        inline void add_into(Cell& dest, Cell& src) { dest.a += src.a; }
    "};
    let rs = quote! {
        let mut cell = CppPin::new(ffi::Cell { a: 3 });
        let dest = cell.as_cpp_mut_ref();
        let src = cell.as_cpp_mut_ref();
        ffi::add_into(dest, src);
        assert_eq!(unsafe { cell.as_ref() }.a, 6);
    };
    run_cpprefs_test("", hdr, rs, &["add_into"], &["Cell"]);
}

/// A mutable reference to a type Rust only ever holds behind a pointer, where
/// the cases above are all types it holds by value. What the parameter becomes
/// is decided by the C++ reference and by nothing about the referent, so this
/// one is a `CppMutRef` as well - here around cxx's `CxxString`.
#[test]
fn test_nonpod_mutable_reference_parameter_cpprefs() {
    let hdr = indoc! {"
        #include <string>
        inline void append_x(std::string& s) { s += \"x\"; }
    "};
    let rs = quote! {
        let mut s = autocxx::CppUniquePtrPin::new(ffi::make_string("hello"));
        ffi::append_x(s.as_cpp_mut_ref());
        assert_eq!(unsafe { s.as_cpp_ref().as_ref() }.to_string_lossy(), "hellox");
    };
    run_cpprefs_test("", hdr, rs, &["append_x"], &[]);
}

/// A method taking a mutable reference: the receiver and the parameter are
/// both C++ references, and both arrive as wrappers. The receiver has always
/// been one; before, the parameter beside it was a `Pin<&mut T>`.
#[test]
fn test_method_with_mutable_reference_parameter_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct Arg { uint32_t a; };
        class Recipient {
            public:
                Recipient() : v(0) {}
                void take(Arg& a) { v = a.a; a.a = 7; }
                uint32_t get() const { return v; }
            private:
                uint32_t v;
        };
    "};
    let rs = quote! {
        let recipient = ffi::Recipient::new().within_unique_ptr();
        let mut recipient = autocxx::CppUniquePtrPin::new(recipient);
        let mut arg = CppPin::new(ffi::Arg { a: 42 });
        recipient.as_cpp_mut_ref().take(arg.as_cpp_mut_ref());
        assert_eq!(recipient.as_cpp_ref().get(), 42);
        assert_eq!(unsafe { arg.as_ref() }.a, 7);
    };
    run_cpprefs_test("", hdr, rs, &["Recipient"], &["Arg"]);
}

/// An rvalue reference is the one kind of C++ reference this mode does not
/// wrap, and need not: `impl RValueParam<T>` consumes the Rust owner, pins it
/// through the call, and presents an rvalue to C++ - after which no Rust
/// reference or usable Rust owner remains, whatever the callee chooses to do
/// with what it was given. There is no Rust alias left for a C++ reference to
/// race with, which is the hazard the mode's wrappers exist for. (This test
/// pins the binding and the API shape; the callee here deliberately does
/// nothing.)
#[test]
fn test_rvalue_reference_parameter_cpprefs() {
    let hdr = indoc! {"
        #include <string>
        struct Movable { std::string a; };
        inline void take_movable(Movable&&) {}
    "};
    let rs = quote! {
        let m = ffi::Movable::new().within_unique_ptr();
        ffi::take_movable(m);
    };
    run_cpprefs_test("", hdr, rs, &["Movable", "take_movable"], &[]);
}

/// `rust::Str&` is a mutable C++ reference like any other, and in this mode it
/// becomes a wrapper like any other - around the `&str` which is how cxx
/// spells a `rust::Str`. Plain mode has the same header in
/// `test_pass_rust_str_by_mut_ref`, where the parameter is `Pin<&mut &str>`:
/// there Rust holds a mutable reference to the very fat pointer C++ is about
/// to write to, and here it holds none. What C++ writes there is still
/// unchecked, so the lifetime half of the note in `type_converter.rs` stands.
#[test]
fn test_pass_rust_str_by_mut_ref_cpprefs() {
    let cxx = indoc! {"
        uint32_t measure_string(rust::Str& z) {
            return static_cast<uint32_t>(std::string(z).length());
        }
    "};
    let hdr = indoc! {"
        #include <cstdint>
        #include <cxx.h>
        uint32_t measure_string(rust::Str& z);
    "};
    let rs = quote! {
        let mut s = CppPin::new("hello");
        assert_eq!(ffi::measure_string(s.as_cpp_mut_ref()), 5);
    };
    run_cpprefs_test(cxx, hdr, rs, &["measure_string"], &[]);
}
