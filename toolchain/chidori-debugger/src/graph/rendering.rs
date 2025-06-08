//! Graph rendering and visualization systems.
//! 
//! This file handles the core rendering logic for the graph visualization, including
//! drawing nodes and edges, managing visual styles and themes, coordinate transformations
//! between screen and world space, and integrating with the egui UI system for displaying
//! node details and interactive elements within the graph view.

use crate::application::ChidoriState;
use crate::graph::types::*;
use crate::graph::layout::generate_tree_layout;
use crate::{bevy_egui, CurrentTheme, Theme, RENDER_LAYER_GRAPH_VIEW};
use bevy::math::{vec2, vec3, Vec3};
use bevy::prelude::*;
use bevy_utils::tracing::{info, warn};
use bevy::render::render_resource::{Extent3d, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages};
use bevy::render::view::RenderLayers;
use bevy::window::PrimaryWindow;
use bevy_rapier2d::geometry::Collider;
use bevy_rapier2d::prelude::Sensor;
use chidori_core::execution::execution::execution_graph::ExecutionNodeId;
use dashmap::DashMap;
use egui::{Color32, Context, Frame, Pos2, RichText, Stroke, TextureHandle, Ui};
use egui_json_tree::JsonTree;
use image::{DynamicImage, ImageBuffer, RgbImage, RgbaImage};
use num::ToPrimitive;
use petgraph::data::DataMap;
use petgraph::visit::Topo;
use std::collections::HashMap;
use crate::bevy_egui::{EguiContext, EguiRenderTarget};
use crate::util::{egui_render_cell_function_evaluation, egui_render_cell_read, serialized_value_to_json_value};
use chidori_core::execution::execution::execution_state::{CloseReason, EnclosedState};
use crate::graph::materials::RoundedRectMaterial;
use crate::vendored::tidy_tree::{TreeGraph, Node};

pub fn compute_egui_transform_matrix(
    mut q_egui_render_target: Query<(&mut EguiRenderTarget, &Transform), (With<EguiRenderTarget>, Without<Window>)>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    q_camera: Query<(&Projection, &Camera, &GlobalTransform), (Without<GraphMinimapCamera>,  With<OnGraphScreen>)>,
) {
    let (_, camera, camera_transform) = q_camera.single();
    let window = q_window.single();
    let scale_factor = window.scale_factor();
    let viewport_pos = if let Some(viewport) = &camera.viewport {
        Vec2::new(viewport.physical_position.x as f32 / scale_factor, viewport.physical_position.y as f32 / scale_factor)
    } else {
        Vec2::ZERO
    };
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    // Transform from the viewport offset into the world coordinates
    let Some(world_cursor_pos) = camera
        .viewport_to_world(camera_transform, cursor - viewport_pos)
        .map(|r| r.origin.truncate()) else {
        return;
    };

    for (mut egui_render_target , element_transform) in q_egui_render_target.iter_mut() {

        // Translate the element then revert the camera position relative to it
        let world_space_to_local_space = (
            // Mat4::from_translation(Vec3::new(0.0, (camera_transform.translation().y -element_transform.translation.y) * 2.0, 0.0))
                 Mat4::from_translation(vec3(element_transform.scale.x * -0.5, element_transform.scale.y * -0.5, 0.0))
                * Mat4::from_translation(element_transform.translation)
        ).inverse();

        let mut local_cursor_pos = world_space_to_local_space
            .transform_point3(world_cursor_pos.extend(0.0))
            .truncate();

        local_cursor_pos.y = element_transform.scale.y - local_cursor_pos.y;

        egui_render_target.cursor_position = Some(local_cursor_pos);
    }
}

