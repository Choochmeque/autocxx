// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Tests specific to reference wrappers.

use crate::code_checkers::{make_checks, make_rust_code_finder};
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

/// As [`run_cpprefs_test`], but also examines the code that was generated -
/// for a test whose subject is what the policy did to a signature, which
/// running the result cannot always tell apart from what it would have done
/// anyway.
fn run_cpprefs_test_with_checks(
    header_code: &str,
    rust_code: TokenStream,
    generate: &[&str],
    generate_pods: &[&str],
    checks: autocxx_integration_tests::CodeChecker,
) {
    if !arbitrary_self_types_supported() {
        // "unsafe_references_wrapped" requires arbitrary_self_types, which requires nightly.
        return;
    }
    do_run_test(
        "",
        header_code,
        rust_code,
        directives_from_lists(generate, generate_pods, None),
        None,
        Some(checks),
        None,
        "unsafe_references_wrapped",
        Some(quote! {
            #![feature(arbitrary_self_types_pointers)]
        }),
    )
    .unwrap()
}

/// As [`run_cpprefs_test`], but for tests which need their own directives
/// (`subclass!`, say) and their own extra Rust items alongside the generated
/// bindings.
fn run_cpprefs_test_ex(
    cxx_code: &str,
    header_code: &str,
    rust_code: TokenStream,
    directives: TokenStream,
    extra_rust: Option<TokenStream>,
) {
    if !arbitrary_self_types_supported() {
        // "unsafe_references_wrapped" requires arbitrary_self_types, which requires nightly.
        return;
    }
    do_run_test(
        cxx_code,
        header_code,
        rust_code,
        directives,
        None,
        None,
        extra_rust,
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

/// As [`test_return_reference_cpprefs`], for a class which says what `&`
/// means for its own objects. What it says need have nothing to do with where
/// the object is, so the wrapper turning the returned reference into the
/// pointer the bridge carries must not ask the class.
#[test]
fn test_return_reference_to_type_overloading_address_of_cpprefs() {
    let cxx = indoc! {"
        Bob3 DECOY { 99, 99 };
        const Bob3* Bob3::operator&() const { return ::std::addressof(DECOY); }
        Bob3* Bob3::operator&() { return ::std::addressof(DECOY); }
        const Bob3& give_bob3(const Bob3& input_bob) {
            return input_bob;
        }
    "};
    let hdr = indoc! {"
        #include <cstdint>
        #include <memory>
        struct Bob3 {
            uint32_t a;
            uint32_t b;
            const Bob3* operator&() const;
            Bob3* operator&();
        };
        extern Bob3 DECOY;
        const Bob3& give_bob3(const Bob3& input_bob);
    "};
    let rs = quote! {
        let b = CppPin::new(ffi::Bob3 { a: 3, b: 4 });
        let bob = ffi::give_bob3(b.as_cpp_ref());
        let val = unsafe { bob.as_ref() };
        assert_eq!(val.b, 4);
    };
    run_cpprefs_test(cxx, hdr, rs, &["give_bob3"], &["Bob3"]);
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

/// A value parameter, out of the owning wrappers this mode leaves you holding.
/// Both of them own their object outright, so handing one over consumes it:
/// C++ is given the object itself rather than something copied out of a Rust
/// owner which lives on, the same bargain as consuming a `UniquePtr`. (Whether
/// the hand-over is then a move or a copy is up to what the type permits;
/// `fx_Load` here declares a copy constructor and so has no implicit move
/// constructor to offer.) Keeping the object instead would mean building a
/// copy from a Rust `&T` to contents C++ may hold aliasing references to,
/// which is what these wrappers exist to refuse; that copy stays spelled
/// `as_copy(unsafe { pin.as_ref() })`, with the promise where the caller can
/// see it.
#[test]
fn test_value_parameter_from_wrappers_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        #include <string>
        struct fx_Load {
            fx_Load() : a(42) {}
            fx_Load(const fx_Load& other) : a(other.a) {}
            uint32_t a;
            std::string so_we_are_non_trivial;
        };
        inline uint32_t fx_weigh(fx_Load l) { return l.a; }
    "};
    let rs = quote! {
        let load = ffi::fx_Load::new().within_cpp_pin();
        assert_eq!(ffi::fx_weigh(load), 42);
        let load = autocxx::CppUniquePtrPin::new(ffi::fx_Load::new().within_unique_ptr());
        assert_eq!(ffi::fx_weigh(load), 42);
    };
    run_cpprefs_test("", hdr, rs, &["fx_Load", "fx_weigh"], &[]);
}

/// `rust::Str&` is a mutable C++ reference like any other, and in this mode it
/// becomes a wrapper like any other - around the `&str` which is how cxx
/// spells a `rust::Str`. This is the one mode that keeps the shape: plain mode
/// refuses it (`test_pass_rust_str_by_mut_ref_refused`) because there the
/// parameter is a `Pin<&mut &str>`, a mutable reference to the very fat
/// pointer C++ is about to overwrite. Here Rust holds no reference to it at
/// all - a `CppMutRef` is never dereferenced except through an unsafe call the
/// caller vouches for - so what C++ writes there stays C++'s business.
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

/// The plainest `subclass!` there is: a pure virtual method with no
/// parameters at all, and a superclass whose only constructor takes none
/// either. Nothing here is a reference the mode has to wrap, which is the
/// point - `subclass!` had never generated buildable code in this mode at
/// all, whether or not a reference was mentioned anywhere.
#[test]
fn test_subclass_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        class fx_Observer {
        public:
            fx_Observer() {}
            virtual uint32_t fx_value() const = 0;
            virtual ~fx_Observer() {}
        };
        inline uint32_t fx_ask(const fx_Observer& o) { return o.fx_value(); }
    "};
    let rs = quote! {
        let obs = MyObserver::new_rust_owned(MyObserver { cpp_peer: Default::default() });
        let obs = obs.borrow();
        let sup: &ffi::fx_Observer = obs.as_ref();
        assert_eq!(ffi::fx_ask(autocxx::CppRef::from_ptr(sup)), 42);
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_ask")
            subclass!("fx_Observer",MyObserver)
        },
        Some(quote! {
            use autocxx::subclass::CppSubclass;
            use ffi::fx_Observer_methods;
            #[autocxx::subclass::subclass]
            pub struct MyObserver {}
            impl fx_Observer_methods for MyObserver {
                fn fx_value(&self) -> u32 { 42 }
            }
        }),
    );
}

/// A virtual method taking a const reference. C++ calls the override with a
/// `const fx_Datum&` and the mode says Rust sees a `CppRef`, so the generated
/// override has to say both things at once - which is the C++-into-Rust
/// direction the reference wrappers had never had to describe.
#[test]
fn test_subclass_const_ref_param_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Datum { uint32_t a; };
        class fx_Taker {
        public:
            fx_Taker() {}
            virtual void fx_take(const fx_Datum& d) = 0;
            virtual ~fx_Taker() {}
        };
        inline void fx_feed(fx_Taker& t, const fx_Datum& d) { t.fx_take(d); }
    "};
    let rs = quote! {
        let taker = MyTaker::new_rust_owned(MyTaker { seen: 0, cpp_peer: Default::default() });
        let d = CppPin::new(ffi::fx_Datum { a: 42 });
        // Take the C++ reference to the peer and let go of the Rust borrow
        // before calling in: C++ is about to call straight back out again.
        let sup: *mut ffi::fx_Taker = unsafe { taker.borrow_mut().pin_mut().get_unchecked_mut() };
        ffi::fx_feed(autocxx::CppMutRef::from_ptr(sup), d.as_cpp_ref());
        assert_eq!(taker.borrow().seen, 42);
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_feed")
            generate_pod!("fx_Datum")
            subclass!("fx_Taker",MyTaker)
        },
        Some(quote! {
            use autocxx::subclass::CppSubclass;
            use ffi::fx_Taker_methods;
            #[autocxx::subclass::subclass]
            pub struct MyTaker {
                seen: u32,
            }
            impl fx_Taker_methods for MyTaker {
                fn fx_take(&mut self, d: autocxx::CppRef<ffi::fx_Datum>) {
                    self.seen = unsafe { d.as_ref() }.a;
                }
            }
        }),
    );
}

