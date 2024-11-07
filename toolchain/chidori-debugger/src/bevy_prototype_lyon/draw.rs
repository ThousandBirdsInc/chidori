//! Types for defining shape color and options.

use bevy::{ecs::component::Component};
use bevy::prelude::Color;
use lyon_tessellation::{FillOptions, StrokeOptions};

/// Defines the fill options for the lyon tessellator and color of the generated
/// vertices.
#[allow(missing_docs)]
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Fill {
    pub options: FillOptions,
    pub color: Color,
}

impl Fill {
    /// Convenience constructor requiring only the `Color`.
    #[must_use]
    pub fn color(color: impl Into<Color>) -> Self {
        Self {
            options: FillOptions::default(),
            color: color.into(),
        }
    }
}

/// Defines the stroke options for the lyon tessellator and color of the
/// generated vertices.
#[allow(missing_docs)]
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Stroke {
    pub options: StrokeOptions,
    pub color: Color,
}

impl Stroke {
    /// Constructor that requires a `Color` and a line width.
    #[must_use]
    pub fn new(color: impl Into<Color>, line_width: f32) -> Self {
        Self {
            options: StrokeOptions::default().with_line_width(line_width),
            color: color.into(),
        }
    }

    /// Convenience constructor requiring only the `Color`.
    #[must_use]
    pub fn color(color: impl Into<Color>) -> Self {
        Self {
            options: StrokeOptions::default(),
            color: color.into(),
        }
    }
}