pub fn egui_graph_node(
    current_theme: &CurrentTheme,
    chidori_state: &mut ChidoriState,
    node_resources_cache: &mut NodeResourcesCache,
    node: &&ExecutionNodeId,
    entity: Entity,
    ctx: &mut Context,
    transform: &Transform
) {
    egui::Area::new(format!("{:?}", entity).into())
        .fixed_pos(Pos2::new(0.0, 0.0)).show(ctx, |ui| {
        ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
        let mut frame = egui::Frame::default().fill(current_theme.theme.card).stroke(current_theme.theme.card_border)
            .inner_margin(16.0).rounding(6.0).begin(ui);
        {
            let mut ui = &mut frame.content_ui;
            ui.set_width(800.0 - (2.0 * 16.0));
            ui.set_max_height(1000.0);
            let node1 = *node;
            let original_style = (*ui.ctx().style()).clone();

            let mut style = original_style.clone();
            ui.set_style(style);

            if chidori_state.debug_mode {
                ui.label(format!("{:?}", transform));
            }

            if *node1 == chidori_core::uuid::Uuid::nil() {
                render_nil_node(ui, chidori_state, node1);
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if let Some(state) = chidori_state.get_execution_state_at_id(&node1) {
                        let state = &state;
                        if !matches!(state.evaluating_enclosed_state, EnclosedState::Open) {
                            render_revert_button(ui, chidori_state, node1);
                        }

                        render_execution_state_content(ui, chidori_state, node_resources_cache, state, node1, &current_theme.theme);
                    } else {
                        ui.label("No evaluation recorded");
                    }
                });
            }

            ui.set_style(original_style);
        }
        frame.end(ui);
    });
}

pub fn create_egui_texture_image(window: &Window, width: f32, height: f32) -> (f32, u32, u32, Image) {
    let scale_factor = window.scale_factor();
    let scaled_width = (width * scale_factor) as u32;
    let scaled_height = ((height * scale_factor) as u32);
    let size = Extent3d {
        width: scaled_width,
        height: scaled_height,
        depth_or_array_layers: 1,
    };
    let mut image = Image {
        texture_descriptor: TextureDescriptor {
            label: None,
            dimension: TextureDimension::D2,
            format: TextureFormat::Bgra8UnormSrgb,
            mip_level_count: 1,
            sample_count: 1,
            usage: TextureUsages::TEXTURE_BINDING
                | TextureUsages::COPY_DST
                | TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
            size
        },
        ..default()
    };
    image.resize(size);
    (scale_factor, scaled_width, scaled_height, image)
}

pub fn read_image(ui: &mut Ui, img_path: &String) -> TextureHandle {
    // Load the image
    let img = image::io::Reader::open(&img_path)
        .expect("Failed to open image")
        .decode()
        .expect("Failed to decode image");

    // Resize the image if necessary
    let resized_img = if img.width() > 512 || img.height() > 512 {
        let ratio = img.width() as f32 / img.height() as f32;
        let (new_width, new_height) = if ratio > 1.0 {
            (512, (512.0 / ratio) as u32)
        } else {
            ((512.0 * ratio) as u32, 512)
        };
        img.resize(new_width, new_height, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    // Convert the image to egui::ColorImage
    let size = [resized_img.width() as _, resized_img.height() as _];
    let image_buffer = resized_img.to_rgba8();
    let pixels = image_buffer.as_flat_samples();
    let color_image = egui::ColorImage::from_rgba_unmultiplied(
        size,
        pixels.as_slice(),
    );

    // Create the texture
    let texture = ui.ctx().load_texture(
        img_path,
        color_image,
        egui::TextureOptions::default()
    );
    texture
}

fn render_nil_node(
    ui: &mut Ui,
    chidori_state: &mut ChidoriState,
    node: &ExecutionNodeId,
) {
    ui.horizontal(|ui| {
        if chidori_state.debug_mode {
            ui.label(node.to_string());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if ui.button(RichText::new("Revert to this State").color(Color32::from_hex("#dddddd").unwrap())).clicked() {
                let _ = chidori_state.set_execution_id(*node);
            }
        });
    });
}

fn render_error_state(
    ui: &mut Ui,
    chidori_state: &mut ChidoriState,
    state: &chidori_core::execution::execution::ExecutionState,
    current_theme: &Theme,
) {
    let mut frame = egui::Frame::default().fill(current_theme.card).stroke(Stroke {
        width: 0.5,
        color: Color32::from_hex("#ff0000").unwrap(),
    }).inner_margin(16.0).rounding(6.0).begin(ui);
    {
        let mut ui = &mut frame.content_ui;
        ui.label("Error");
        egui_execution_state(
            ui,
            chidori_state,
            state, 
            current_theme
        );
    }
    frame.end(ui);
}

fn render_revert_button(
    ui: &mut Ui,
    chidori_state: &mut ChidoriState,
    node: &ExecutionNodeId,
) {
    ui.horizontal(|ui| {
        if chidori_state.debug_mode {
            ui.label(node.to_string());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if ui.button("Revert to this State").clicked() {
                info!("We would like to revert to {:?}", node);
                let _ = chidori_state.set_execution_id(*node);
            }
        });
    });
}