/// The mutable twin of [`test_subclass_const_ref_param_cpprefs`]: the override
/// is handed a C++ reference it may write through, and writes through it.
#[test]
fn test_subclass_mut_ref_param_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Slot { uint32_t a; };
        class fx_Filler {
        public:
            fx_Filler() {}
            virtual void fx_fill_slot(fx_Slot& s) = 0;
            virtual ~fx_Filler() {}
        };
        inline void fx_run(fx_Filler& f, fx_Slot& s) { f.fx_fill_slot(s); }
    "};
    let rs = quote! {
        let filler = MyFiller::new_rust_owned(MyFiller { cpp_peer: Default::default() });
        let mut s = CppPin::new(ffi::fx_Slot { a: 0 });
        let sup: *mut ffi::fx_Filler = unsafe { filler.borrow_mut().pin_mut().get_unchecked_mut() };
        ffi::fx_run(autocxx::CppMutRef::from_ptr(sup), s.as_cpp_mut_ref());
        assert_eq!(unsafe { s.as_ref() }.a, 42);
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_run")
            generate_pod!("fx_Slot")
            subclass!("fx_Filler",MyFiller)
        },
        Some(quote! {
            use autocxx::subclass::CppSubclass;
            use ffi::fx_Filler_methods;
            #[autocxx::subclass::subclass]
            pub struct MyFiller {}
            impl fx_Filler_methods for MyFiller {
                fn fx_fill_slot(&mut self, mut s: autocxx::CppMutRef<ffi::fx_Slot>) {
                    unsafe { s.as_mut() }.a = 42;
                }
            }
        }),
    );
}

