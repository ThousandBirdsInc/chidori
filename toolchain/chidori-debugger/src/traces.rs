//! A shader that renders a mesh multiple times in one draw call.

use std::cell::RefCell;
use std::cmp::min;
use std::collections::HashMap;
use bevy::input::touchpad::{TouchpadMagnify, TouchpadRotate};
use std::ops::Add;
use bevy::{
    core_pipeline::core_3d::Transparent3d,
    ecs::{
        query::QueryItem,
        system::{lifetimeless::*, SystemParamItem},
    },
    pbr::{
        MeshPipeline, MeshPipelineKey, RenderMeshInstances, SetMeshBindGroup, SetMeshViewBindGroup,
    },
    prelude::*,
    render::{
        extract_component::{ExtractComponent, ExtractComponentPlugin},
        mesh::{GpuBufferInfo, MeshVertexBufferLayout},
        render_asset::RenderAssets,
        render_phase::{
            AddRenderCommand, DrawFunctions, PhaseItem, RenderCommand, RenderCommandResult,
            RenderPhase, SetItemPipeline, TrackedRenderPass,
        },
        render_resource::*,
        renderer::RenderDevice,
        view::{ExtractedView, NoFrustumCulling},
        Render, RenderApp, RenderSet,
    },
};
use bevy::input::mouse::{MouseButtonInput, MouseMotion, MouseScrollUnit, MouseWheel};
use bevy::math::{vec2, vec3};
use bevy::render::camera::{ScalingMode, Viewport};
use bevy::render::camera::ScalingMode::FixedVertical;
use bevy::render::view::RenderLayers;
use bevy::sprite::{Anchor, MaterialMesh2dBundle};
use bevy::text::Text2dBounds;
use bevy::utils::petgraph::visit::Walker;
use bevy::window::{PrimaryWindow, WindowResized};
use bevy_cosmic_edit::{CosmicEditPlugin, CosmicFontConfig};
use bevy_rapier2d::geometry::{Collider, Sensor};
use bevy_rapier2d::pipeline::QueryFilter;
use bevy_rapier2d::plugin::RapierContext;
use bytemuck::{offset_of, Pod, Zeroable};
use petgraph::prelude::{DiGraphMap, EdgeRef, NodeIndex, StableDiGraph, StableGraph};
use chidori_core::utils::telemetry::TraceEvents;
use crate::chidori::ChidoriTraceEvents;
use crate::util::{change_active_editor_ui, deselect_editor_on_esc, despawn_screen, print_editor_text};


const PADDING: f32 = 20.0;
const SPAN_HEIGHT: f32 = 20.0;
const CAMERA_SPACE_WIDTH: f32 = 1000.0;

#[derive(Component)]
struct MinimapTraceViewport;

#[derive(Component)]
struct IdentifiedSpan {
    id: String,
    is_hovered: bool
}

#[derive(Resource)]
struct TraceSpaceViewport {
    x: f32,
    y: f32,
    horizontal_scale: f32, // scale of the view
    vertical_scale: f32,
    max_vertical_extent: f32
}

fn update_trace_space_to_minimap_camera_configuration(
    mut trace_space: ResMut<TraceSpaceViewport>,
    mut camera: Query<(&mut Projection, &mut Transform), (With<OnTraceScreen>, With<TraceCameraMinimap>)>,
) {
    let (projection, mut camera_transform) = camera.single_mut();
    let (mut scale) = match projection.into_inner() {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref mut o) => { (&mut o.scaling_mode) }
    };
    camera_transform.translation.y = -trace_space.max_vertical_extent / 2.0;
    *scale = ScalingMode::Fixed {
        width: CAMERA_SPACE_WIDTH,
        height: trace_space.max_vertical_extent,
    };
}