fn render_execution_state_content(
    ui: &mut Ui,
    chidori_state: &mut ChidoriState,
    node_resources_cache: &mut NodeResourcesCache,
    state: &chidori_core::execution::execution::ExecutionState,
    node: &ExecutionNodeId,
    current_theme: &Theme,
) {
    match &state.evaluating_enclosed_state {
        EnclosedState::Close(CloseReason::Error) => {
            render_error_state(ui, chidori_state, state, current_theme);
        }
        EnclosedState::Close(CloseReason::Failure) => {
            ui.label("Eval Failure");
        }
        EnclosedState::Open => {
            egui_execution_state(ui, chidori_state, state, current_theme);
        }
        EnclosedState::SelfContained | EnclosedState::Close(CloseReason::Complete) => {
            egui_execution_state(ui, chidori_state, state, current_theme);
            render_resource_images(ui, node_resources_cache, state, node);
        }
    }
}

fn render_resource_images(
    ui: &mut Ui,
    node_resources_cache: &mut NodeResourcesCache,
    state: &chidori_core::execution::execution::ExecutionState,
    node: &ExecutionNodeId,
) {
    let image_paths = node_resources_cache.matched_strings_in_resource.entry(*node).or_insert_with(|| {
        state.state.iter().map(|(_, value)| {
            if let Ok(output) = &value.output.clone() {
                crate::util::find_matching_strings(&output, r"(?i)\.(png|jpe?g)$")
            } else {
                vec![]
            }
        }).flatten().collect()
    });
    
    for (img_path, _) in image_paths {
        let texture = if let Some(cached_texture) = node_resources_cache.image_texture_cache.get(img_path) {
            cached_texture.clone()
        } else {
            let texture = read_image(ui, &img_path);
            node_resources_cache.image_texture_cache.insert(img_path.clone(), texture.clone());
            texture
        };

        // Display the image
        ui.add(egui::Image::new(&texture));
    }
}

fn egui_execution_state(
    ui: &mut Ui,
    internal_state: &mut ChidoriState,
    execution_state: &chidori_core::execution::execution::ExecutionState,
    current_theme: &Theme
) {
    ui.vertical(|ui| {
        ui.label("Evaluated:");
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.vertical(|ui| {
                if internal_state.debug_mode {
                    render_debug_info(ui, execution_state);
                }
                
                render_cell_name(ui, execution_state);
                egui_render_cell_function_evaluation(ui, execution_state);
                render_execution_output(ui, execution_state);
            })
        });

        render_cell_definition(ui, execution_state);

        if internal_state.debug_mode {
            render_debug_stack_and_args(ui, execution_state);
        }

        render_cell_mutation(ui, internal_state, execution_state, current_theme);
    });
}