/// A superclass whose constructor takes a const reference. The subclass
/// constructor autocxx synthesizes has to hand that parameter on to the base
/// class initializer, and in this mode what it was handed is a pointer.
#[test]
fn test_subclass_constructor_ref_param_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Seed { uint32_t a; };
        class fx_Grower {
        public:
            fx_Grower(const fx_Seed& s) : v(s.a) {}
            virtual uint32_t fx_grown() const = 0;
            uint32_t fx_seeded() const { return v; }
            virtual ~fx_Grower() {}
        private:
            uint32_t v;
        };
        inline uint32_t fx_seed_of(const fx_Grower& g) { return g.fx_seeded(); }
    "};
    let rs = quote! {
        let seed = CppPin::new(ffi::fx_Seed { a: 42 });
        let grower = MyGrower::new_rust_owned(MyGrower {
            seed: seed.as_cpp_ref(),
            cpp_peer: Default::default(),
        });
        let grower = grower.borrow();
        let sup: &ffi::fx_Grower = grower.as_ref();
        assert_eq!(ffi::fx_seed_of(autocxx::CppRef::from_ptr(sup)), 42);
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_seed_of")
            generate_pod!("fx_Seed")
            subclass!("fx_Grower",MyGrower)
        },
        Some(quote! {
            use autocxx::subclass::{CppPeerConstructor, CppSubclass, CppSubclassRustPeerHolder};
            use ffi::fx_Grower_methods;
            #[autocxx::subclass::subclass]
            pub struct MyGrower {
                seed: autocxx::CppRef<ffi::fx_Seed>,
            }
            impl fx_Grower_methods for MyGrower {
                fn fx_grown(&self) -> u32 { 1 }
            }
            // The superclass constructor takes an argument, so autocxx can't
            // synthesize this for us.
            impl CppPeerConstructor<ffi::MyGrowerCpp> for MyGrower {
                fn make_peer(
                    &mut self,
                    peer_holder: CppSubclassRustPeerHolder<Self>,
                ) -> cxx::UniquePtr<ffi::MyGrowerCpp> {
                    ffi::MyGrowerCpp::new(peer_holder, self.seed)
                }
            }
        }),
    );
}

/// A virtual method which is not pure, so the peer class gets a `_super`
/// helper the Rust override can call - the one place where the generated C++
/// calls C++ and has to spell each converted parameter a third way.
#[test]
fn test_subclass_super_call_ref_param_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Note { uint32_t a; };
        class fx_Ledger {
        public:
            fx_Ledger() : total(0) {}
            virtual void fx_post(const fx_Note& n) { total += n.a; }
            uint32_t fx_total() const { return total; }
            virtual ~fx_Ledger() {}
        private:
            uint32_t total;
        };
        inline void fx_post_to(fx_Ledger& l, const fx_Note& n) { l.fx_post(n); }
    "};
    let rs = quote! {
        let ledger = MyLedger::new_rust_owned(MyLedger { cpp_peer: Default::default() });
        let n = CppPin::new(ffi::fx_Note { a: 21 });
        let sup: *mut ffi::fx_Ledger = unsafe { ledger.borrow_mut().pin_mut().get_unchecked_mut() };
        ffi::fx_post_to(autocxx::CppMutRef::from_ptr(sup), n.as_cpp_ref());
        assert_eq!(autocxx::CppRef::from_ptr(sup as *const ffi::fx_Ledger).fx_total(), 42);
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_post_to")
            generate_pod!("fx_Note")
            subclass!("fx_Ledger",MyLedger)
        },
        Some(quote! {
            use autocxx::subclass::CppSubclass;
            use ffi::fx_Ledger_methods;
            #[autocxx::subclass::subclass]
            pub struct MyLedger {}
            impl fx_Ledger_methods for MyLedger {
                fn fx_post(&mut self, n: autocxx::CppRef<ffi::fx_Note>) {
                    use ffi::fx_Ledger_supers;
                    // Post it twice, so that the assertion can only pass if
                    // the superclass really saw what we were given.
                    self.fx_post_super(n);
                    self.fx_post_super(n);
                }
            }
        }),
    );
}