fn update_trace_space_to_camera_configuration(
    mut trace_space: ResMut<TraceSpaceViewport>,
    mut camera: Query<(&mut Projection, &mut Transform), (With<OnTraceScreen>, With<TraceCameraTraces>)>,
    mut minimap_trace_viewport: Query<(&mut Transform), (With<MinimapTraceViewport>, Without<TraceCameraTraces>)>,
) {
    let (projection, mut camera_transform) = camera.single_mut();
    let (mut scale) = match projection.into_inner() {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref mut o) => { (&mut o.scaling_mode) }
    };
    *scale = ScalingMode::Fixed {
        width: trace_space.horizontal_scale,
        height: trace_space.vertical_scale,
    };
    camera_transform.translation.x = trace_space.x;
    camera_transform.translation.y = trace_space.y;
    minimap_trace_viewport.iter_mut().for_each(|mut transform| {
        transform.translation.x = trace_space.x;
        transform.translation.y = trace_space.y;
        transform.scale.x = trace_space.horizontal_scale;
        transform.scale.y = trace_space.vertical_scale;
    });
}

fn update_handle_scroll_events(
    mut scroll_evr: EventReader<MouseWheel>,
    mut trace_space: ResMut<TraceSpaceViewport>,
    keyboard_input: Res<ButtonInput<KeyCode>>,
) {
    for ev in scroll_evr.read() {
        if keyboard_input.pressed(KeyCode::SuperLeft) {
            trace_space.horizontal_scale = (trace_space.horizontal_scale + ev.y).clamp(1.0, 100000.0);
        } else {
            trace_space.x -= ev.x;
            trace_space.y += ev.y;
        }
    }
}


#[derive(Component, Default)]
struct CursorWorldCoords(Vec2);




fn my_cursor_system(
    mut q_mycoords: Query<&mut CursorWorldCoords, With<OnTraceScreen>>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    q_camera: Query<(&Camera, &GlobalTransform, &Projection), (With<OnTraceScreen>, With<TraceCameraTraces>)>,
    mut trace_space: ResMut<TraceSpaceViewport>,
) {
    let mut coords = q_mycoords.single_mut();
    let (camera, camera_transform, projection) = q_camera.single();
    let ortho_projection = match projection {
        Projection::Perspective(_) => { unreachable!("This should be orthographic") }
        Projection::Orthographic(ref o) => { o }
    };
    let viewport_pos = if let Some(viewport) = &camera.viewport {
        vec2(viewport.physical_position.x as f32, viewport.physical_position.y as f32)
    } else {
        Vec2::ZERO
    };
    let window = q_window.single();
    let window_size = vec2(window.width() as f32, window.height() as f32);
    if let ScalingMode::Fixed { width: scale_width , height : scale_height} = ortho_projection.scaling_mode {
        if let Some(world_position) = window.cursor_position()
            .and_then(|cursor| {
                let adjusted_cursor = cursor - viewport_pos;
                camera.viewport_to_world(camera_transform, adjusted_cursor)
            })
            .map(|ray| ray.origin.truncate())
        {
            // Adjust according to the ratio of our actual window size and our scaling independently of it
            coords.0 = world_position * vec2((window_size.x / scale_width), 1.0);
        }
    }
}

fn mouse_pan(
    mut trace_space: ResMut<TraceSpaceViewport>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut motion_evr: EventReader<MouseMotion>,
) {
    if buttons.pressed(MouseButton::Left) {
        for ev in motion_evr.read() {
            trace_space.x -= (ev.delta.x * trace_space.horizontal_scale);
        }
    }
}


// these only work on macOS
fn touchpad_gestures(
    mut trace_space: ResMut<TraceSpaceViewport>,
    mut evr_touchpad_magnify: EventReader<TouchpadMagnify>,
    mut evr_touchpad_rotate: EventReader<TouchpadRotate>,
) {
    for ev_magnify in evr_touchpad_magnify.read() {
        trace_space.horizontal_scale = (trace_space.horizontal_scale + (ev_magnify.0 * trace_space.horizontal_scale)).clamp(1.0, 100000.0);
    }
}

fn mouse_over_system(
    q_mycoords: Query<&CursorWorldCoords, With<OnTraceScreen>>,
    mut node_query: Query<(Entity, &Collider, &mut IdentifiedSpan), With<IdentifiedSpan>>,
    mut gizmos: Gizmos,
    rapier_context: Res<RapierContext>,
) {
    let cursor = q_mycoords.single();

    for (_, collider, mut span) in node_query.iter_mut() {
        span.is_hovered = false;
    }

    gizmos
        .circle(Vec3::new(cursor.0.x, cursor.0.y, 0.0), Direction3d::Z, 8.0, Color::YELLOW)
        .segments(64);
    let point = Vec2::new(cursor.0.x, cursor.0.y);
    let filter = QueryFilter::default();
    rapier_context.intersections_with_point(
        point, filter, |entity| {
            if let Ok((_, _, mut span)) = node_query.get_mut(entity) {
                span.is_hovered = true;
            }
            false
        }
    );
}


