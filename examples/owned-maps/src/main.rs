// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Two `std::map` instantiations, in every position a C++ API puts one.

use autocxx::prelude::*;

include_cpp! {
    #include "config.h"
    safety!(unsafe_ffi)
    generate!("Config")
    concrete!("std::map<std::string, std::string>", Settings)
    concrete!("std::map<uint32_t, uint32_t>", Limits)
}

fn main() {
    let mut config = ffi::Config::new().within_unique_ptr();

    // A map returned by value: Rust owns it.
    let settings = config.settings();
    cxx::let_cxx_string!(theme = "theme");
    assert_eq!(settings.len(), 2);
    assert_eq!(settings.get(&theme).unwrap().to_str().unwrap(), "dark");

    // A map returned by reference: the store's own, read in place.
    let limits = config.limits();
    assert_eq!(limits.get(1u32), Some(&4096u32));

    // A map Rust builds and C++ reads.
    let mut overrides = ffi::Settings::new();
    cxx::let_cxx_string!(light = "light");
    cxx::let_cxx_string!(locale = "locale");
    cxx::let_cxx_string!(fr = "fr");
    overrides.pin_mut().insert(&theme, &light);
    overrides.pin_mut().insert(&locale, &fr);
    config.pin_mut().merge(&overrides);
    assert_eq!(config.setting_count(), 2);
    let settings = config.settings();
    assert_eq!(settings.get(&theme).unwrap().to_str().unwrap(), "light");

    // A map C++ writes through.
    let mut limits = ffi::Limits::new();
    limits.pin_mut().insert(1u32, 100u32);
    config.raise(limits.pin_mut());
    assert_eq!(limits.get(1u32), Some(&200u32));

    // Snapshots, in key order, lining up entry for entry.
    let keys = settings.keys();
    let values = settings.values();
    let pairs: Vec<(String, String)> = keys
        .iter()
        .zip(values.iter())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("locale".to_string(), "fr".to_string()),
            ("theme".to_string(), "light".to_string()),
        ]
    );

    println!("{pairs:?}");
}