fn render_debug_info(
    ui: &mut Ui,
    execution_state: &chidori_core::execution::execution::ExecutionState,
) {
    ui.label(format!("Chronology Id: {:?}", execution_state.chronology_id));
    ui.label(format!("Chronology Parent Id: {:?}", execution_state.parent_state_chronology_id));
    ui.label(format!("Resolving Execution Node Id: {:?}", execution_state.resolving_execution_node_state_id));
    ui.label(format!("Enclosed State: {:?}", execution_state.evaluating_enclosed_state));
    ui.label(format!("Function Name: {:?}", execution_state.evaluating_fn));
    ui.label(format!("Operation Id: {:?}", execution_state.evaluating_operation_id));
}

fn render_cell_name(
    ui: &mut Ui,
    execution_state: &chidori_core::execution::execution::ExecutionState,
) {
    if let Some(evaluating_name) = execution_state.evaluating_name.as_ref() {
        ui.label(format!("Cell Name: {:?}", evaluating_name));
    }
}

fn render_execution_output(
    ui: &mut Ui,
    execution_state: &chidori_core::execution::execution::ExecutionState,
) {
    if !execution_state.state.is_empty() {
        ui.label("Output:");
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            for (key, value) in execution_state.state.iter() {
                if execution_state.fresh_values.contains(key) {
                    match &value.output.clone() {
                        Ok(o) => {
                            let _ = JsonTree::new(format!("{:?}", key), &serialized_value_to_json_value(&o))
                                .show(ui);
                        }
                        Err(e) => {
                            ui.label(format!("{:?}", e));
                        }
                    }
                }
            }
        });
    }
}

fn render_cell_definition(
    ui: &mut Ui,
    execution_state: &chidori_core::execution::execution::ExecutionState,
) {
    if let Some(cell) = &execution_state.evaluating_cell() {
        egui::CollapsingHeader::new("Cell Definition")
            .show(ui, |ui| {
                egui_render_cell_read(ui, cell, execution_state);
            });
    }
}

fn render_debug_stack_and_args(
    ui: &mut Ui,
    execution_state: &chidori_core::execution::execution::ExecutionState,
) {
    if !execution_state.stack.is_empty() {
        ui.label("Exec Stack:");
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.vertical(|ui| {
                for item in &execution_state.stack {
                    ui.label(format!("{:?}", item));
                }
            })
        });
    }
    if let Some(args) = &execution_state.evaluating_arguments {
        ui.label("Evaluating With Arguments");
        let _ = JsonTree::new(format!("evaluating_args"), &serialized_value_to_json_value(&args))
            .show(ui);
    }
}

fn render_cell_mutation(
    ui: &mut Ui,
    internal_state: &mut ChidoriState,
    execution_state: &chidori_core::execution::execution::ExecutionState,
    current_theme: &Theme,
) {
    if let Some((op_id, _)) = &execution_state.evaluated_mutation_of_cell {
        ui.label("Cell Mutation:");
        ui.horizontal(|ui| {
            ui.add_space(10.0);
        });
        let mut code_theme = egui_extras::syntax_highlighting::CodeTheme::dark();
        crate::code::editable_chidori_cell_content(
            internal_state,
            &current_theme,
            ui,
            &mut code_theme,
            *op_id,
            true);
    }
}