#[derive(Debug, Clone)]
struct CallNode {
    id: String,
    depth: usize,
    start_weight: u128,
    total_duration: u128,
    event: TraceEvents,
}


fn build_call_tree(events: Vec<TraceEvents>) -> (u128, u128, StableGraph<CallNode, ()>) {
    let mut node_map: HashMap<String, NodeIndex> = HashMap::new();
    let mut graph = StableDiGraph::new();
    let mut endpoint = 0;
    let mut startpoint = u128::MAX;
    for event in events {
        match &event {
            e @ TraceEvents::NewSpan {
                id,
                parent_id,
                weight,
                ..
            } => {
                let node = CallNode {
                    id: id.clone(),
                    depth: 0,
                    start_weight: *weight,
                    total_duration: 0,
                    event: e.clone(),
                };
                if *weight < startpoint {
                    startpoint = *weight;
                }
                if *weight > endpoint {
                    // If the node has no duration, it's a leaf node
                    endpoint = *weight;
                }
                let node_id = graph.add_node(node);
                node_map.insert(id.clone(), node_id);
                if let Some(parent_id) = parent_id {
                    if let Some(parent) = node_map.get(parent_id) {
                        graph.add_edge(*parent, node_id, ());
                    }
                }
            }
            TraceEvents::Enter(id) => {
            }
            TraceEvents::Exit(id, weight) => {
                if let Some(node) = graph.node_weight_mut(node_map[id]) {
                    node.total_duration += weight - node.start_weight;
                    if node.total_duration + node.start_weight > endpoint {
                        // If the node has no duration, it's a leaf node
                        endpoint = node.total_duration + node.start_weight;
                    }
                }
            }
            TraceEvents::Close(id, weight) => {
                // If the node is at the top of the stack
            }
            TraceEvents::Record => {}
            TraceEvents::Event => {}
        }
    }

    // Set durations of anything incomplete to the current max time
    graph.node_weights_mut().for_each(|n| {
        if n.total_duration == 0 {
            n.total_duration = endpoint - n.start_weight;
        }
    });

    (startpoint, endpoint, graph)
}

fn scale_to_target(v: u128, max_value: u128, target_max: f32) -> f32 {
    let scale_factor = target_max / max_value as f32;
    v as f32 * scale_factor
}


#[derive(Resource)]
struct SpanToTextMapping {
    spans: HashMap<String, Entity>,
    identity: HashMap<String, Entity>,
}

