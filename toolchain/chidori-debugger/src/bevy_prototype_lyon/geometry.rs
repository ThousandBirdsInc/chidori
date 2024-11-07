//! Types for defining and using geometries.

use lyon_tessellation::path::path::Builder;

use crate::bevy_prototype_lyon::entity::Path;

/// Structs that implement this trait can be drawn as a shape. See the
/// [`shapes`](crate::shapes) module for some examples.
///
/// # Implementation example
///
/// ```
/// use bevy_prototype_lyon::geometry::Geometry;
/// use lyon_tessellation::{
///     math::{Box2D, Point, Size},
///     path::{path::Builder, traits::PathBuilder, Path, Winding},
/// };
///
/// // First, create a struct to hold the shape features:
/// #[derive(Debug, Clone, Copy, PartialEq)]
/// pub struct Rectangle {
///     pub width: f32,
///     pub height: f32,
/// }
///
/// // Implementing the `Default` trait is not required, but it may facilitate the
/// // definition of the shape before spawning it.
/// impl Default for Rectangle {
///     fn default() -> Self {
///         Self {
///             width: 1.0,
///             height: 1.0,
///         }
///     }
/// }
///
/// // Finally, implement the `add_geometry` method.
/// impl Geometry for Rectangle {
///     fn add_geometry(&self, b: &mut Builder) {
///         b.add_rectangle(
///             &Box2D::new(Point::zero(), Point::new(self.width, self.height)),
///             Winding::Positive,
///         );
///     }
/// }
/// ```
pub trait Geometry {
    /// Adds the geometry of the shape to the given Lyon path `Builder`.
    fn add_geometry(&self, b: &mut Builder);
}

/// Allows the creation of shapes using geometries added to a path builder.
pub struct GeometryBuilder(Builder);

impl GeometryBuilder {
    /// Creates a new, empty `GeometryBuilder`.
    #[must_use]
    pub fn new() -> Self {
        Self(Builder::new())
    }

    /// Adds a geometry to the path builder.
    ///
    /// # Example
    ///
    /// ```
    /// # use bevy::prelude::*;
    /// # use bevy_prototype_lyon::prelude::*;
    /// # use bevy::color::palettes;
    /// #
    /// fn my_system(mut commands: Commands) {
    ///     let line = shapes::Line(Vec2::ZERO, Vec2::new(10.0, 0.0));
    ///     let square = shapes::Rectangle {
    ///         extents: Vec2::splat(100.0),
    ///         ..shapes::Rectangle::default()
    ///     };
    ///     let mut builder = GeometryBuilder::new().add(&line).add(&square);
    ///
    ///     commands.spawn((
    ///         ShapeBundle {
    ///             path: builder.build(),
    ///             ..default()
    ///         },
    ///         Fill::color(Color::Srgba(palettes::css::ORANGE_RED)),
    ///         Stroke::new(Color::Srgba(palettes::css::ORANGE_RED), 10.0),
    ///     ));
    /// }
    /// # bevy::ecs::system::assert_is_system(my_system);
    /// ```
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn add(mut self, shape: &impl Geometry) -> Self {
        shape.add_geometry(&mut self.0);
        self
    }

    /// Returns a [`Path`] using the data contained in the geometry
    /// builder.
    #[must_use]
    pub fn build(self) -> Path {
        Path(self.0.build())
    }

    /// Returns a [`Path`] component with only one geometry.
    ///
    /// # Example
    ///
    /// ```
    /// # use bevy::prelude::*;
    /// # use bevy_prototype_lyon::prelude::*;
    /// # use bevy::color::palettes;
    /// #
    /// fn my_system(mut commands: Commands) {
    ///     let line = shapes::Line(Vec2::ZERO, Vec2::new(10.0, 0.0));
    ///     commands.spawn((
    ///         ShapeBundle {
    ///             path: GeometryBuilder::build_as(&line),
    ///             ..default()
    ///         },
    ///         Fill::color(Color::Srgba(palettes::css::ORANGE_RED)),
    ///     ));
    /// }
    /// # bevy::ecs::system::assert_is_system(my_system);
    /// ```
    pub fn build_as(shape: &impl Geometry) -> Path {
        Self::new().add(shape).build()
    }
}

impl Default for GeometryBuilder {
    fn default() -> Self {
        Self::new()
    }
}
