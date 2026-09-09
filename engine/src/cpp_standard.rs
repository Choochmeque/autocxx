// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Which C++ standard the headers are parsed at.
//!
//! autocxx parses at the standard in [`crate::AUTOCXX_CLANG_ARGS`] and puts it
//! first in the argument vector so a caller's `-std=` overrides it - the escape
//! for C++ which only parses at an older standard. Mostly nothing downstream
//! needs to know which one won. The exception is a fact which is only available
//! from C++17: `noexcept(expr)` is resolved in a function's canonical type
//! there, because that is where a C++17 exception specification lives, and
//! before C++17 the canonical type carries no specification at all. Reading the
//! canonical kind without knowing which of those is the case would take a
//! function declared `noexcept` for one which may throw.
//!
//! So this works out which `-std=` clang will obey, by clang's own rule that
//! the last one wins, over the same arguments clang is given: autocxx's own,
//! then the caller's, then bindgen's `BINDGEN_EXTRA_CLANG_ARGS`, which bindgen
//! appends after both.
//!
//! This is a reading of an argument vector and not clang's own parse of it, so
//! it can be wrong, in either direction: `-std=c++14 -I -std=c++20` parses as
//! C++14, the second `-std=` being the directory `-I` asked for, and reading it
//! as a standard would answer C++20 for a C++14 parse. So the options which take
//! their value separately are known here, and their values skipped rather than
//! read.
//!
//! A `--driver-mode=` which is not the gcc-like default spells the standard
//! `/std:c++17` and ignores `-std=` entirely, so nothing here could say what
//! clang obeys; such a vector gets no answer at all.
//!
//! [`crate::parse_callbacks::UnindexedParseCallbackResults::index`] holds a
//! backstop for whatever this still gets wrong in the direction that matters: a
//! specification the canonical type dropped proves the parse predates C++17
//! however this read the arguments. It is a backstop and not a guarantee - it
//! needs the header to declare an unconditional non-throwing specification
//! somewhere - so being right here is what does the work.

use crate::clang_target::bindgen_extra_clang_args_for_parse;
use crate::AUTOCXX_CLANG_ARGS;

/// The driver modes which read a standard the way this module does. clang's
/// default is `gcc`; `cl` and the rest take `/std:` instead.
const GCC_LIKE_DRIVER_MODES: &[&str] = &["gcc", "g++", "cpp", "cpp-output"];

/// clang options whose value is the next argument rather than part of this one.
///
/// Their values are skipped, because a value is not an option however much it
/// looks like one: the directory in `-I -std=c++20` is not a standard, and
/// reading it as one would answer for a parse which never happened.
const OPTIONS_TAKING_A_SEPARATE_VALUE: &[&str] = &[
    "-D",
    "-U",
    "-I",
    "-L",
    "-l",
    "-o",
    "-x",
    "-arch",
    "-framework",
    "-idirafter",
    "-imacros",
    "-include",
    "-iprefix",
    "-iquote",
    "-isysroot",
    "-isystem",
    "-iwithprefix",
    "-iwithprefixbefore",
    "-target",
    "-Xclang",
    "-Xpreprocessor",
    "-MF",
    "-MQ",
    "-MT",
];

/// The year of the C++ standard the headers will be parsed at, or `None` where
/// the arguments name one this does not recognize or cannot read at all.
fn parse_standard_year(extra_clang_args: &[&str]) -> Option<u32> {
    let env_args = bindgen_extra_clang_args_for_parse();
    let args: Vec<&str> = AUTOCXX_CLANG_ARGS
        .iter()
        .copied()
        .chain(extra_clang_args.iter().copied())
        .chain(env_args.iter().map(|s| s.as_str()))
        .collect();
    let mut year = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if let Some(mode) = arg.strip_prefix("--driver-mode=") {
            if !GCC_LIKE_DRIVER_MODES.contains(&mode) {
                return None;
            }
            continue;
        }
        if OPTIONS_TAKING_A_SEPARATE_VALUE.contains(&arg) {
            args.next();
            continue;
        }
        // `-std=x` and `--std=x` join the value; `--std x` separates it, which
        // clang accepts as well. A bare `-std` is not an option at all.
        let name = match arg
            .strip_prefix("-std=")
            .or_else(|| arg.strip_prefix("--std="))
        {
            Some(name) => name,
            None if arg == "--std" => args.next()?,
            None => continue,
        };
        // The last one clang is given is the one it obeys, including where it is
        // one this does not recognize: that is not an older standard in
        // disguise.
        year = standard_year(name);
    }
    year
}

/// The year of the standard a `-std=` value names.
fn standard_year(name: &str) -> Option<u32> {
    let digits = name
        .strip_prefix("c++")
        .or_else(|| name.strip_prefix("gnu++"))?;
    match digits {
        // clang's aliases for the standards which had no number while they were
        // drafts. `c++03` is C++98 plus a technical corrigendum and has no
        // feature this cares about.
        "0x" => Some(2011),
        "1y" => Some(2014),
        "1z" => Some(2017),
        "2a" => Some(2020),
        "2b" => Some(2023),
        "2c" => Some(2026),
        // `-std=c++17`, and whatever two digits a standard written after this
        // carries: `98` is the only one from the last century. Two exactly,
        // because that is how every standard is named and because `1900 + n`
        // overflows for a longer number which would still parse.
        digits if digits.len() == 2 && digits.bytes().all(|b| b.is_ascii_digit()) => digits
            .parse::<u32>()
            .ok()
            .map(|year| if year >= 90 { 1900 + year } else { 2000 + year }),
        _ => None,
    }
}