/// A virtual method which *returns* a const reference. The mirror of
/// [`test_subclass_const_ref_param_cpprefs`]: this time the Rust override is
/// the one which has to produce a reference, and the mode says it speaks
/// `CppRef` rather than a raw pointer.
///
/// The reference the override hands back must outlive the call, exactly as a
/// hand-written C++ override's would - here it refers to a `CppPin` which
/// outlives the whole test.
#[test]
fn test_subclass_const_ref_return_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Reading { uint32_t a; };
        class fx_Gauge {
        public:
            fx_Gauge() {}
            virtual const fx_Reading& fx_latest() const = 0;
            virtual ~fx_Gauge() {}
        };
        inline uint32_t fx_read(const fx_Gauge& g) { return g.fx_latest().a; }
    "};
    let rs = quote! {
        let reading = CppPin::new(ffi::fx_Reading { a: 42 });
        let gauge = MyGauge::new_rust_owned(MyGauge {
            reading: reading.as_cpp_ref(),
            cpp_peer: Default::default(),
        });
        let gauge = gauge.borrow();
        let sup: &ffi::fx_Gauge = gauge.as_ref();
        assert_eq!(ffi::fx_read(autocxx::CppRef::from_ptr(sup)), 42);
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_read")
            generate_pod!("fx_Reading")
            subclass!("fx_Gauge",MyGauge)
        },
        Some(quote! {
            use autocxx::subclass::CppSubclass;
            use ffi::fx_Gauge_methods;
            #[autocxx::subclass::subclass]
            pub struct MyGauge {
                reading: autocxx::CppRef<ffi::fx_Reading>,
            }
            impl fx_Gauge_methods for MyGauge {
                fn fx_latest(&self) -> autocxx::CppRef<ffi::fx_Reading> {
                    self.reading
                }
            }
        }),
    );
}

/// The mutable twin of [`test_subclass_const_ref_return_cpprefs`]: the
/// override hands back a reference C++ then writes through.
#[test]
fn test_subclass_mut_ref_return_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Dial { uint32_t a; };
        class fx_Knob {
        public:
            fx_Knob() {}
            virtual fx_Dial& fx_dial() = 0;
            virtual ~fx_Knob() {}
        };
        inline void fx_turn(fx_Knob& k) { k.fx_dial().a = 42; }
    "};
    let rs = quote! {
        let mut dial = CppPin::new(ffi::fx_Dial { a: 0 });
        let knob = MyKnob::new_rust_owned(MyKnob {
            dial: dial.as_cpp_mut_ref(),
            cpp_peer: Default::default(),
        });
        let sup: *mut ffi::fx_Knob = unsafe { knob.borrow_mut().pin_mut().get_unchecked_mut() };
        ffi::fx_turn(autocxx::CppMutRef::from_ptr(sup));
        assert_eq!(unsafe { dial.as_ref() }.a, 42);
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_turn")
            generate_pod!("fx_Dial")
            subclass!("fx_Knob",MyKnob)
        },
        Some(quote! {
            use autocxx::subclass::CppSubclass;
            use ffi::fx_Knob_methods;
            #[autocxx::subclass::subclass]
            pub struct MyKnob {
                dial: autocxx::CppMutRef<ffi::fx_Dial>,
            }
            impl fx_Knob_methods for MyKnob {
                fn fx_dial(&mut self) -> autocxx::CppMutRef<ffi::fx_Dial> {
                    self.dial
                }
            }
        }),
    );
}

