//! Collection of common shapes that can be drawn.
//!
//! The structs defined in this module implement the
//! [`Geometry`](crate::geometry::Geometry) trait. You can also implement
//! the trait for your own shapes.

use bevy::math::Vec2;
use lyon_tessellation::{
    geom::euclid::default::Size2D,
    math::{point, Angle, Box2D, Point, Vector},
    path::{
        builder::WithSvg, path::Builder, traits::SvgPathBuilder, ArcFlags, Polygon as LyonPolygon,
        Winding,
    },
};
use svgtypes::{PathParser, PathSegment};

use crate::bevy_prototype_lyon::{
    geometry::Geometry,
    utils::{ToPoint, ToVector},
};

/// Defines where the origin, or pivot of the `Rectangle` should be positioned.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RectangleOrigin {
    Center,
    BottomLeft,
    BottomRight,
    TopRight,
    TopLeft,
    CustomCenter(Vec2),
}

impl Default for RectangleOrigin {
    fn default() -> Self {
        Self::Center
    }
}

#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rectangle {
    pub extents: Vec2,
    pub origin: RectangleOrigin,
}

impl Default for Rectangle {
    fn default() -> Self {
        Self {
            extents: Vec2::ONE,
            origin: RectangleOrigin::default(),
        }
    }
}

impl Geometry for Rectangle {
    fn add_geometry(&self, b: &mut Builder) {
        let origin = match self.origin {
            RectangleOrigin::Center => Point::new(-self.extents.x / 2.0, -self.extents.y / 2.0),
            RectangleOrigin::BottomLeft => Point::new(0.0, 0.0),
            RectangleOrigin::BottomRight => Point::new(-self.extents.x, 0.0),
            RectangleOrigin::TopRight => Point::new(-self.extents.x, -self.extents.y),
            RectangleOrigin::TopLeft => Point::new(0.0, -self.extents.y),
            RectangleOrigin::CustomCenter(v) => {
                Point::new(v.x - self.extents.x / 2.0, v.y - self.extents.y / 2.0)
            }
        };

        b.add_rectangle(
            &Box2D::from_origin_and_size(origin, Size2D::new(self.extents.x, self.extents.y)),
            Winding::Positive,
        );
    }
}

#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Circle {
    pub radius: f32,
    pub center: Vec2,
}

impl Default for Circle {
    fn default() -> Self {
        Self {
            radius: 1.0,
            center: Vec2::ZERO,
        }
    }
}

impl Geometry for Circle {
    fn add_geometry(&self, b: &mut Builder) {
        b.add_circle(self.center.to_point(), self.radius, Winding::Positive);
    }
}

#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ellipse {
    pub radii: Vec2,
    pub center: Vec2,
}

impl Default for Ellipse {
    fn default() -> Self {
        Self {
            radii: Vec2::ONE,
            center: Vec2::ZERO,
        }
    }
}

impl Geometry for Ellipse {
    fn add_geometry(&self, b: &mut Builder) {
        b.add_ellipse(
            self.center.to_point(),
            self.radii.to_vector(),
            Angle::zero(),
            Winding::Positive,
        );
    }
}

#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq)]
pub struct Polygon {
    pub points: Vec<Vec2>,
    pub closed: bool,
}

impl Default for Polygon {
    fn default() -> Self {
        Self {
            points: Vec::new(),
            closed: true,
        }
    }
}

impl Geometry for Polygon {
    fn add_geometry(&self, b: &mut Builder) {
        let points = self
            .points
            .iter()
            .map(|p| p.to_point())
            .collect::<Vec<Point>>();
        let polygon: LyonPolygon<Point> = LyonPolygon {
            points: points.as_slice(),
            closed: self.closed,
        };

        b.add_polygon(polygon);
    }
}

#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq)]
pub struct RoundedPolygon {
    pub points: Vec<Vec2>,
    pub radius: f32,
    pub closed: bool,
}

impl Default for RoundedPolygon {
    fn default() -> Self {
        Self {
            points: Vec::new(),
            radius: 0.0,
            closed: true,
        }
    }
}

impl Geometry for RoundedPolygon {
    fn add_geometry(&self, b: &mut Builder) {
        let points = self
            .points
            .iter()
            .map(|p| p.to_point())
            .collect::<Vec<Point>>();
        let polygon: LyonPolygon<Point> = LyonPolygon {
            points: points.as_slice(),
            closed: self.closed,
        };
        lyon_algorithms::rounded_polygon::add_rounded_polygon(
            b,
            polygon,
            self.radius,
            lyon_algorithms::path::NO_ATTRIBUTES,
        );
    }
}

