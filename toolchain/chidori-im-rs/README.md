# This is a FORK of the `im` crate.

Our primary modification is to modify the HashMap implementation to support streaming changes
to durable storage in an efficient manner. In this context of Chidori this is how we're handling
the storage of the state of the system.

The original README.md is below.

------------------------------------------------------------------------------------------------------------------------
# im-rs

[![Crate Status](https://img.shields.io/crates/v/im.svg)](https://crates.io/crates/im)

Blazing fast immutable collection datatypes for Rust.

Comes in two versions: [`im`](https://crates.io/crates/im) (thread safe) and
[`im-rc`](https://crates.io/crates/im-rc) (fast but not thread safe).

## Documentation

* [API docs](https://docs.rs/im/)

## Licence

Copyright 2017 Bodil Stokke

This software is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at http://mozilla.org/MPL/2.0/.

## Code of Conduct

Please note that this project is released with a [Contributor Code of
Conduct][coc]. By participating in this project you agree to abide by its
terms.

[coc]: https://github.com/bodil/im-rs/blob/master/CODE_OF_CONDUCT.md