fn update_positions(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut traces: ResMut<ChidoriTraceEvents>,
    mut query: Query<(&mut InstanceMaterialData,)>,
    mut span_to_text_mapping: ResMut<SpanToTextMapping>,
    mut q_text_elements: Query<(&Text, &mut Transform), With<Text>>,
    mut q_span_identities: Query<(&IdentifiedSpan, &mut Collider, &mut Transform), (With<IdentifiedSpan>, Without<Text>)>,
    trace_camera_query: Query<(&Camera, &Projection, &GlobalTransform), With<TraceCameraTraces>>,
    text_camera_query: Query<(&Camera, &GlobalTransform), With<TraceCameraTextAtlas>>,
    mut trace_space: ResMut<TraceSpaceViewport>,
) {
    let font = asset_server.load("fonts/CommitMono-1.143/CommitMono-700-Regular.otf");
    let text_style = TextStyle {
        font,
        font_size: 14.0,
        color: Color::WHITE,
    };

    let (trace_camera, trace_camera_projection, trace_camera_transform) = trace_camera_query.single();
    let (text_camera, text_camera_transform) = text_camera_query.single();

    let projection = match trace_camera_projection {
        Projection::Perspective(_) => {unreachable!("This should be orthographic")}
        Projection::Orthographic(o) => {o}
    };
    let camera_position = trace_camera_transform.translation();
    let viewport_width = projection.area.width();
    let left = (camera_position.x * trace_space.horizontal_scale) - viewport_width / 2.0;

    let trace_to_text = |point: Vec3| -> Vec3 {
        let pos = trace_camera.world_to_ndc(trace_camera_transform, point).unwrap_or(Vec3::ZERO);
        text_camera.ndc_to_world(text_camera_transform, pos).unwrap_or(Vec3::ZERO)
    };


    let (startpoint_value, endpoint_value, call_tree) = build_call_tree(traces.inner.clone());
    for (mut data,) in query.iter_mut() {
        let mut instances = data.0.iter_mut().collect::<Vec<_>>();
        let mut idx = 0;
        let root_nodes: Vec<NodeIndex> = call_tree.node_indices()
            .filter(|&node_idx| call_tree.edges_directed(node_idx, petgraph::Direction::Incoming).count() == 0)
            .collect();

        let mut max_vertical_extent : f32 = 0.0;
        for root_node in root_nodes {
            let mut node_depths: HashMap<NodeIndex, usize> = HashMap::new(); // Stack to hold nodes and their depths
            node_depths.insert(root_node, 0);
            petgraph::visit::Dfs::new(&call_tree, root_node)
                .iter(&call_tree)
                .for_each(|node_idx| {
                    let mut depth = 0;
                    call_tree.edges_directed(node_idx, petgraph::Direction::Incoming)
                        .for_each(|edge| {
                            let parent_depth = node_depths.get(&edge.source()).unwrap();
                            depth = parent_depth + 1;
                            node_depths.insert(node_idx, depth);
                        });
                    let node = call_tree.node_weight(node_idx).unwrap();
                    idx += 1;
                    let config_space_pos_x = scale_to_target(node.start_weight - startpoint_value, endpoint_value - startpoint_value, CAMERA_SPACE_WIDTH) - (CAMERA_SPACE_WIDTH / 2.0);
                    let config_space_width = scale_to_target(node.total_duration, endpoint_value - startpoint_value, CAMERA_SPACE_WIDTH);
                    let screen_space_pos_y = (idx as f32) * -1.0 * (SPAN_HEIGHT + 0.5);
                    max_vertical_extent = max_vertical_extent.max(screen_space_pos_y.abs());

                    let hue = (idx as f32 * 20.0) % 360.0; // Cycle through hues from 0 to 360 degrees
                    instances[idx].color = Color::Hsla {
                        hue,
                        saturation: 0.8, // Full saturation for vivid colors
                        lightness: 0.5,      // Maximum value for brightness
                        alpha: 1.0,      // Fully opaque
                    }.as_rgba_f32();
                    instances[idx].width = config_space_width;
                    instances[idx].position.x = config_space_pos_x + (config_space_width / 2.0);
                    instances[idx].position.y = screen_space_pos_y;
                    instances[idx].vertical_scale = SPAN_HEIGHT;


                    instances[idx].border_color = Color::Rgba {
                        red: 0.0,
                        green: 0.0,
                        blue: 0.0,
                        alpha: 1.0,      // Fully opaque
                    }.as_rgba_f32();
                    // Hovered state
                    if let Some(&entity) = span_to_text_mapping.identity.get(&node.id) {
                        if let Ok((span, mut collider, mut transform)) = q_span_identities.get_mut(entity) {
                            if span.is_hovered {
                                instances[idx].border_color = Color::Rgba {
                                    red: 1.0,
                                    green: 1.0,
                                    blue: 1.0,
                                    alpha: 1.0,      // Fully opaque
                                }.as_rgba_f32();
                            }
                        }
                    }

                    // let collision_pos = trace_to_text(vec3(config_space_pos_x + config_space_width / 2.0, screen_space_pos_y, 0.0));
                    // let collision_width = trace_to_text(vec3(config_space_width, screen_space_pos_y, 0.0));
                    // // Update or Create collision instances
                    // if let Some(&entity) = span_to_text_mapping.identity.get(&node.id) {
                    //     if let Ok((span, mut collider, mut transform)) = q_span_identities.get_mut(entity) {
                    //         transform.translation = collision_pos;
                    //         commands.entity(entity).remove::<Collider>();
                    //         commands.entity(entity).insert(Collider::cuboid(collision_width.x / 2.0, SPAN_HEIGHT / 2.0));
                    //     }
                    // } else {
                    //     let identity = commands.spawn((
                    //         IdentifiedSpan {
                    //             id: node.id.clone(),
                    //             is_hovered: false,
                    //         },
                    //         TransformBundle::from_transform(Transform::from_translation(collision_pos)),
                    //         Collider::cuboid(collision_width.x / 2.0, SPAN_HEIGHT / 2.0),
                    //         Sensor,
                    //         OnTraceScreen
                    //     )).id();
                    //     span_to_text_mapping.identity.insert(node.id.clone(), identity);
                    // }


                    let mut text_pos = trace_to_text(vec3(config_space_pos_x, screen_space_pos_y - 5.0, 0.0)) + vec3(3.0, 0.0, 0.0);

                    // Update or Create text instances
                    if let Some(&entity) = span_to_text_mapping.spans.get(&node.id) {
                        if let Ok(mut text_bundle) = q_text_elements.get_mut(entity) {
                            text_bundle.1.translation = text_pos;
                        }
                    } else {
                        let text = if let TraceEvents::NewSpan {name, location, line, ..} = &node.event {
                            Text::from_section(format!("{}: {} ({})", name, location, line), text_style.clone())
                        } else {
                            Text::from_section(format!("???"), text_style.clone())
                        };
                        let entity = commands.spawn((
                            Text2dBundle {
                                text,
                                text_anchor: Anchor::BottomLeft,
                                transform: Transform::from_translation(text_pos),
                                text_2d_bounds: Text2dBounds {
                                    size: Vec2::new(config_space_width, SPAN_HEIGHT / 2.0),
                                },
                                ..default()
                            },
                            IdentifiedSpan {
                                id: node.id.clone(),
                                is_hovered: false,
                            },
                            RenderLayers::layer(4),
                            OnTraceScreen
                        )).id();
                        span_to_text_mapping.spans.insert(node.id.clone(), entity);
                    }
                });
        }
        trace_space.max_vertical_extent = max_vertical_extent;
    }
}