/// A non-pure virtual method returning a const reference, so the peer gets a
/// `_super` helper the Rust override can call. The `_supers` trait item
/// returns the same `CppRef` the `_methods` one does, so the override passes
/// the superclass's answer straight back out with no conversion of its own -
/// which is the point: the two trait items describe one C++ signature and
/// have to agree about it.
#[test]
fn test_subclass_super_call_ref_return_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Tally { uint32_t a; };
        class fx_Counter {
        public:
            fx_Counter() : held{7} {}
            virtual const fx_Tally& fx_held() const { return held; }
            virtual ~fx_Counter() {}
        private:
            fx_Tally held;
        };
        inline uint32_t fx_held_of(const fx_Counter& c) { return c.fx_held().a; }
    "};
    let rs = quote! {
        let counter = MyCounter::new_rust_owned(MyCounter { cpp_peer: Default::default() });
        let counter = counter.borrow();
        let sup: &ffi::fx_Counter = counter.as_ref();
        assert_eq!(ffi::fx_held_of(autocxx::CppRef::from_ptr(sup)), 7);
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_held_of")
            generate_pod!("fx_Tally")
            subclass!("fx_Counter",MyCounter)
        },
        Some(quote! {
            use autocxx::subclass::CppSubclass;
            use ffi::fx_Counter_methods;
            #[autocxx::subclass::subclass]
            pub struct MyCounter {}
            impl fx_Counter_methods for MyCounter {
                fn fx_held(&self) -> autocxx::CppRef<ffi::fx_Tally> {
                    use ffi::fx_Counter_supers;
                    self.fx_held_super()
                }
            }
        }),
    );
}

/// The mutable twin of [`test_subclass_super_call_ref_return_cpprefs`], which
/// is a different path and not only a different type: the peer's `_super`
/// binding hands back a `CppMutLtRef`, whose `lifetime_cast` wants `&mut
/// self`, so the generated `_supers` impl has to be able to take a mutable
/// borrow of the value it just received. Nothing else in the suite puts a
/// mutable reference return and a `_super` helper together.
#[test]
fn test_subclass_super_call_mut_ref_return_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Meter { uint32_t a; };
        class fx_Panel {
        public:
            fx_Panel() : gauge{1} {}
            virtual fx_Meter& fx_gauge() { return gauge; }
            uint32_t fx_reading() const { return gauge.a; }
            virtual ~fx_Panel() {}
        private:
            fx_Meter gauge;
        };
        inline void fx_bump(fx_Panel& p) { p.fx_gauge().a += 41; }
    "};
    let rs = quote! {
        let panel = MyPanel::new_rust_owned(MyPanel { cpp_peer: Default::default() });
        let sup: *mut ffi::fx_Panel = unsafe { panel.borrow_mut().pin_mut().get_unchecked_mut() };
        ffi::fx_bump(autocxx::CppMutRef::from_ptr(sup));
        assert_eq!(
            autocxx::CppRef::from_ptr(sup as *const ffi::fx_Panel).fx_reading(),
            42
        );
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_bump")
            generate_pod!("fx_Meter")
            subclass!("fx_Panel",MyPanel)
        },
        Some(quote! {
            use autocxx::subclass::CppSubclass;
            use ffi::fx_Panel_methods;
            #[autocxx::subclass::subclass]
            pub struct MyPanel {}
            impl fx_Panel_methods for MyPanel {
                fn fx_gauge(&mut self) -> autocxx::CppMutRef<ffi::fx_Meter> {
                    use ffi::fx_Panel_supers;
                    self.fx_gauge_super()
                }
            }
        }),
    );
}

