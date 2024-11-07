//! Draw 2D shapes in Bevy.
//!
//! This crate provides a Bevy [plugin] to easily draw shapes.
//! Some shapes are provided for convenience, however you can extend the
//! functionality of this crate by implementing the
//! [`Geometry`](geometry::Geometry) trait by your own.
//!
//! ## Usage
//! Check out the `README.md` on the [**GitHub repository**](https://github.com/Nilirad/bevy_prototype_lyon)
//! or run the [examples](https://github.com/Nilirad/bevy_prototype_lyon/tree/master/examples).

// rustc
#![deny(future_incompatible, nonstandard_style)]
#![warn(missing_docs, rust_2018_idioms, unused)]
#![allow(elided_lifetimes_in_paths)]
// clippy
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::multiple_crate_versions)] // this is a dependency problem
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::needless_pass_by_value)] // False positives with `SystemParam`s.
#![allow(clippy::forget_non_drop)]
#![allow(clippy::missing_const_for_fn)]

pub mod draw;
pub mod entity;
pub mod geometry;
pub mod path;
pub mod plugin;
pub mod shapes;

mod utils;
mod vertex;

/// Import this module as `use bevy_prototype_lyon::prelude::*` to get
/// convenient imports.
pub mod prelude {
    pub use lyon_tessellation::{
        self as tess, FillOptions, FillRule, LineCap, LineJoin, Orientation, StrokeOptions,
    };

    pub use crate::bevy_prototype_lyon::{
        draw::{Fill, Stroke},
        entity::{Path, ShapeBundle},
        geometry::{Geometry, GeometryBuilder},
        path::{PathBuilder, ShapePath},
        plugin::ShapePlugin,
        shapes::{self, RectangleOrigin, RegularPolygon, RegularPolygonFeature},
    };
}