fn set_camera_viewports(
    windows: Query<&Window>,
    mut resize_events: EventReader<WindowResized>,
    mut main_camera: Query<&mut Camera, (With<TraceCameraTraces>, Without<TraceCameraMinimap>, Without<TraceCameraTextAtlas>)>,
    mut text_camera: Query<&mut Camera, (With<TraceCameraTextAtlas>, Without<TraceCameraMinimap>, Without<TraceCameraTraces>)>,
    mut minimap_camera: Query<&mut Camera, (With<TraceCameraMinimap>, Without<TraceCameraTraces>, Without<TraceCameraTextAtlas>)>,
) {
    // We need to dynamically resize the camera's viewports whenever the window size changes
    // so then each camera always takes up half the screen.
    // A resize_event is sent when the window is first created, allowing us to reuse this system for initial setup.
    for resize_event in resize_events.read() {
        let window = windows.get(resize_event.window).unwrap();
        let mut main_camera = main_camera.single_mut();
        let mut text_camera = text_camera.single_mut();
        let mut minimap_camera = minimap_camera.single_mut();
        main_camera.viewport = Some(Viewport {
            physical_position: UVec2::new(0, 100),
            physical_size: UVec2::new(
                window.resolution.physical_width(),
                window.resolution.physical_height() - 100,
            ),
            ..default()
        });
        text_camera.viewport = main_camera.viewport.clone();
        minimap_camera.viewport = Some(Viewport {
            physical_position: UVec2::new(0, 0),
            physical_size: UVec2::new(
                window.resolution.physical_width(),
                100,
            ),
            ..default()
        });

    }
}



