// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The `returns_borrow_from!` directive: which parameter of a C++ function the
//! reference it returns borrows from.

use std::fmt::Display;

/// The parameter a returned C++ reference borrows from, as
/// `returns_borrow_from!` spells it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BorrowSource {
    /// The object a member function was called on: `"self"` or `"this"`.
    Receiver,
    /// A parameter under the name the C++ declaration gives it.
    Named(String),
    /// A parameter by position, counting from zero across the declared
    /// parameters and not counting the receiver, written `"#0"`. For the
    /// parameter C++ declared without a name, which has none to write.
    Position(usize),
}

/// Why a `returns_borrow_from!` second argument was rejected.
#[derive(Debug)]
pub(crate) enum BorrowSourceError {
    /// Neither a parameter name nor a `#0` position.
    NotAParameter,
    /// A `#`, and then something which is not a number.
    NotAPosition,
}

impl BorrowSource {
    /// Read the parameter a directive named, or say why it is unreadable.
    ///
    /// Settled here, where the string literal the user wrote still has a span
    /// to point at, rather than against a particular C++ declaration much
    /// later: a misspelling is a misspelling whatever function it was written
    /// for.
    pub(crate) fn parse(spelling: &str) -> Result<Self, BorrowSourceError> {
        if spelling == "self" || spelling == "this" {
            return Ok(BorrowSource::Receiver);
        }
        if let Some(position) = spelling.strip_prefix('#') {
            return position
                .parse()
                .map(BorrowSource::Position)
                .map_err(|_| BorrowSourceError::NotAPosition);
        }
        if !is_plain_identifier(spelling) {
            return Err(BorrowSourceError::NotAParameter);
        }
        Ok(BorrowSource::Named(spelling.to_string()))
    }
}

impl Display for BorrowSource {
    /// The spelling `BorrowSource::parse` reads back, which is what the
    /// reproduction case has to write and what a diagnostic quotes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BorrowSource::Receiver => write!(f, "self"),
            BorrowSource::Named(name) => write!(f, "{name}"),
            BorrowSource::Position(position) => write!(f, "#{position}"),
        }
    }
}

/// Whether `name` is one plain identifier, which is all a C++ parameter name
/// can be.
fn is_plain_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::BorrowSource;

    #[test]
    fn test_parameter_spellings() {
        assert_eq!(BorrowSource::parse("self").unwrap(), BorrowSource::Receiver);
        assert_eq!(BorrowSource::parse("this").unwrap(), BorrowSource::Receiver);
        assert_eq!(
            BorrowSource::parse("key").unwrap(),
            BorrowSource::Named("key".into())
        );
        assert_eq!(
            BorrowSource::parse("#0").unwrap(),
            BorrowSource::Position(0)
        );
        assert_eq!(
            BorrowSource::parse("#12").unwrap(),
            BorrowSource::Position(12)
        );
        assert!(BorrowSource::parse("").is_err());
        assert!(BorrowSource::parse("#").is_err());
        assert!(BorrowSource::parse("#x").is_err());
        assert!(BorrowSource::parse("#-1").is_err());
        assert!(BorrowSource::parse("a::b").is_err());
        assert!(BorrowSource::parse("&key").is_err());
        assert!(BorrowSource::parse("2key").is_err());
    }

    /// The reproduction case writes a source back out and re-parses it, so
    /// every spelling has to survive the round trip.
    #[test]
    fn test_spellings_round_trip() {
        for source in [
            BorrowSource::Receiver,
            BorrowSource::Named("key".into()),
            BorrowSource::Position(3),
        ] {
            assert_eq!(BorrowSource::parse(&source.to_string()).unwrap(), source);
        }
    }
}