fn process_graph_node_simple(
    commands: &mut Commands,
    graph_resource: &mut GraphResource,
    edge_pair_id_to_entity: &mut EdgePairIdToEntity,
    node_id_to_entity: &mut NodeIdToEntity,
    current_theme: &CurrentTheme,
    images: &mut Assets<Image>,
    materials_custom: &mut Assets<RoundedRectMaterial>,
    materials: &mut Assets<StandardMaterial>,
    meshes: &mut Assets<Mesh>,
    chidori_state: &mut ChidoriState,
    node_resources_cache: &mut NodeResourcesCache,
    window: &Window,
    idx: petgraph::graph::NodeIndex,
    node: &ExecutionNodeId,
    tree_graph: &TreeGraph,
    node_index_to_entity: &mut HashMap<usize, Entity>,
    node_query: &mut Query<
        (Entity, &mut Transform, &GraphIdx, &mut EguiContext, &mut EguiRenderTarget, &Handle<RoundedRectMaterial>),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    edge_query: &mut Query<
        (Entity, &mut Transform, &GraphIdxPair),
        (With<GraphIdxPair>, Without<GraphIdx>),
    >,
) -> bool {
    let mut parents = graph_resource
        .execution_graph
        .neighbors_directed(idx, petgraph::Direction::Incoming);

    // Get position of the node's parent
    let parent_pos = parents
        .next()
        .and_then(|parent| node_id_to_entity.mapping.get(&parent))
        .and_then(|entity| {
            if let Ok((_, mut transform, _, _, _, _)) = node_query.get_mut(*entity) {
                Some(transform.translation.truncate())
            } else {
                None
            }
        }).unwrap_or(vec2(0.0, 0.0));

    if let Some((n_idx, n)) = tree_graph.get_from_external_id(&idx.index()) {
        // Create the appropriately sized egui render target texture
        let width = n.width as f32;
        let height = n.height as f32;
        info!("Found node in tree graph: external_id={}, width={}, height={}", n.external_id, width, height);
        let entity = create_or_update_node_entity_simple(commands, graph_resource, node_id_to_entity, current_theme, images, materials_custom, meshes, window, idx, node, width, height, parent_pos);
        node_index_to_entity.insert(n.external_id, entity);
        info!("Created/updated entity {:?} for node {}", entity, node);

        // Check if dimensions have changed for existing entries
        update_node_dimensions_if_needed_simple(commands, images, materials_custom, window, entity, width, height, node_query);

        let (new_height, content_updated) = update_node_transform_and_content_simple(current_theme, chidori_state, node_resources_cache, entity, node, n, width, node_query);
        
        if content_updated {
            graph_resource.node_dimensions.insert(node.clone(), (800.0, new_height));
        }

        process_node_edges_simple(commands, edge_pair_id_to_entity, materials, meshes, tree_graph, n, *n_idx, node_index_to_entity, node_query, edge_query);
        
        content_updated
    } else {
        warn!("Node {} (index: {}) not found in tree graph", node, idx.index());
        false
    }
}

fn create_or_update_node_entity_simple(
    commands: &mut Commands,
    graph_resource: &mut GraphResource,
    node_id_to_entity: &mut NodeIdToEntity,
    current_theme: &CurrentTheme,
    images: &mut Assets<Image>,
    materials_custom: &mut Assets<RoundedRectMaterial>,
    meshes: &mut Assets<Mesh>,
    window: &Window,
    idx: petgraph::graph::NodeIndex,
    node: &ExecutionNodeId,
    width: f32,
    height: f32,
    parent_pos: Vec2,
) -> Entity {
    let entity = node_id_to_entity.mapping.entry(idx).or_insert_with(|| {
        info!("Creating new entity for node {} at position ({}, {})", node, parent_pos.x, parent_pos.y);
        // This is the texture that will be rendered to.
        let (scale_factor, scaled_width, scaled_height, image) = create_egui_texture_image(window, width, height);
        let image_handle = images.add(image);
        info!("Created image handle {:?} with dimensions {}x{}", image_handle, scaled_width, scaled_height);
        
        let node_material = materials_custom.add(RoundedRectMaterial {
            width: 1.0,
            height: 1.0,
            color_texture: Some(image_handle.clone()),
            base_color: Vec4::new(0.0, 0.0, 0.0, 1.0),
            alpha_mode: AlphaMode::Blend,
        });
        info!("Created material handle {:?}", node_material);

        let entity = commands.spawn((
            MaterialMeshBundle {
                mesh: meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))),
                material: node_material,
                transform: Transform::from_xyz(parent_pos.x, parent_pos.y, -1.0).with_scale(vec3(width, height, 1.0)),
                ..Default::default()
            },
            GraphIdx {
                loading: false,
                execution_id: *node,
                id: idx.index(),
                is_hovered: false,
                is_selected: false,
            },
            EguiRenderTarget {
                inner_physical_width: scaled_width,
                inner_physical_height: scaled_height,
                image: Some(image_handle),
                inner_scale_factor: scale_factor,
                ..default()
            },
            Sensor,
            Collider::cuboid(0.5, 0.5),
            RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW),
            OnGraphScreen
        ));
        info!("Spawned entity {:?} with GraphIdx id: {}", entity.id(), idx.index());
        entity.id()
    });

    *entity
}