fn trace_setup(
    windows: Query<&Window>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut config_store: ResMut<GizmoConfigStore>,
) {
    let window = windows.single();
    let (config, _) = config_store.config_mut::<DefaultGizmoConfigGroup>();
    config.line_width = 1.0;
    config.render_layers = RenderLayers::layer(4);

    commands.spawn((
        meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))),
        SpatialBundle::INHERITED_IDENTITY,
        InstanceMaterialData(
            (1..=100)
                .flat_map(|x| 1..=100)
                .map(|_| InstanceData {
                    position: Vec3::new(-100000.0, -10000.0, -2.0),
                    width: 300.0,
                    vertical_scale: 300.0,
                    scale: 1.0,
                    border_color: Color::hsla(360., 1.0, 0.5, 1.0).as_rgba_f32(),
                    color: Color::hsla(360., 1.0, 0.5, 1.0).as_rgba_f32(),
                })
                .collect(),

        ),
        RenderLayers::layer(3),
        // NOTE: Frustum culling is done based on the Aabb of the Mesh and the GlobalTransform.
        // As the cube is at the origin, if its Aabb moves outside the view frustum, all the
        // instanced cubes will be culled.
        // The InstanceMaterialData contains the 'GlobalTransform' information for this custom
        // instancing, and that is not taken into account with the built-in frustum culling.
        // We must disable the built-in frustum culling by adding the `NoFrustumCulling` marker
        // component to avoid incorrect culling.
        NoFrustumCulling,
        OnTraceScreen,
    ));

    // Main trace view camera
    commands.spawn((
        Camera3dBundle {
            camera: Camera {
                order: 2,
                ..default()
            },
            transform: Transform::from_xyz(0.0, 0.0, 1.0).looking_at(Vec3::ZERO, Vec3::Y),
            projection: OrthographicProjection {
                scale: 1.0,
                scaling_mode: ScalingMode::Fixed {
                    width: CAMERA_SPACE_WIDTH,
                    height: (window.resolution.physical_height() - 100) as f32,
                },
                ..default()
            }.into(),
            ..default()
        },
        TraceCameraTraces,
        OnTraceScreen,
        RenderLayers::layer(3)
    ));

    // Text rendering camera
    commands.spawn((
        Camera2dBundle {
            camera: Camera {
                order: 4,
                ..default()
            },
            transform: Transform::from_xyz(0.0, 0.0, 0.0).looking_at(Vec3::ZERO, Vec3::Y),
            ..default()
        },
        OnTraceScreen,
        TraceCameraTextAtlas,
        RenderLayers::layer(4)
    ));

    // Minimap camera
    commands.spawn((
        Camera3dBundle {
            camera: Camera {
                order: 3,
                ..default()
            },
            transform: Transform::from_xyz(0.0, 0.0, 1.0)
                .looking_at(Vec3::ZERO, Vec3::Y)
                .with_translation(Vec3::new(0.0, -100.0, 0.0)),
            projection: OrthographicProjection {
                scale: 1.0,
                scaling_mode: ScalingMode::Fixed {
                    width: CAMERA_SPACE_WIDTH,
                    height: 100.0,
                },
                ..default()
            }.into(),
            ..default()
        },
        TraceCameraMinimap,
        OnTraceScreen,
        RenderLayers::from_layers(&[3, 5])
    ));

    commands.spawn((
        PbrBundle {
            mesh: meshes.add(Mesh::from(Rectangle::new(1.0, 1.0))).into(),
            material: materials.add(Color::hsla(3.0, 1.0, 1.0, 0.5)),
            transform: Transform::from_xyz(0.0, -50.0, -1.0),
            ..default()
        },
        RenderLayers::layer(5),
        MinimapTraceViewport,
        NoFrustumCulling,
        OnTraceScreen,
    ));

    // Minimap viewport indicator
    // commands.spawn((
    //     Camera2dBundle {
    //         camera: Camera {
    //             order: 5,
    //             ..default()
    //         },
    //         transform: Transform::from_xyz(0.0, 0.0, 1.0).looking_at(Vec3::ZERO, Vec3::Y),
    //         ..default()
    //     },
    //     OnTraceScreen,
    //     TraceCameraMinimapDraw,
    //     RenderLayers::layer(5)
    // ));


    commands.spawn((CursorWorldCoords(Vec2::ZERO), OnTraceScreen));

    commands.insert_resource(SpanToTextMapping {
        spans: Default::default(),
        identity: Default::default(),
    });

    commands.insert_resource(TraceSpaceViewport {
        x: 0.0,
        y: 0.0,
        horizontal_scale: CAMERA_SPACE_WIDTH,
        vertical_scale: (window.resolution.physical_height() - 100) as f32,
        max_vertical_extent: 100.0,
    });
    
}


#[derive(Component, Deref)]
struct InstanceMaterialData(Vec<InstanceData>);

impl ExtractComponent for InstanceMaterialData {
    type QueryData = &'static InstanceMaterialData;
    type QueryFilter = ();
    type Out = Self;