/// The regular polygon feature used to determine the dimensions of the polygon.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RegularPolygonFeature {
    /// The radius of the polygon's circumcircle.
    Radius(f32),
    /// The radius of the polygon's incircle.
    Apothem(f32),
    /// The length of the polygon's side.
    SideLength(f32),
}

#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegularPolygon {
    pub sides: usize,
    pub center: Vec2,
    pub feature: RegularPolygonFeature,
}

impl RegularPolygon {
    /// Gets the radius of the polygon.
    fn radius(&self) -> f32 {
        let ratio = std::f32::consts::PI / self.sides as f32;

        match self.feature {
            RegularPolygonFeature::Radius(r) => r,
            RegularPolygonFeature::Apothem(a) => a * ratio.tan() / ratio.sin(),
            RegularPolygonFeature::SideLength(s) => s / (2.0 * ratio.sin()),
        }
    }
}

impl Default for RegularPolygon {
    fn default() -> Self {
        Self {
            sides: 3,
            center: Vec2::ZERO,
            feature: RegularPolygonFeature::Radius(1.0),
        }
    }
}

impl Geometry for RegularPolygon {
    fn add_geometry(&self, b: &mut Builder) {
        // -- Implementation details **PLEASE KEEP UPDATED** --
        // - `step`: angle between two vertices.
        // - `internal`: internal angle of the polygon.
        // - `offset`: bias to make the shape lay flat on a line parallel to the x-axis.

        use std::f32::consts::PI;
        assert!(self.sides > 2, "Polygons must have at least 3 sides");
        let n = self.sides as f32;
        let radius = self.radius();
        let internal = (n - 2.0) * PI / n;
        let offset = -internal / 2.0;

        let mut points = Vec::with_capacity(self.sides);
        let step = 2.0 * PI / n;
        for i in 0..self.sides {
            let cur_angle = (i as f32).mul_add(step, offset);
            let x = radius.mul_add(cur_angle.cos(), self.center.x);
            let y = radius.mul_add(cur_angle.sin(), self.center.y);
            points.push(point(x, y));
        }

        let polygon = LyonPolygon {
            points: points.as_slice(),
            closed: true,
        };

        b.add_polygon(polygon);
    }
}

/// A simple line segment, specified by two points.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Line(pub Vec2, pub Vec2);

