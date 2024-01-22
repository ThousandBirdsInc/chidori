// This language exists to be able to author lazily evaluated functions.
// It's possible to do this in Rust, but it's not ergonomic.

pub mod javascript;
pub mod python;

// TODO: implement a function that infers the language from the source code successfully parsing