    fn extract_component(item: QueryItem<'_, Self::QueryData>) -> Option<Self> {
        Some(InstanceMaterialData(item.0.clone()))
    }
}

struct CustomMaterialPlugin;

impl Plugin for CustomMaterialPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ExtractComponentPlugin::<InstanceMaterialData>::default());
        app.sub_app_mut(RenderApp)
            .add_render_command::<Transparent3d, DrawCustom>()
            .init_resource::<SpecializedMeshPipelines<CustomPipeline>>()
            .add_systems(
                Render,
                (
                    queue_custom.in_set(RenderSet::QueueMeshes),
                    prepare_instance_buffers.in_set(RenderSet::PrepareResources),
                ),
            );
    }

    fn finish(&self, app: &mut App) {
        app.sub_app_mut(RenderApp).init_resource::<CustomPipeline>();
    }
}

#[derive(Default, Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct InstanceData {
    position: Vec3,
    color: [f32; 4],
    border_color: [f32; 4],
    width: f32,
    vertical_scale: f32,
    scale: f32,
}


#[allow(clippy::too_many_arguments)]
fn queue_custom(
    transparent_3d_draw_functions: Res<DrawFunctions<Transparent3d>>,
    custom_pipeline: Res<CustomPipeline>,
    msaa: Res<Msaa>,
    mut pipelines: ResMut<SpecializedMeshPipelines<CustomPipeline>>,
    pipeline_cache: Res<PipelineCache>,
    meshes: Res<RenderAssets<Mesh>>,
    render_mesh_instances: Res<RenderMeshInstances>,
    material_meshes: Query<Entity, With<InstanceMaterialData>>,
    mut views: Query<(&ExtractedView, &mut RenderPhase<Transparent3d>)>,
) {
    let draw_custom = transparent_3d_draw_functions.read().id::<DrawCustom>();

    let msaa_key = MeshPipelineKey::from_msaa_samples(msaa.samples());

    for (view, mut transparent_phase) in &mut views {
        let view_key = msaa_key | MeshPipelineKey::from_hdr(view.hdr);
        let rangefinder = view.rangefinder3d();
        for entity in &material_meshes {
            let Some(mesh_instance) = render_mesh_instances.get(&entity) else {
                continue;
            };
            let Some(mesh) = meshes.get(mesh_instance.mesh_asset_id) else {
                continue;
            };
            let key = view_key | MeshPipelineKey::from_primitive_topology(mesh.primitive_topology);
            let pipeline = pipelines
                .specialize(&pipeline_cache, &custom_pipeline, key, &mesh.layout)
                .unwrap();
            transparent_phase.add(Transparent3d {
                entity,
                pipeline,
                draw_function: draw_custom,
                distance: rangefinder
                    .distance_translation(&mesh_instance.transforms.transform.translation),
                batch_range: 0..1,
                dynamic_offset: None,
            });
        }
    }
}

#[derive(Component)]
struct InstanceBuffer {
    buffer: Buffer,
    length: usize,
}

fn prepare_instance_buffers(
    mut commands: Commands,
    query: Query<(Entity, &InstanceMaterialData)>,
    render_device: Res<RenderDevice>,
) {
    for (entity, instance_data) in &query {
        let buffer = render_device.create_buffer_with_data(&BufferInitDescriptor {
            label: Some("instance data buffer"),
            contents: bytemuck::cast_slice(instance_data.as_slice()),
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        });
        commands.entity(entity).insert(InstanceBuffer {
            buffer,
            length: instance_data.len(),
        });
    }
}

#[derive(Resource)]
struct CustomPipeline {
    shader: Handle<Shader>,
    mesh_pipeline: MeshPipeline,
}

impl FromWorld for CustomPipeline {
    fn from_world(world: &mut World) -> Self {
        let asset_server = world.resource::<AssetServer>();
        let shader = asset_server.load("shaders/instancing.wgsl");

        let mesh_pipeline = world.resource::<MeshPipeline>();

        CustomPipeline {
            shader,
            mesh_pipeline: mesh_pipeline.clone(),
        }
    }
}

impl SpecializedMeshPipeline for CustomPipeline {
    type Key = MeshPipelineKey;