impl Geometry for Line {
    fn add_geometry(&self, b: &mut Builder) {
        b.add_polygon(LyonPolygon {
            points: &[self.0.to_point(), self.1.to_point()],
            closed: false,
        });
    }
}
///An easy way to display svg paths as a shape, takes an svg path string and a
///document size(Vec2).
///
///For documentation on svg paths: <https://developer.mozilla.org/en-US/docs/Web/SVG/Tutorial/Paths>
///
///Make sure that your units are pixels(px) and that the transform of the \<g\>
///in your svg document is set to transform="translate(0,0)" so as to not
///offset the coordinates of the paths
///
///In inkscape for example, to turn your units into pixels, you:
/// 1) Go to File>Document Properties>General>Display Units and set it to px
///
/// 2) In File>Document Properties>Custom Size>Units set it to px, also, this
///    size would be used for `svg_doc_size_in_px`
///
/// 3) In File>Document Properties>Scale>Scale x make sure it is set to 1 User
///    unit per px
///
///Example exists in the examples folder
pub struct SvgPathShape {
    ///The document size of the svg art, make sure the units are in pixels
    pub svg_doc_size_in_px: Vec2,
    ///The string that describes the path, make sure the units are in pixels
    ///and that the transform of the \<g\> in your svg document is set to
    ///transform="translate(0,0)" so as to not offset the coordinates of the
    ///paths
    pub svg_path_string: String,
}
fn get_y_in_bevy_orientation(y: f64) -> f32 {
    y as f32 * -1.
}
fn get_y_after_offset(y: f64, offset_y: f32) -> f32 {
    get_y_in_bevy_orientation(y) + offset_y
}
fn get_x_after_offset(x: f64, offset_x: f32) -> f32 {
    x as f32 - offset_x
}
fn get_point_after_offset(x: f64, y: f64, offset_x: f32, offset_y: f32) -> Point {
    Point::new(
        get_x_after_offset(x, offset_x),
        get_y_after_offset(y, offset_y),
    )
}
fn get_corrected_relative_vector(x: f64, y: f64) -> Vector {
    Vector::new(x as f32, get_y_in_bevy_orientation(y))
}
impl Geometry for SvgPathShape {
    #[allow(clippy::too_many_lines)]
    fn add_geometry(&self, b: &mut Builder) {
        let builder = Builder::new();
        let mut svg_builder = WithSvg::new(builder);
        let offset_x = self.svg_doc_size_in_px.x / 2.;
        let offset_y = self.svg_doc_size_in_px.y / 2.;
        let mut used_move_command = false;

        for path_segment in PathParser::from(self.svg_path_string.as_str()) {
            match path_segment.unwrap() {
                PathSegment::MoveTo { abs, x, y } => {
                    if abs || !used_move_command {
                        svg_builder.move_to(get_point_after_offset(x, y, offset_x, offset_y));
                        used_move_command = true;
                    } else {
                        svg_builder.relative_move_to(get_corrected_relative_vector(x, y));
                    }
                }
                PathSegment::LineTo { abs, x, y } => {
                    if abs {
                        svg_builder.line_to(get_point_after_offset(x, y, offset_x, offset_y));
                    } else {
                        svg_builder.relative_line_to(get_corrected_relative_vector(x, y));
                    }
                }
                PathSegment::HorizontalLineTo { abs, x } => {
                    if abs {
                        svg_builder.horizontal_line_to(get_x_after_offset(x, offset_x));
                    } else {
                        svg_builder.relative_horizontal_line_to(x as f32);
                    }
                }
                PathSegment::VerticalLineTo { abs, y } => {
                    if abs {
                        svg_builder.vertical_line_to(get_y_after_offset(y, offset_y));
                    } else {
                        svg_builder.relative_vertical_line_to(get_y_in_bevy_orientation(y));
                    }
                }
                PathSegment::CurveTo {
                    abs,
                    x1,
                    y1,
                    x2,
                    y2,
                    x,
                    y,
                } => {
                    if abs {
                        svg_builder.cubic_bezier_to(
                            get_point_after_offset(x1, y1, offset_x, offset_y),
                            get_point_after_offset(x2, y2, offset_x, offset_y),
                            get_point_after_offset(x, y, offset_x, offset_y),
                        );
                    } else {
                        svg_builder.relative_cubic_bezier_to(
                            get_corrected_relative_vector(x1, y1),
                            get_corrected_relative_vector(x2, y2),
                            get_corrected_relative_vector(x, y),
                        );
                    }
                }
                PathSegment::SmoothCurveTo { abs, x2, y2, x, y } => {
                    if abs {
                        svg_builder.smooth_cubic_bezier_to(
                            get_point_after_offset(x2, y2, offset_x, offset_y),
                            get_point_after_offset(x, y, offset_x, offset_y),
                        );
                    } else {
                        svg_builder.smooth_relative_cubic_bezier_to(
                            get_corrected_relative_vector(x2, y2),
                            get_corrected_relative_vector(x, y),
                        );
                    }
                }
                PathSegment::Quadratic { abs, x1, y1, x, y } => {
                    if abs {
                        svg_builder.quadratic_bezier_to(
                            get_point_after_offset(x1, y1, offset_x, offset_y),
                            get_point_after_offset(x, y, offset_x, offset_y),
                        );
                    } else {
                        svg_builder.relative_quadratic_bezier_to(
                            get_corrected_relative_vector(x1, y1),
                            get_corrected_relative_vector(x, y),
                        );
                    }
                }
                PathSegment::SmoothQuadratic { abs, x, y } => {
                    if abs {
                        svg_builder.smooth_quadratic_bezier_to(get_point_after_offset(
                            x, y, offset_x, offset_y,
                        ));
                    } else {
                        svg_builder.smooth_relative_quadratic_bezier_to(
                            get_corrected_relative_vector(x, y),
                        );
                    }
                }
                PathSegment::EllipticalArc {
                    abs,
                    rx,
                    ry,
                    x_axis_rotation,
                    large_arc,
                    sweep,
                    x,
                    y,
                } => {
                    if abs {
                        svg_builder.arc_to(
                            Vector::new(rx as f32, ry as f32),
                            Angle {
                                radians: x_axis_rotation as f32,
                            },
                            ArcFlags { large_arc, sweep },
                            get_point_after_offset(x, y, offset_x, offset_y),
                        );
                    } else {
                        svg_builder.relative_arc_to(
                            Vector::new(rx as f32, ry as f32),
                            Angle {
                                radians: x_axis_rotation as f32,
                            },
                            ArcFlags { large_arc, sweep },
                            get_corrected_relative_vector(x, y),
                        );
                    }
                }
                PathSegment::ClosePath { abs: _ } => {
                    svg_builder.close();
                }
            }
        }
        let path = svg_builder.build();
        b.extend_from_paths(&[path.as_slice()]);
    }
}
