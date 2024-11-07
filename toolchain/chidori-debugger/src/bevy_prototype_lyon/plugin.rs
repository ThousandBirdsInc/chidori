//! Contains the plugin and its helper types.
//!
//! The [`ShapePlugin`] provides the creation of shapes with minimal
//! boilerplate.
//!
//! ## How it works
//! The user spawns a [`ShapeBundle`](crate::entity::ShapeBundle) from a
//! system in the `UPDATE` stage.
//!
//! Then, in [`Stage::Shape`] stage, there is a system
//! that creates a mesh for each entity that has been spawned as a
//! `ShapeBundle`.

use bevy::{
    prelude::*,
    render::{mesh::Indices, render_asset::RenderAssetUsages, render_resource::PrimitiveTopology},
};
use lyon_tessellation::{self as tess, BuffersBuilder};

use crate::bevy_prototype_lyon::{
    draw::{Fill, Stroke},
    entity::Path,
    vertex::{VertexBuffers, VertexConstructor},
};

pub(crate) const STANDARD_MATERIAL_HANDLE: Handle<StandardMaterial> =
    Handle::weak_from_u128(0x7CC6_61A1_0CD6_C147_129A_2C01_882D_9580);

/// A plugin that provides resources and a system to draw shapes in Bevy with
/// less boilerplate.
pub struct ShapePlugin;

impl Plugin for ShapePlugin {
    fn build(&self, app: &mut App) {
        let fill_tess = tess::FillTessellator::new();
        let stroke_tess = tess::StrokeTessellator::new();
        app.insert_resource(FillTessellator(fill_tess))
            .insert_resource(StrokeTessellator(stroke_tess))
            .configure_sets(
                PostUpdate,
                BuildShapes.after(bevy::transform::TransformSystem::TransformPropagate),
            )
            .add_systems(PostUpdate, mesh_shapes_system.in_set(BuildShapes));

        app.add_systems(Startup, |mut materials: ResMut<Assets<StandardMaterial>>| {
            materials.insert(
                STANDARD_MATERIAL_HANDLE,
                StandardMaterial {
                    cull_mode: None,
                    base_color: Color::WHITE,
                    unlit: true,
                    ..default()
                },
            );
        });
    }
}

/// [`SystemSet`] for the system that builds the meshes for newly-added
/// or changed shapes. Resides in [`PostUpdate`] schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SystemSet)]
pub struct BuildShapes;

/// Queries all the [`ShapeBundle`]s to mesh them when they are added
/// or re-mesh them when they are changed.
#[allow(clippy::type_complexity)]
fn mesh_shapes_system(
    mut meshes: ResMut<Assets<Mesh>>,
    mut fill_tess: ResMut<FillTessellator>,
    mut stroke_tess: ResMut<StrokeTessellator>,
    mut query: Query<
        (Option<&Fill>, Option<&Stroke>, &Path, &Handle<Mesh>),
        Or<(Changed<Path>, Changed<Fill>, Changed<Stroke>)>,
    >,
) {
    for (maybe_fill_mode, maybe_stroke_mode, path, mesh_handle) in &mut query {
        let mut buffers = VertexBuffers::new();

        if let Some(fill_mode) = maybe_fill_mode {
            fill(&mut fill_tess, &path.0, fill_mode, &mut buffers);
        }

        if let Some(stroke_mode) = maybe_stroke_mode {
            stroke(&mut stroke_tess, &path.0, stroke_mode, &mut buffers);
        }

        if (maybe_fill_mode, maybe_stroke_mode) == (None, None) {
            fill(
                &mut fill_tess,
                &path.0,
                &Fill::color(Color::FUCHSIA),
                &mut buffers,
            );
        }
        let mesh = build_mesh(&buffers);
        if meshes.get(mesh_handle).is_none() {
            meshes.insert(mesh_handle, mesh);
        } else if let Some(existing_mesh) = meshes.get_mut(mesh_handle) {
            *existing_mesh = mesh;
        }
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // lyon takes &FillOptions
fn fill(
    tess: &mut ResMut<FillTessellator>,
    path: &tess::path::Path,
    mode: &Fill,
    buffers: &mut VertexBuffers,
) {
    if let Err(e) = tess.tessellate_path(
        path,
        &mode.options,
        &mut BuffersBuilder::new(buffers, VertexConstructor { color: mode.color }),
    ) {
        error!("FillTessellator error: {:?}", e);
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // lyon takes &StrokeOptions
fn stroke(
    tess: &mut ResMut<StrokeTessellator>,
    path: &tess::path::Path,
    mode: &Stroke,
    buffers: &mut VertexBuffers,
) {
    if let Err(e) = tess.tessellate_path(
        path,
        &mode.options,
        &mut BuffersBuilder::new(buffers, VertexConstructor { color: mode.color }),
    ) {
        error!("StrokeTessellator error: {:?}", e);
    }
}

// Helper function to build a 3D mesh from vertex buffers
fn build_mesh(buffers: &VertexBuffers) -> Mesh {
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());

    // Convert positions from 2D to 3D by adding z=0
    let positions: Vec<[f32; 3]> = buffers.vertices
        .iter()
        .map(|vertex| [vertex.position[0], vertex.position[1], vertex.position[2]])
        .collect();

    // Create normals pointing in +z direction for all vertices
    let normals: Vec<[f32; 3]> = vec![[0.0, 0.0, -1.0]; positions.len()];

    // Convert indices to u32
    let indices: Vec<u32> = buffers.indices.iter()
        .map(|&idx| idx as u32)
        .collect();

    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);

    mesh.insert_attribute(
        Mesh::ATTRIBUTE_COLOR,
        buffers
            .vertices
            .iter()
            .map(|v| v.color)
            .collect::<Vec<[f32; 4]>>(),
    );
    mesh.insert_indices(Indices::U32(indices));


    mesh
}

#[derive(Resource, Deref, DerefMut)]
struct FillTessellator(lyon_tessellation::FillTessellator);

#[derive(Resource, Deref, DerefMut)]
struct StrokeTessellator(lyon_tessellation::StrokeTessellator);