/// Whether a function's exception specification is part of its type at the
/// standard the headers are parsed at, which is what decides whether a
/// `noexcept(expr)` resolves to an answer autocxx can read.
///
/// A standard this does not recognize reads as "no": the refusal that answer
/// leads to is the one autocxx gave before it read canonical types at all.
pub(crate) fn exception_specifications_are_part_of_the_type(extra_clang_args: &[&str]) -> bool {
    parse_standard_year(extra_clang_args).is_some_and(|year| year >= 2017)
}

#[cfg(test)]
mod tests {
    use super::{
        exception_specifications_are_part_of_the_type, parse_standard_year, standard_year,
    };

    #[test]
    fn the_default_parse_resolves_conditional_specifications() {
        // Nothing passed: the standard in `AUTOCXX_CLANG_ARGS` is the answer,
        // and this is the test which fails if that is ever lowered without the
        // `noexcept(expr)` handling being revisited.
        assert!(exception_specifications_are_part_of_the_type(&[]));
    }

    #[test]
    fn a_caller_can_lower_the_parse_standard() {
        // The documented escape for C++ which does not parse at C++17. It comes
        // after autocxx's own `-std=`, so clang obeys it, and autocxx has to
        // agree with clang about which one that is.
        for args in [
            vec!["-std=c++14"],
            vec!["-std=gnu++11"],
            vec!["--std=c++03"],
            // clang accepts the separate spelling too, and obeys it.
            vec!["--std", "c++14"],
        ] {
            assert!(
                !exception_specifications_are_part_of_the_type(&args),
                "{args:?}"
            );
        }
    }

    #[test]
    fn the_last_standard_argument_wins() {
        // clang's rule, and the reason the vector is ordered as it is.
        assert!(!exception_specifications_are_part_of_the_type(&[
            "-std=c++20",
            "-std=c++11"
        ]));
        assert!(exception_specifications_are_part_of_the_type(&[
            "-std=c++11",
            "-std=c++20"
        ]));
    }

    #[test]
    fn a_raised_standard_still_resolves_them() {
        for arg in [
            "-std=c++17",
            "-std=c++1z",
            "-std=c++20",
            "-std=c++2a",
            "-std=c++23",
            "-std=c++26",
            "-std=gnu++17",
        ] {
            assert!(
                exception_specifications_are_part_of_the_type(&[arg]),
                "{arg}"
            );
        }
    }

    #[test]
    fn an_unrecognized_standard_reads_as_no() {
        // clang rejects it and bindgen reports the error, so what autocxx would
        // have concluded never matters; concluding the conservative thing keeps
        // it from mattering if that ever changes.
        assert!(!exception_specifications_are_part_of_the_type(&[
            "-std=c++latest"
        ]));
        assert!(!exception_specifications_are_part_of_the_type(&[
            "-std=c17"
        ]));
        // `--std` with nothing after it.
        assert!(!exception_specifications_are_part_of_the_type(&["--std"]));
    }

    #[test]
    fn a_driver_mode_which_spells_it_differently_gets_no_answer() {
        // clang-cl takes `/std:c++17` and ignores `-std=`, so the vector says
        // nothing about what clang will obey.
        assert_eq!(
            parse_standard_year(&["--driver-mode=cl", "/std:c++17"]),
            None
        );
        assert!(!exception_specifications_are_part_of_the_type(&[
            "--driver-mode=cl",
            "/std:c++17"
        ]));
        // The gcc-like modes read it the way this module does.
        assert_eq!(
            parse_standard_year(&["--driver-mode=g++", "-std=c++20"]),
            Some(2020)
        );
    }

    #[test]
    fn nothing_else_is_mistaken_for_a_standard() {
        for arg in [
            "-x",
            "c++",
            "-DBINDGEN",
            "-I-std=c++14",
            "-Wno-error=dynamic-exception-spec",
            "--target=x86_64-pc-windows-msvc",
            "-std",
        ] {
            assert_eq!(
                parse_standard_year(&[arg]),
                Some(2017),
                "{arg} disturbed the default"
            );
        }
    }

    #[test]
    fn a_value_which_looks_like_a_standard_is_not_one() {
        // `-I -std=c++14` names a directory and clang stays at C++17, so
        // reading the value would answer for a parse which never happened.
        assert_eq!(parse_standard_year(&["-I", "-std=c++14"]), Some(2017));
        // And the other way round, which is the direction that matters: this
        // vector parses as C++14, so answering C++20 would resolve a
        // `noexcept(expr)` from a canonical type carrying no specification.
        assert_eq!(
            parse_standard_year(&["-std=c++14", "-I", "-std=c++20"]),
            Some(2014)
        );
        // A joined value is part of its own argument and skips nothing.
        assert_eq!(parse_standard_year(&["-I.", "-std=c++14"]), Some(2014));
    }

    #[test]
    fn a_number_too_long_to_be_a_standard_is_not_one() {
        // `1900 + n` would overflow, and `-I -std=c++4294967295` reaches this
        // through a directory name.
        assert_eq!(standard_year("c++4294967295"), None);
        assert_eq!(standard_year("c++1"), None);
        assert_eq!(standard_year("c++017"), None);
        assert_eq!(standard_year("c++1a"), None);
    }

    #[test]
    fn the_century_is_read_from_the_number() {
        assert_eq!(standard_year("c++98"), Some(1998));
        assert_eq!(standard_year("c++14"), Some(2014));
        assert_eq!(standard_year("c17"), None);
    }
}