fn update_node_dimensions_if_needed_simple(
    commands: &mut Commands,
    images: &mut Assets<Image>,
    materials_custom: &mut Assets<RoundedRectMaterial>,
    window: &Window,
    entity: Entity,
    width: f32,
    height: f32,
    node_query: &mut Query<
        (Entity, &mut Transform, &GraphIdx, &mut EguiContext, &mut EguiRenderTarget, &Handle<RoundedRectMaterial>),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
) -> bool {
    if let Ok((_, mut transform, _, _, mut egui_render_target, material_handle)) = node_query.get_mut(entity) {
        let current_image = egui_render_target.image.as_ref();
        let dimensions_changed = current_image.map_or(true, |image| {
            let texture = images.get(image).unwrap();
            texture.texture_descriptor.size.width != (width * window.scale_factor()) as u32 ||
                texture.texture_descriptor.size.height != (height * window.scale_factor()) as u32
        });

        if dimensions_changed {
            // Create new image with updated dimensions
            let (scale_factor, scaled_width, scaled_height, image) = create_egui_texture_image(window, width, height);
            let image_handle = images.add(image);

            // Create new EguiRenderTarget
            let new_egui_render_target = EguiRenderTarget {
                inner_physical_width: scaled_width,
                inner_physical_height: scaled_height,
                image: Some(image_handle.clone()),
                inner_scale_factor: scale_factor,
                ..default()
            };

            // Replace the old EguiRenderTarget with the new one
            commands.entity(entity).remove::<EguiRenderTarget>()
                .insert(new_egui_render_target);

            // Update material with new texture
            let mut material = materials_custom.get_mut(material_handle).unwrap();
            material.color_texture = Some(image_handle);

            transform.scale.x = scaled_width as f32;
            transform.scale.y = scaled_height as f32;
        }
        dimensions_changed
    } else {
        false
    }
}

fn update_node_transform_and_content_simple(
    current_theme: &CurrentTheme,
    chidori_state: &mut ChidoriState,
    node_resources_cache: &mut NodeResourcesCache,
    entity: Entity,
    node: &ExecutionNodeId,
    n: &Node,
    width: f32,
    node_query: &mut Query<
        (Entity, &mut Transform, &GraphIdx, &mut EguiContext, &mut EguiRenderTarget, &Handle<RoundedRectMaterial>),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
) -> (f32, bool) {
    if let Ok((entity, mut transform, _, mut egui_ctx, _, _)) = node_query.get_mut(entity) {
        let egui_ctx = egui_ctx.into_inner();
        let ctx_ref = egui_ctx.get_mut();

        // Position the node according to its tidytree layout
        let new_pos = Vec3::new(n.x as f32, -n.y as f32, -1.0);
        transform.translation = transform.translation.lerp(new_pos, 0.5);
        info!("Updated entity {:?} transform position to ({}, {}, {})", entity, transform.translation.x, transform.translation.y, transform.translation.z);

        // Draw text within these elements
        egui_graph_node(current_theme, chidori_state, node_resources_cache, &node, entity, ctx_ref, &transform);

        let used_rect = ctx_ref.used_rect();
        let height = used_rect.height().min(1000.0);
        transform.scale = vec3(width, height, 1.0);
        info!("Updated entity {:?} scale to ({}, {}, {})", entity, transform.scale.x, transform.scale.y, transform.scale.z);
        
        (height, true)
    } else {
        warn!("Failed to get node query for entity {:?}", entity);
        (0.0, false)
    }
}

