//! Utility types and conversion traits.

use bevy::math::Vec2;
use lyon_tessellation::math::{Point, Vector};

pub trait ToPoint {
    fn to_point(self) -> Point;
}

pub trait ToVector {
    fn to_vector(self) -> Vector;
}

impl ToPoint for Vec2 {
    fn to_point(self) -> Point {
        Point::new(self.x, self.y)
    }
}

impl ToVector for Vec2 {
    fn to_vector(self) -> Vector {
        Vector::new(self.x, self.y)
    }
}
