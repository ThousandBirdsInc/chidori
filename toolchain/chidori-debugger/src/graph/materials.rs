//! Custom materials and shaders for graph rendering.
//! 
//! This file defines custom Bevy materials and shaders used to render graph elements
//! with special visual effects, including rounded rectangles for nodes and other
//! styled elements. It manages the integration between custom shaders and the
//! Bevy rendering pipeline for enhanced visual presentation.

use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, ShaderRef};
use crate::graph::types::*;

// This struct defines the data that will be passed to your shader
#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
pub struct RoundedRectMaterial {
    #[uniform(0)]
    pub width: f32,
    #[uniform(1)]
    pub height: f32,

    #[texture(2)]
    #[sampler(3)]
    pub color_texture: Option<Handle<Image>>,

    #[uniform(4)]
    pub base_color: Vec4,

    pub alpha_mode: AlphaMode,
}

/// The Material trait is very configurable, but comes with sensible defaults for all methods.
/// You only need to implement functions for features that need non-default behavior. See the Material api docs for details!
impl Material for RoundedRectMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://chidori_debugger/../../assets/shaders/rounded_rect.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        self.alpha_mode
    }
}

pub fn update_node_materials(
    mut node_query: Query<
        (Entity, &Transform, &mut GraphIdx, &Handle<RoundedRectMaterial>),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    mut materials_custom: ResMut<Assets<RoundedRectMaterial>>,
) {
    for (_, t, _, mh) in node_query.iter_mut() {
        if let Some(mat) = materials_custom.get_mut(mh) {
            mat.width = t.scale.x;
            mat.height = t.scale.y;
        }
    }
}

pub fn update_cursor_materials(
    mut execution_head_cursor: Query<
        (Entity, &mut Transform, &Handle<RoundedRectMaterial>),
        (With<ExecutionHeadCursor>, Without<GraphIdx>, Without<ExecutionSelectionCursor>, Without<GraphMainCamera>),
    >,
    mut execution_selection_cursor: Query<
        (Entity, &mut Transform, &Handle<RoundedRectMaterial>),
        (With<ExecutionSelectionCursor>, Without<GraphIdx>, Without<ExecutionHeadCursor>, Without<GraphMainCamera>),
    >,
    mut materials: ResMut<Assets<RoundedRectMaterial>>
) {
    let (_, t, mh) = execution_head_cursor.single_mut();
    if let Some(mat) = materials.get_mut(mh) {
        mat.width = t.scale.x;
        mat.height = t.scale.y;
    }
    let (_, t, mh) = execution_selection_cursor.single_mut();
    if let Some(mat) = materials.get_mut(mh) {
        mat.width = t.scale.x;
        mat.height = t.scale.y;
    }
}

// Re-export the MaterialPlugin for easy access
pub use bevy::pbr::MaterialPlugin; 