fn process_node_edges_simple(
    commands: &mut Commands,
    edge_pair_id_to_entity: &mut EdgePairIdToEntity,
    materials: &mut Assets<StandardMaterial>,
    meshes: &mut Assets<Mesh>,
    tree_graph: &TreeGraph,
    n: &Node,
    n_idx: petgraph::graph::NodeIndex,
    node_index_to_entity: &HashMap<usize, Entity>,
    node_query: &mut Query<
        (Entity, &mut Transform, &GraphIdx, &mut EguiContext, &mut EguiRenderTarget, &Handle<RoundedRectMaterial>),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    edge_query: &mut Query<
        (Entity, &mut Transform, &GraphIdxPair),
        (With<GraphIdxPair>, Without<GraphIdx>),
    >,
) {
    tree_graph.get_children(n_idx).into_iter().for_each(|child| {
        let child = &tree_graph.graph[child];
        let parent_pos = if let Some(entity) = node_index_to_entity.get(&n.external_id) {
            if let Ok((_, mut transform, _, _, _, _)) = node_query.get(*entity) {
                transform.translation.truncate()
            } else {
                return;
            }
        } else {
            return;
        };
        let child_pos = if let Some(entity) = node_index_to_entity.get(&child.external_id) {
            if let Ok((_, mut transform, _, _, _, _)) = node_query.get(*entity) {
                transform.translation.truncate()
            } else {
                return;
            }
        } else {
            return;
        };

        create_or_update_edge_simple(commands, edge_pair_id_to_entity, materials, meshes, n.external_id, child.external_id, parent_pos, child_pos, edge_query);
    });
}

fn create_or_update_edge_simple(
    commands: &mut Commands,
    edge_pair_id_to_entity: &mut EdgePairIdToEntity,
    materials: &mut Assets<StandardMaterial>,
    meshes: &mut Assets<Mesh>,
    parent_id: usize,
    child_id: usize,
    parent_pos: Vec2,
    child_pos: Vec2,
    edge_query: &mut Query<
        (Entity, &mut Transform, &GraphIdxPair),
        (With<GraphIdxPair>, Without<GraphIdx>),
    >,
) {
    let midpoint = (parent_pos + child_pos) / 2.0;
    let distance = (parent_pos - child_pos).length();
    let angle = (child_pos.y - parent_pos.y).atan2(child_pos.x - parent_pos.x);

    let entity = edge_pair_id_to_entity.mapping.entry((parent_id, child_id)).or_insert_with(|| {
        let entity = commands.spawn((
            PbrBundle {
                mesh: meshes.add(Rectangle::new(1.0, 1.0)),
                transform: Transform::from_xyz(midpoint.x, midpoint.y, -50.0).with_scale(vec3(distance, 3.0, 1.0)).with_rotation(Quat::from_rotation_z(-angle)),
                material: materials.add(StandardMaterial {
                    base_color: Color::hex("#ffffff").unwrap().into(),
                    unlit: true,
                    ..default()
                }),
                ..default()
            },
            GraphIdxPair{
                source: parent_id,
                target: child_id,
            },
            RenderLayers::layer(RENDER_LAYER_GRAPH_VIEW),
            OnGraphScreen ));
        entity.id()
    });

    if let Ok((_, mut transform, _)) = edge_query.get_mut(*entity) {
        transform.translation = vec3(midpoint.x, midpoint.y, -50.0);
        transform.scale = vec3(distance, 3.0, 1.0);
        transform.rotation = Quat::from_rotation_z(angle);
    }
}

