//! Custom Bevy ECS bundle for 3D shapes.

use bevy::utils::Uuid;
use bevy::prelude::*;
use bevy::render::mesh::Mesh;
use lyon_tessellation::{self as tess};

use crate::bevy_prototype_lyon::{geometry::Geometry};
use crate::bevy_prototype_lyon::plugin::STANDARD_MATERIAL_HANDLE;

/// A Bevy `Bundle` to represent a 3D shape.
#[allow(missing_docs)]
#[derive(Bundle, Clone)]
pub struct ShapeBundle {
    pub path: Path,
    pub mesh: Handle<Mesh>,
    pub material: Handle<StandardMaterial>,
    pub transform: Transform,
    pub global_transform: GlobalTransform,
    /// User indication of whether an entity is visible
    pub visibility: Visibility,
    /// Inherited visibility of an entity.
    pub inherited_visibility: InheritedVisibility,
    /// Algorithmically-computed indication of whether an entity is visible and should be extracted for rendering
    pub view_visibility: ViewVisibility,
}

impl Default for ShapeBundle {
    fn default() -> Self {
        Self {
            path: default(),
            mesh: Handle::<Mesh>::weak_from_u128(Uuid::new_v4().as_u128()),
            material: STANDARD_MATERIAL_HANDLE,
            transform: default(),
            global_transform: default(),
            visibility: default(),
            inherited_visibility: default(),
            view_visibility: default(),
        }
    }
}

#[allow(missing_docs)]
#[derive(Component, Default, Clone)]
pub struct Path(pub tess::path::Path);

impl Geometry for Path {
    fn add_geometry(&self, b: &mut tess::path::path::Builder) {
        b.extend_from_paths(&[self.0.as_slice()]);
    }
}