/// A virtual method returning an *rvalue* reference. C++'s `&&` has no Rust
/// spelling at all - the type converter makes the same pointer of it that it
/// makes of a `T&` - so this mode says about it what it says about any other
/// C++ reference an override hands back: a `CppMutRef`, not the raw pointer
/// the trait item used to ask for. The extra `&` is the superclass's promise
/// that C++ may move out of the referent, which is between the override and
/// the header it implements, exactly as it is between two C++ classes.
///
/// The peer's override repeats the `fx_Baton&&`, which is what makes it an
/// override at all; a `fx_Baton*` there overrides nothing and does not
/// compile. `fx_Baton`'s copy constructor is deleted, so C++ can only take
/// what it was given by moving out of it - and the moved-from object is
/// checked afterwards, so the move has to have happened through the override
/// rather than anywhere else.
#[test]
fn test_subclass_rvalue_ref_return_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Baton {
            uint32_t a;
            explicit fx_Baton(uint32_t a) : a(a) {}
            fx_Baton(fx_Baton&& other) : a(other.a) { other.a = 0; }
            fx_Baton(const fx_Baton&) = delete;
            uint32_t fx_get() const { return a; }
        };
        class fx_Relay {
        public:
            fx_Relay() {}
            virtual fx_Baton&& fx_pass() = 0;
            virtual ~fx_Relay() {}
        };
        inline uint32_t fx_run(fx_Relay& r) {
            fx_Baton taken(r.fx_pass());
            return taken.fx_get();
        }
    "};
    let rs = quote! {
        let mut baton = autocxx::CppUniquePtrPin::new(ffi::fx_Baton::new(42).within_unique_ptr());
        let relay = MyRelay::new_rust_owned(MyRelay {
            baton: baton.as_cpp_mut_ref(),
            cpp_peer: Default::default(),
        });
        // Take the pointer in a statement of its own, so the `RefCell` borrow
        // is over before C++ calls back into the override.
        let sup: *mut ffi::fx_Relay = unsafe { relay.borrow_mut().pin_mut().get_unchecked_mut() };
        assert_eq!(ffi::fx_run(autocxx::CppMutRef::from_ptr(sup)), 42);
        assert_eq!(baton.as_cpp_ref().fx_get(), 0);
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_run")
            generate!("fx_Baton")
            subclass!("fx_Relay",MyRelay)
        },
        Some(quote! {
            use autocxx::subclass::CppSubclass;
            use ffi::fx_Relay_methods;
            #[autocxx::subclass::subclass]
            pub struct MyRelay {
                baton: autocxx::CppMutRef<ffi::fx_Baton>,
            }
            impl fx_Relay_methods for MyRelay {
                fn fx_pass(&mut self) -> autocxx::CppMutRef<ffi::fx_Baton> {
                    self.baton
                }
            }
        }),
    );
}

/// The const twin of [`test_subclass_rvalue_ref_return_cpprefs`]. A
/// `const fx_Token&&` crosses the bridge as a `*const`, so the wrapper on the
/// Rust side is a `CppRef`, and the peer has to say `const` in both halves of
/// its signature or it overrides nothing.
#[test]
fn test_subclass_const_rvalue_ref_return_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        struct fx_Token { uint32_t a; };
        class fx_Vault {
        public:
            fx_Vault() {}
            virtual const fx_Token&& fx_yield() const = 0;
            virtual ~fx_Vault() {}
        };
        inline uint32_t fx_peek(const fx_Vault& v) { return v.fx_yield().a; }
    "};
    let rs = quote! {
        let token = CppPin::new(ffi::fx_Token { a: 42 });
        let vault = MyVault::new_rust_owned(MyVault {
            token: token.as_cpp_ref(),
            cpp_peer: Default::default(),
        });
        let vault = vault.borrow();
        let sup: &ffi::fx_Vault = vault.as_ref();
        assert_eq!(ffi::fx_peek(autocxx::CppRef::from_ptr(sup)), 42);
    };
    run_cpprefs_test_ex(
        "",
        hdr,
        rs,
        quote! {
            generate!("fx_peek")
            generate_pod!("fx_Token")
            subclass!("fx_Vault",MyVault)
        },
        Some(quote! {
            use autocxx::subclass::CppSubclass;
            use ffi::fx_Vault_methods;
            #[autocxx::subclass::subclass]
            pub struct MyVault {
                token: autocxx::CppRef<ffi::fx_Token>,
            }
            impl fx_Vault_methods for MyVault {
                fn fx_yield(&self) -> autocxx::CppRef<ffi::fx_Token> {
                    self.token
                }
            }
        }),
    );
}

