#![feature(generic_nonzero)]

mod util;
mod code;
mod traces;
mod graph;
mod chidori;
mod tokio_tasks;
mod tidy_tree;
mod chat;
mod shader_trace;

use bevy::core_pipeline::fxaa::FxaaPlugin;
use bevy::diagnostic::FrameTimeDiagnosticsPlugin;
use bevy::ecs::schedule::LogLevel;
use bevy::prelude::*;
use bevy::input::keyboard::KeyboardInput;
use bevy::log::{Level, LogPlugin};
use bevy::render::render_resource::{Extent3d, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages};
use bevy::render::view::RenderLayers;
use bevy::window::{PrimaryWindow, WindowResolution};
use bevy_cosmic_edit::*;
use bevy_egui::{EguiPlugin, egui, EguiContexts};
use bevy_egui::egui::{Color32, FontData, FontDefinitions, FontFamily, FontId};
use bevy_rapier2d::plugin::{NoUserData, RapierPhysicsPlugin};
use bevy_rapier2d::prelude::RapierDebugRenderPlugin;
use once_cell::sync::{Lazy, OnceCell};
use tokio::runtime::Runtime;
use chidori_core::sdk::entry::Chidori;
use util::{change_active_editor_ui, deselect_editor_on_esc, print_editor_text};


static DEVICE_SCALE: OnceCell<f32> = OnceCell::new();



#[derive(Clone, Copy, Default, Eq, PartialEq, Debug, Hash, States)]
enum GameState {
    Editor,
    Traces,
    Chat,
    #[default]
    Graph,
}

// System to check if the 'E' key is pressed and change state
fn check_key_input(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<NextState<GameState>>,
) {
    if keyboard_input.just_pressed(KeyCode::KeyE) {
        state.set(GameState::Editor);
    }
    if keyboard_input.just_pressed(KeyCode::KeyT) {
        state.set(GameState::Traces);
    }
    if keyboard_input.just_pressed(KeyCode::KeyC) {
        state.set(GameState::Chat);
    }
    if keyboard_input.just_pressed(KeyCode::KeyG) {
        state.set(GameState::Graph);
    }
}



fn setup(
    mut commands: Commands,
    mut contexts: EguiContexts,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {

    let size = Extent3d {
        width: 512,
        height: 512,
        ..default()
    };

    // This is the texture that will be rendered to.
    let mut image = Image {
        texture_descriptor: TextureDescriptor {
            label: None,
            size,
            dimension: TextureDimension::D2,
            format: TextureFormat::Bgra8UnormSrgb,
            mip_level_count: 1,
            sample_count: 1,
            usage: TextureUsages::TEXTURE_BINDING
                | TextureUsages::COPY_DST
                | TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        },
        ..default()
    };

    let first_pass_layer = RenderLayers::layer(1);
    // fill image.data with zeroes
    image.resize(size);

    let image_handle = images.add(image);

    let camera_bundle = Camera2dBundle {
        camera: Camera {
            // target: image_handle.clone().into(),
            clear_color: ClearColorConfig::Custom(Color::BLACK),
            order: 1,
            ..default()
        },
        ..default()

    };
    commands.spawn((camera_bundle, CosmicPrimaryCamera, RenderLayers::layer(1)));

    // This material has the texture that has been rendered.
    let material_handle = materials.add(StandardMaterial {
        base_color_texture: Some(image_handle),
        reflectance: 0.02,
        unlit: false,
        ..default()
    });


    let mut fonts = FontDefinitions::default();

    fonts.font_data.insert("my_font".to_owned(),
                           FontData::from_static(include_bytes!("../assets/fonts/CommitMono-1.143/CommitMono-400-Regular.otf"))); // .ttf and .otf supported
    fonts.families.get_mut(&FontFamily::Proportional).unwrap()
        .insert(0, "my_font".to_owned());
    fonts.families.get_mut(&FontFamily::Monospace).unwrap()
        .insert(0, "my_font".to_owned());

    contexts.ctx_mut().set_fonts(fonts);

    let mut style = (*contexts.ctx_mut().style()).clone();
    style.text_styles = [
        (bevy_egui::egui::TextStyle::Heading, FontId::new(24.0, bevy_egui::egui::FontFamily::Proportional)),
        (bevy_egui::egui::TextStyle::Body, FontId::new(14.0, bevy_egui::egui::FontFamily::Proportional)),
        (bevy_egui::egui::TextStyle::Monospace, FontId::new(14.0, bevy_egui::egui::FontFamily::Proportional)),
        (bevy_egui::egui::TextStyle::Button, FontId::new(14.0, bevy_egui::egui::FontFamily::Proportional)),
        (bevy_egui::egui::TextStyle::Small, FontId::new(12.0, bevy_egui::egui::FontFamily::Proportional)),
    ]
        .into();
    style.visuals.widgets.hovered.bg_stroke = bevy_egui::egui::Stroke::new(1.0, Color32::from_hex("#333333").unwrap());

    style.spacing.button_padding = egui::vec2(8.0, 6.0);
    contexts.ctx_mut().set_style(style);
}

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        // resolution: WindowResolution::new(500., 300.).with_scale_factor_override(2.0),
                        title: "Chidori Debugger".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                // We disable the log plugin to avoid conflicts with the Chidori logger
                .disable::<LogPlugin>()
            ,
            FrameTimeDiagnosticsPlugin,
        ))
        .add_plugins(RapierPhysicsPlugin::<NoUserData>::pixels_per_meter(100.0))
        // .add_plugins(RapierDebugRenderPlugin::default())
        .add_plugins(EguiPlugin)
        .add_plugins(tokio_tasks::TokioTasksPlugin::default())
        // Insert as resource the initial value for the settings resources
        // .insert_resource(DisplayQuality::Medium)
        // .insert_resource(Volume(7))
        // Declare the game state, whose starting value is determined by the `Default` trait
        .init_state::<GameState>()
        .add_systems(Startup, setup)
        .add_systems(Update, check_key_input)
        // Adds the plugins for each state
        .add_plugins((chidori::chidori_plugin, code::editor_plugin, traces::trace_plugin, graph::graph_plugin))
        .run();
}