//! Types outputting lyon `Path`s.

use bevy::math::Vec2;
use lyon_tessellation::{
    geom::Angle,
    path::{
        builder::WithSvg,
        path::{Builder, BuilderImpl},
        EndpointId,
    },
};

use crate::bevy_prototype_lyon::{
    entity::Path,
    geometry::Geometry,
    utils::{ToPoint, ToVector},
};

/// A builder for `Path`s based on shapes implementing [`Geometry`].
pub struct ShapePath(Builder);

impl ShapePath {
    /// Returns a new builder.
    #[must_use]
    pub fn new() -> Self {
        Self(Builder::new())
    }

    /// Adds a shape to the builder.
    ///
    /// # Example
    ///
    /// ```
    /// # use bevy::prelude::*;
    /// # use bevy_prototype_lyon::prelude::{RegularPolygon, *};
    /// #
    /// # #[derive(Component)]
    /// # struct Player;
    /// #
    /// fn my_system(mut query: Query<&mut Path, With<Player>>) {
    ///     let mut path = query.single_mut();
    ///
    ///     let square = shapes::Rectangle {
    ///         extents: Vec2::splat(50.0),
    ///         ..shapes::Rectangle::default()
    ///     };
    ///     let triangle = RegularPolygon {
    ///         sides: 3,
    ///         center: Vec2::new(100.0, 0.0),
    ///         ..RegularPolygon::default()
    ///     };
    ///
    ///     *path = ShapePath::new().add(&square).add(&triangle).build();
    /// }
    /// # bevy::ecs::system::assert_is_system(my_system);
    /// ```
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn add(mut self, shape: &impl Geometry) -> Self {
        shape.add_geometry(&mut self.0);
        self
    }

    /// Builds the `Path` and returns it.
    #[must_use]
    pub fn build(self) -> Path {
        Path(self.0.build())
    }

    /// Directly builds a `Path` from a `shape`.
    ///
    /// # Example
    ///
    /// ```
    /// # use bevy::prelude::*;
    /// # use bevy_prototype_lyon::prelude::{RegularPolygon, *};
    /// #
    /// # #[derive(Component)]
    /// # struct Player;
    /// #
    /// fn my_system(mut query: Query<&mut Path, With<Player>>) {
    ///     let mut path = query.single_mut();
    ///
    ///     let triangle = RegularPolygon {
    ///         sides: 3,
    ///         center: Vec2::new(100.0, 0.0),
    ///         ..RegularPolygon::default()
    ///     };
    ///
    ///     *path = ShapePath::build_as(&triangle);
    /// }
    /// # bevy::ecs::system::assert_is_system(my_system);
    /// ```
    pub fn build_as(shape: &impl Geometry) -> Path {
        Self::new().add(shape).build()
    }
}

impl Default for ShapePath {
    fn default() -> Self {
        Self::new()
    }
}

/// A SVG-like path builder.
pub struct PathBuilder(WithSvg<BuilderImpl>);

impl PathBuilder {
    /// Returns a new, empty `PathBuilder`.
    #[must_use]
    pub fn new() -> Self {
        Self(Builder::new().with_svg())
    }

    /// Returns a finalized [`Path`].
    #[must_use]
    pub fn build(self) -> Path {
        Path(self.0.build())
    }

    /// Moves the current point to the given position.
    pub fn move_to(&mut self, to: Vec2) -> EndpointId {
        self.0.move_to(to.to_point())
    }

    /// Adds to the path a line from the current position to the given one.
    pub fn line_to(&mut self, to: Vec2) -> EndpointId {
        self.0.line_to(to.to_point())
    }

    /// Closes the shape, adding to the path a line from the current position to
    /// the starting location.
    pub fn close(&mut self) {
        self.0.close();
    }

    /// Adds a quadratic bezier to the path.
    pub fn quadratic_bezier_to(&mut self, ctrl: Vec2, to: Vec2) -> EndpointId {
        self.0.quadratic_bezier_to(ctrl.to_point(), to.to_point())
    }

    /// Adds a cubic bezier to the path.
    pub fn cubic_bezier_to(&mut self, ctrl1: Vec2, ctrl2: Vec2, to: Vec2) -> EndpointId {
        self.0
            .cubic_bezier_to(ctrl1.to_point(), ctrl2.to_point(), to.to_point())
    }

    /// Adds an arc to the path.
    pub fn arc(&mut self, center: Vec2, radii: Vec2, sweep_angle: f32, x_rotation: f32) {
        self.0.arc(
            center.to_point(),
            radii.to_vector(),
            Angle::radians(sweep_angle),
            Angle::radians(x_rotation),
        );
    }

    /// Returns the path's current position.
    #[must_use]
    pub fn current_position(&self) -> Vec2 {
        let p = self.0.current_position();
        Vec2::new(p.x, p.y)
    }
}

impl Default for PathBuilder {
    fn default() -> Self {
        Self::new()
    }
}