/// The opaque holder autocxx lowers a `std::shared_ptr<const T>` to, in this
/// mode. Its payload accessor is a C++ reference like any other the mode deals
/// in, so it hands back a `CppRef` rather than the `*const` plain mode gives -
/// and the rest of the holder is unaffected, because ownership is the
/// `UniquePtr`'s business and has nothing to do with references.
///
/// `get` is `unsafe` in this mode alone, and that is the point of covering it
/// here. A `CppRef` is what a C++ `const T&` parameter takes under this policy,
/// and the generated C++ dereferences it with no further `unsafe` from the
/// caller - so a `CppRef` built from `std::shared_ptr::get`, which may be null
/// or (through the aliasing constructor) not owned by this holder at all,
/// has to be vouched for where it is made rather than where it is used.
///
/// Addresses the bug reported upstream as google/autocxx#799.
#[test]
fn test_shared_ptr_const_cpprefs() {
    let hdr = indoc! {"
        #include <memory>
        inline std::shared_ptr<const int> fx_hold() {
            return std::make_shared<const int>(3);
        }
        inline int fx_peek(std::shared_ptr<const int> p) { return *p; }
    "};
    let rs = quote! {
        let held = ffi::fx_hold();
        // Safe: `fx_hold` returns a `make_shared` result, which owns a live
        // payload, and `held` keeps it alive across the use below.
        let payload: autocxx::CppRef<autocxx::c_int> = unsafe { held.get() };
        assert_eq!(*unsafe { payload.as_ref() }, autocxx::c_int(3));
        assert_eq!(held.use_count(), 1);
        let second = held.clone();
        assert_eq!(held.use_count(), 2);
        assert_eq!(ffi::fx_peek(second), autocxx::c_int(3));
        assert_eq!(held.use_count(), 1);
    };
    run_cpprefs_test("", hdr, rs, &["fx_hold", "fx_peek"], &[]);
}

/// The `std::unique_ptr<const T>` holder in this mode, which takes the same
/// decision for the same reason: `get` hands back a `CppRef` and is `unsafe`,
/// because a `std::unique_ptr` may hold nothing and this mode's `CppRef` is
/// dereferenced by generated C++ with no further `unsafe` from the caller.
///
/// What is different is that the promise can be discharged here without any
/// outside knowledge: `payload_is_null` is a total answer to the only way this
/// pointer can be bad, where `std::shared_ptr::get` has the aliasing
/// constructor as well. The test therefore checks first and calls `get`
/// afterwards, which is the pattern the generated docs recommend.
///
/// Addresses part of the bug reported upstream as google/autocxx#799.
#[test]
fn test_unique_ptr_const_cpprefs() {
    let hdr = indoc! {"
        #include <memory>
        inline std::unique_ptr<const int> fx_own() {
            return std::unique_ptr<const int>(new int(3));
        }
        inline std::unique_ptr<const int> fx_own_nothing() {
            return std::unique_ptr<const int>();
        }
    "};
    let rs = quote! {
        let held = ffi::fx_own();
        assert!(!held.payload_is_null());
        // Safe: just established that the `unique_ptr` holds something, and
        // `held` owns it across the use below.
        let payload: autocxx::CppRef<autocxx::c_int> = unsafe { held.get() };
        assert_eq!(*unsafe { payload.as_ref() }, autocxx::c_int(3));

        let empty = ffi::fx_own_nothing();
        assert!(empty.payload_is_null());
    };
    run_cpprefs_test("", hdr, rs, &["fx_own", "fx_own_nothing"], &[]);
}

/// A `std::vector<T*>` holder under `unsafe_references_wrapped`.
///
/// The accessors are unchanged by the policy, which is the decision this test
/// records. A `CppRef` is what a C++ *reference* becomes in this mode, and
/// there is no reference here: a `std::vector<T*>` stores pointers, and a C++
/// pointer reaches Rust as a raw pointer under every policy autocxx has.
/// Wrapping one would manufacture exactly the promise a `CppRef` carries and
/// the vector does not make - non-null, and live for as long as you hold it -
/// which is the hole review found in the smart-pointer holder's `get` and
/// closed by making that method `unsafe`. There is nothing to close here.
///
/// The mode gives up nothing by leaving these alone, because its guarantee is
/// kept at the other end: `argument_conversion_details` gives a parameter of
/// `TypeKind::Pointer` `UnsafetyNeeded::Always` whatever the policy, so a
/// generated function which *takes* one of these elements is an `unsafe fn`
/// here as everywhere. `fx_horns` below is one, and the test insists on the
/// `unsafe fn` in the generated signature rather than settling for writing
/// `unsafe` at the call, which would compile either way.
///
/// Addresses the bug reported upstream as google/autocxx#330.
#[test]
fn test_vector_of_pointers_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        #include <vector>
        struct fx_Goat { uint32_t horns; };
        inline std::vector<fx_Goat*> fx_herd() {
            static fx_Goat only{3};
            return { &only, nullptr };
        }
        inline uint32_t fx_horns(fx_Goat* g) { return g->horns; }
    "};
    let rs = quote! {
        let v = ffi::fx_herd();
        assert_eq!(v.len(), 2);
        // The annotation is the assertion: a raw pointer, not a `CppMutRef`.
        let first: *mut ffi::fx_Goat = v.get(0).unwrap();
        assert!(v.get(1).unwrap().is_null());
        assert!(v.get(2).is_none());
        // Safe: `fx_herd`'s first element points at a `static`, so it is
        // non-null and outlives this call.
        assert_eq!(unsafe { ffi::fx_horns(first) }, 3);
    };
    run_cpprefs_test_with_checks(
        hdr,
        rs,
        &["fx_herd", "fx_horns"],
        &["fx_Goat"],
        make_checks(vec![make_rust_code_finder(vec![
            // The element as a raw pointer coming out...
            quote! {
                pub fn get (& self , pos : usize) -> Option < * mut output :: fx_Goat >
            },
            // ...and `unsafe` demanded of anything taking one back in. The
            // parameter is part of the pattern so that this cannot be
            // satisfied by one of bindgen's own declarations of the same
            // function under a suffixed name.
            quote! {
                pub unsafe fn fx_horns (g : * mut fx_Goat) -> u32
            },
        ])]),
    );
}

/// A copy constructor in this mode. Its source is a `const T&`, which the mode
/// otherwise turns into a `CppRef`, but `moveit`'s `CopyNew` copies from a
/// `&Self` and that is not negotiable - so the source stays a Rust reference
/// here as it is under every other policy. Before, the mode's conversion made
/// the parameter unrecognizable as a reference and the copy constructor became
/// an ordinary constructor: `CopyNew` went unimplemented and `.clone()` and
/// `moveit!` had nothing to call.
#[test]
fn test_copy_constructor_cpprefs() {
    let hdr = indoc! {"
        #include <cstdint>
        #include <string>
        class fx_Copyable {
        public:
            fx_Copyable(uint32_t a) : s(std::to_string(a)) {}
            fx_Copyable(const fx_Copyable& other) : s(other.s) {}
            uint32_t len() const { return static_cast<uint32_t>(s.length()); }
        private:
            std::string s;
        };
    "};
    let rs = quote! {
        let a = ffi::fx_Copyable::new(12345).within_unique_ptr();
        let a = autocxx::CppUniquePtrPin::new(a);
        moveit! { let b = autocxx::moveit::new::copy(unsafe { a.as_cpp_ref().as_ref() }); }
        let b = autocxx::CppRef::from_ptr(b.as_ref().get_ref() as *const ffi::fx_Copyable);
        assert_eq!(b.len(), 5);
    };
    run_cpprefs_test("", hdr, rs, &["fx_Copyable"], &[]);
}

/// The holder standing for a `const` reference to a C++ variable, under
/// `unsafe_references_wrapped`.
///
/// `get` hands back a `CppRef` here, as the smart-pointer holders' do, and is
/// `unsafe` for the same kind of reason: a `std::reference_wrapper` is never
/// empty, but a variable of static storage duration is not alive before its
/// initialization or after static destruction, and this holder's C++ type is
/// one a header can hand back referring to anything at all.
///
/// Addresses the bug reported upstream as google/autocxx#94.
#[test]
fn test_non_pod_constant_cpprefs() {
    let cxx = indoc! {"
        const fx_Held FX_HELD(3);
    "};
    let hdr = indoc! {"
        struct fx_Held {
            int v;
            explicit fx_Held(int v) : v(v) {}
            fx_Held(const fx_Held&) = delete;
            int peek() const { return v; }
        };
        extern const fx_Held FX_HELD;
    "};
    let rs = quote! {
        let held = ffi::FX_HELD();
        // Safe: `FX_HELD` is a constant, and this runs between its
        // initialization and static destruction.
        let held: autocxx::CppRef<ffi::fx_Held> = unsafe { held.as_ref().unwrap().get() };
        // A `CppRef` is a receiver in this mode, which is the whole point of
        // handing one back rather than a raw pointer.
        assert_eq!(held.peek(), autocxx::c_int(3));
    };
    run_cpprefs_test(cxx, hdr, rs, &["FX_HELD", "fx_Held"], &[]);
}
