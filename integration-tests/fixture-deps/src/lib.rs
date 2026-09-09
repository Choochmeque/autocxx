// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The dependencies autocxx's test fixtures are built with, and nothing else.
//!
//! trybuild builds each fixture as a crate of its own, and composes that
//! crate's manifest from the `[dependencies]` of whichever package
//! `CARGO_MANIFEST_DIR` names. The integration-test harness names this package,
//! so this package's manifest is the fixture manifest - see its comments for
//! what is in it and why.
//!
//! There is no code here to run, and the library is empty on purpose. It has to
//! be a library rather than nothing at all, because trybuild adds the package it
//! was pointed at to the fixture's own dependencies whenever that package has a
//! library target - which is what carries this crate's build script, and with it
//! the link search path the fixture needs, into the fixture's build.
