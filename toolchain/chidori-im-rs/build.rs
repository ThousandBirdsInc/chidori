// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if let Some(channel) = version_check::Channel::read() {
        if channel.supports_features() {
            println!("cargo:rustc-cfg=has_specialisation");
        }
    }
    let pkgname = env::var("CARGO_PKG_NAME").unwrap_or("chidori-im".to_string());
    let test_rc = env::var("IM_TEST_RC").is_ok();
    match pkgname.as_str() {
        "chidori-im" => {
            if !test_rc {
                println!("cargo:rustc-cfg=threadsafe")
            }
        }
        "chidori-im-rc" => {}
        _ => panic!("unexpected package name!"),
    }
}