pub fn update_graph_system_renderer(
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut commands: Commands,
    mut graph_resource: ResMut<GraphResource>,
    mut edge_pair_id_to_entity: ResMut<EdgePairIdToEntity>,
    mut node_id_to_entity: ResMut<NodeIdToEntity>,
    current_theme: Res<CurrentTheme>,
    mut node_query: Query<
        (Entity, &mut Transform, &GraphIdx, &mut EguiContext, &mut EguiRenderTarget, &Handle<RoundedRectMaterial>),
        (With<GraphIdx>, Without<GraphIdxPair>),
    >,
    mut edge_query: Query<
        (Entity, &mut Transform, &GraphIdxPair),
        (With<GraphIdxPair>, Without<GraphIdx>),
    >,
    mut images: ResMut<Assets<Image>>,
    mut materials_custom: ResMut<Assets<RoundedRectMaterial>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut chidori_state: ResMut<ChidoriState>,
    mut node_index_to_entity: Local<HashMap<usize, Entity>>,
    mut node_resources_cache: Local<NodeResourcesCache>,
) {
    println!("=== GRAPH SYSTEM RENDERER CALLED ===");
    if !graph_resource.is_active {
        println!("Graph resource is not active, skipping rendering");
        return;
    }
    let window = q_window.single();
    println!("Graph system renderer starting...");

    let execution_graph = &graph_resource.execution_graph;
    println!("Execution graph has {} nodes", execution_graph.node_count());
    
    // Ensure we always have a tree graph - generate one if missing or dirty
    if graph_resource.is_layout_dirty || graph_resource.layout_graph.is_none() {
        println!("Layout is dirty or missing, regenerating tree layout");
        let tree_graph = generate_tree_layout(&execution_graph, &graph_resource.node_dimensions);
        graph_resource.layout_graph = Some(tree_graph);
        graph_resource.is_layout_dirty = false;
    }
    
    let mut flag_layout_is_dirtied = false;
    
    // Clone the tree_graph to avoid borrowing issues
    let tree_graph = graph_resource.layout_graph.as_ref().expect("Missing tree graph").clone();
    println!("Tree graph has {} nodes", tree_graph.graph.node_count());

    // Traverse the graph again, and render the elements of the graph based on their layout in the tidy_tree
    let mut topo = petgraph::visit::Topo::new(&graph_resource.execution_graph);
    let mut processed_nodes = 0;
    while let Some(idx) = topo.next(&graph_resource.execution_graph) {
        let node = graph_resource.execution_graph.node_weight(idx).cloned();
        if let Some(node) = node {
            println!("Processing node {} (index: {})", node, idx.index());
            let layout_updated = process_graph_node_simple(
                &mut commands,
                &mut graph_resource,
                &mut edge_pair_id_to_entity,
                &mut node_id_to_entity,
                &current_theme,
                &mut images,
                &mut materials_custom,
                &mut materials,
                &mut meshes,
                &mut chidori_state,
                &mut node_resources_cache,
                window,
                idx,
                &node,
                &tree_graph,
                &mut node_index_to_entity,
                &mut node_query,
                &mut edge_query,
            );
            
            if layout_updated {
                flag_layout_is_dirtied = true;
            }
            processed_nodes += 1;
        } else {
            println!("WARNING: Node at index {} has no weight", idx.index());
        }
    }
    println!("Processed {} nodes total", processed_nodes);

    // Log entity counts
    let total_entities = node_query.iter().count();
    let node_id_mappings = node_id_to_entity.mapping.len();
    let node_index_mappings = node_index_to_entity.len();
    println!("Total graph entities: {}, NodeId mappings: {}, NodeIndex mappings: {}", 
        total_entities, node_id_mappings, node_index_mappings);

    if flag_layout_is_dirtied {
        graph_resource.is_layout_dirty = true;
    }
} 