    fn specialize(
        &self,
        key: Self::Key,
        layout: &MeshVertexBufferLayout,
    ) -> Result<RenderPipelineDescriptor, SpecializedMeshPipelineError> {
        let mut descriptor = self.mesh_pipeline.specialize(key, layout)?;

        descriptor.vertex.shader = self.shader.clone();
        descriptor.vertex.buffers.push(VertexBufferLayout {
            array_stride: std::mem::size_of::<InstanceData>() as u64,
            step_mode: VertexStepMode::Instance,
            attributes: vec![
                // position
                VertexAttribute {
                    format: VertexFormat::Float32x4,
                    offset: 0,
                    shader_location: 3, // shader locations 0-2 are taken up by Position, Normal and UV attributes
                },
                // color
                VertexAttribute {
                    format: VertexFormat::Float32x4,
                    offset: VertexFormat::Float32x4.size(),
                    shader_location: 4,
                },
                // border color
                VertexAttribute {
                    format: VertexFormat::Float32x4,
                    offset: VertexFormat::Float32x4.size() * 2,
                    shader_location: 5,
                },
                VertexAttribute {
                    format: VertexFormat::Float32,
                    offset: offset_of!(InstanceData, width) as u64,
                    shader_location: 6, // New location for horizontal scale
                },
                VertexAttribute {
                    format: VertexFormat::Float32,
                    offset: offset_of!(InstanceData, vertical_scale) as u64,
                    shader_location: 7, // New location for horizontal scale
                },
            ],
        });
        descriptor.fragment.as_mut().unwrap().shader = self.shader.clone();
        Ok(descriptor)
    }
}

type DrawCustom = (
    SetItemPipeline,
    SetMeshViewBindGroup<0>,
    SetMeshBindGroup<1>,
    DrawMeshInstanced,
);

struct DrawMeshInstanced;

impl<P: PhaseItem> RenderCommand<P> for DrawMeshInstanced {
    type Param = (SRes<RenderAssets<Mesh>>, SRes<RenderMeshInstances>);
    type ViewQuery = ();
    type ItemQuery = Read<InstanceBuffer>;

    #[inline]
    fn render<'w>(
        item: &P,
        _view: (),
        instance_buffer: Option<&'w InstanceBuffer>,
        (meshes, render_mesh_instances): SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let Some(mesh_instance) = render_mesh_instances.get(&item.entity()) else {
            return RenderCommandResult::Failure;
        };
        let Some(gpu_mesh) = meshes.into_inner().get(mesh_instance.mesh_asset_id) else {
            return RenderCommandResult::Failure;
        };
        let Some(instance_buffer) = instance_buffer else {
            return RenderCommandResult::Failure;
        };

        pass.set_vertex_buffer(0, gpu_mesh.vertex_buffer.slice(..));
        pass.set_vertex_buffer(1, instance_buffer.buffer.slice(..));

        match &gpu_mesh.buffer_info {
            GpuBufferInfo::Indexed {
                buffer,
                index_format,
                count,
            } => {
                pass.set_index_buffer(buffer.slice(..), 0, *index_format);
                pass.draw_indexed(0..*count, 0, 0..instance_buffer.length as u32);
            }
            GpuBufferInfo::NonIndexed => {
                pass.draw(0..gpu_mesh.vertex_count, 0..instance_buffer.length as u32);
            }
        }
        RenderCommandResult::Success
    }
}

#[derive(Component)]
struct TraceCameraTextAtlas;

#[derive(Component)]
struct TraceCameraTraces;

#[derive(Component)]
struct TraceCameraMinimap;

#[derive(Component)]
struct OnTraceScreen;

pub fn trace_plugin(app: &mut App) {
    app
        .add_plugins((CustomMaterialPlugin, ))
        .add_systems(OnEnter(crate::GameState::Traces), (trace_setup,))
        .add_systems(OnExit(crate::GameState::Traces), despawn_screen::<OnTraceScreen>)
        .add_systems(Update, (
            update_trace_space_to_minimap_camera_configuration,
            update_trace_space_to_camera_configuration,
            update_positions,
            update_handle_scroll_events,
            my_cursor_system,
            mouse_over_system,
            mouse_pan,
            touchpad_gestures,
            set_camera_viewports
        ).run_if(in_state(crate::GameState::Traces)));
}
