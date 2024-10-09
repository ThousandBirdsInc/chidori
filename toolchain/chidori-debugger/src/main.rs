#![feature(generic_nonzero)]
#![feature(inline_const)]
#![feature(associated_type_bounds)]
#![feature(pointer_is_aligned)]

mod util;
mod code;
mod traces;
mod graph;
mod chidori;
mod tokio_tasks;
mod tidy_tree;
mod chat;
mod logs;
mod shader_trace;
mod bevy_egui;
mod egui_json_tree;
mod tree_grouping;
mod json_editor;
// mod r#mod;
// use bevy_assets_bundler::BundledAssetIoPlugin;
// use r#mod::BUNDLE_OPTIONS;


use bevy::diagnostic::FrameTimeDiagnosticsPlugin;
use bevy::prelude::*;
use bevy::log::{Level, LogPlugin};
use bevy::render::render_resource::{Extent3d, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages};
use bevy::render::view::{Layer, RenderLayers};
use bevy::window::{PrimaryWindow, WindowMode, WindowResolution};
use bevy::winit::WinitWindows;
use bevy_cosmic_edit::*;
use crate::bevy_egui::{EguiPlugin, egui, EguiContexts};
use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, Rounding, Stroke};
use bevy_rapier2d::plugin::{NoUserData, RapierPhysicsPlugin};
use bevy_rapier2d::render::RapierDebugRenderPlugin;
use egui::style::{HandleShape, NumericColorSpace};
use once_cell::sync::{Lazy, OnceCell};




static DEVICE_SCALE: OnceCell<f32> = OnceCell::new();

pub const RENDER_LAYER_ROOT_CAMERA: Layer = 1;
pub const RENDER_LAYER_GRAPH_VIEW: Layer = 2;

pub const RENDER_LAYER_GRAPH_MINIMAP: Layer = 6;
pub const RENDER_LAYER_TRACE_VIEW: Layer = 3;
pub const RENDER_LAYER_TRACE_TEXT: Layer = 4;
pub const RENDER_LAYER_TRACE_MINIMAP: Layer = 5;


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
    if keyboard_input.pressed(KeyCode::SuperLeft) {
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
}



fn setup(
    mut commands: Commands,
    mut contexts: EguiContexts,
) {
    // commands.spawn((
    //     Camera2dBundle {
    //         camera: Camera {
    //             clear_color: ClearColorConfig::Custom(Color::BLACK),
    //             order: 8,
    //             ..default()
    //         },
    //         ..default()
    //
    //     },
    //     RenderLayers::layer(RENDER_LAYER_ROOT_CAMERA)));


    // style_egui_context(contexts);
}


#[derive(Debug, Clone, Copy, Hash)]
pub struct Theme {
    pub is_dark_mode: bool,
    pub background: Color32,
    pub foreground: Color32,
    pub card: Color32,
    pub card_border: Stroke,
    pub card_foreground: Color32,
    pub popover: Color32,
    pub popover_foreground: Color32,
    pub primary: Color32,
    pub primary_foreground: Color32,
    pub secondary: Color32,
    pub secondary_foreground: Color32,
    pub muted: Color32,
    pub muted_foreground: Color32,
    pub accent: Color32,
    pub accent_foreground: Color32,
    pub destructive: Color32,
    pub destructive_foreground: Color32,
    pub border: Color32,
    pub input: Color32,
    pub ring: Color32,
    pub chart_1: Color32,
    pub chart_2: Color32,
    pub chart_3: Color32,
    pub chart_4: Color32,
    pub chart_5: Color32,
    pub radius: usize,
}

impl Theme {
    pub fn new(is_dark_mode: bool) -> Self {
        if is_dark_mode {
            Theme {
                is_dark_mode: true,
                background: Color32::from_rgb(9, 9, 11),          // #09090B
                foreground: Color32::from_rgb(242, 242, 242),      // #f2f2f2
                card: Color32::from_rgb(28, 25, 23),               // #1C1917
                card_border: Stroke {
                    width: 0.5,
                    color: Color32::from_rgb(39, 39, 32),
                },
                card_foreground: Color32::from_rgb(242, 242, 242), // #f2f2f2
                popover: Color32::from_rgb(23, 23, 23),            // #171717
                popover_foreground: Color32::from_rgb(242, 242, 242), // #f2f2f2
                primary: Color32::from_rgb(225, 29, 72),           // #e11d48
                primary_foreground: Color32::from_rgb(255, 241, 242), // #fff1f2
                secondary: Color32::from_rgb(12, 10, 9),          // #0C0A09
                secondary_foreground: Color32::from_rgb(250, 250, 250), // #fafafa
                muted: Color32::from_rgb(38, 38, 38),              // #262626
                muted_foreground: Color32::from_rgb(161, 161, 170),   // #a1a1aa
                accent: Color32::from_rgb(42, 42, 42),             // #2a2a2a
                accent_foreground: Color32::from_rgb(250, 250, 250), // #fafafa
                destructive: Color32::from_rgb(220, 38, 38),        // #dc2626
                destructive_foreground: Color32::from_rgb(255, 241, 242), // #fff1f2
                border: Color32::from_rgb(38, 38, 38),             // #262626
                input: Color32::from_rgb(38, 38, 38),              // #262626
                ring: Color32::from_rgb(225, 29, 72),              // #e11d48
                chart_1: Color32::from_rgb(59, 130, 246),          // #3b82f6
                chart_2: Color32::from_rgb(16, 185, 129),          // #10b981
                chart_3: Color32::from_rgb(245, 158, 11),          // #f59e0b
                chart_4: Color32::from_rgb(139, 92, 246),          // #8b5cf6
                chart_5: Color32::from_rgb(236, 72, 153),          // #ec4899
                radius: 8
            }
        } else {
            Theme {
                is_dark_mode: false,
                background: Color32::from_rgb(255, 255, 255),      // #ffffff
                foreground: Color32::from_rgb(9, 9, 11),           // #09090b
                card: Color32::from_rgb(255, 255, 255),            // #ffffff
                card_border: Stroke {
                    width: 0.5,
                    color: Color32::from_rgb(39, 39, 32),
                },
                card_foreground: Color32::from_rgb(9, 9, 11),      // #09090b
                popover: Color32::from_rgb(255, 255, 255),         // #ffffff
                popover_foreground: Color32::from_rgb(9, 9, 11),   // #09090b
                primary: Color32::from_rgb(225, 29, 72),           // #e11d48
                primary_foreground: Color32::from_rgb(255, 241, 242), // #fff1f2
                secondary: Color32::from_rgb(243, 243, 245),       // #f3f3f5
                secondary_foreground: Color32::from_rgb(24, 24, 27), // #18181b
                muted: Color32::from_rgb(243, 243, 245),           // #f3f3f5
                muted_foreground: Color32::from_rgb(113, 113, 121),  // #717179
                accent: Color32::from_rgb(243, 243, 245),          // #f3f3f5
                accent_foreground: Color32::from_rgb(24, 24, 27),    // #18181b
                destructive: Color32::from_rgb(239, 68, 68),       // #ef4444
                destructive_foreground: Color32::from_rgb(250, 250, 250), // #fafafa
                border: Color32::from_rgb(228, 228, 231),          // #e4e4e7
                input: Color32::from_rgb(228, 228, 231),           // #e4e4e7
                ring: Color32::from_rgb(225, 29, 72),              // #e11d48
                chart_1: Color32::from_rgb(231, 110, 79),          // #e76e4f
                chart_2: Color32::from_rgb(42, 157, 144),          // #2a9d90
                chart_3: Color32::from_rgb(39, 71, 84),            // #274754
                chart_4: Color32::from_rgb(232, 196, 104),         // #e8c468
                chart_5: Color32::from_rgb(244, 164, 98),          // #f4a462
                radius: 8
            }
        }
    }

    fn make_widget_visual(
        &self,
        old: egui::style::WidgetVisuals,
        bg_fill: Color32,
    ) -> egui::style::WidgetVisuals {
        egui::style::WidgetVisuals {
            bg_fill,
            weak_bg_fill: bg_fill,
            bg_stroke: egui::Stroke {
                color: self.border,
                ..old.bg_stroke
            },
            fg_stroke: egui::Stroke {
                color: self.foreground,
                ..old.fg_stroke
            },
            ..old
        }
    }

    pub fn visuals(&self, old: egui::Visuals) -> egui::Visuals {
        egui::Visuals {
            override_text_color: Some(self.foreground),
            hyperlink_color: self.primary,
            faint_bg_color: self.secondary,
            extreme_bg_color: self.background,
            code_bg_color: self.card,
            warn_fg_color: self.destructive,
            error_fg_color: self.destructive,
            window_fill: self.background,
            panel_fill: self.card,
            window_stroke: egui::Stroke {
                color: self.border,
                ..old.window_stroke
            },
            widgets: egui::style::Widgets {
                noninteractive: self.make_widget_visual(old.widgets.noninteractive, self.background),
                inactive: self.make_widget_visual(old.widgets.inactive, self.secondary),
                hovered: self.make_widget_visual(old.widgets.hovered, self.muted),
                active: self.make_widget_visual(old.widgets.active, self.accent),
                open: self.make_widget_visual(old.widgets.open, self.secondary),
            },
            selection: egui::style::Selection {
                bg_fill: self.primary.linear_multiply(0.2),
                stroke: egui::Stroke {
                    color:  Color32::from_rgb(84, 221, 255),
                    ..old.selection.stroke
                },
            },
            window_shadow: egui::epaint::Shadow {
                color: self.background,
                ..old.window_shadow
            },
            popup_shadow: egui::epaint::Shadow {
                color: self.background,
                ..old.popup_shadow
            },
            dark_mode: self.is_dark_mode,
            ..old
        }
    }
}

pub fn set_theme(ctx: &egui::Context, is_dark_mode: bool) {
    let theme = Theme::new(is_dark_mode);
    let old = ctx.style().visuals.clone();
    ctx.set_visuals(theme.visuals(old));
}


#[derive(Resource)]
struct CurrentTheme {
    theme: Theme,
}

impl Default for CurrentTheme {
    fn default() -> Self {
        CurrentTheme {
            theme: Theme::new(true)
        }
    }
}


pub fn style_egui_context(ctx: &mut egui::Context ) {
    let mut fonts = FontDefinitions::default();

    set_theme(ctx, true);

    fonts.font_data.insert("CommitMono".to_owned(),
                           FontData::from_static(include_bytes!("../assets/fonts/CommitMono-1.143/CommitMono-400-Regular.otf"))); // .ttf and .otf supported
    fonts.font_data.insert("Inter".to_owned(),
                           FontData::from_static(include_bytes!("../assets/fonts/Inter/static/Inter-Regular.ttf"))); // .ttf and .otf supported
    fonts.families.get_mut(&FontFamily::Proportional).unwrap()
        .insert(0, "Inter".to_owned());
    fonts.families.get_mut(&FontFamily::Monospace).unwrap()
        .insert(0, "CommitMono".to_owned());


    egui_extras::install_image_loaders(ctx);

    ctx.set_fonts(fonts);

    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (bevy_egui::egui::TextStyle::Heading, FontId::new(24.0, bevy_egui::egui::FontFamily::Proportional)),
        (bevy_egui::egui::TextStyle::Body, FontId::new(14.0, bevy_egui::egui::FontFamily::Proportional)),
        (bevy_egui::egui::TextStyle::Monospace, FontId::new(14.0, bevy_egui::egui::FontFamily::Proportional)),
        (bevy_egui::egui::TextStyle::Button, FontId::new(14.0, bevy_egui::egui::FontFamily::Proportional)),
        (bevy_egui::egui::TextStyle::Small, FontId::new(12.0, bevy_egui::egui::FontFamily::Proportional)),
    ]
        .into();
    // style.visuals.widgets.hovered.bg_stroke = bevy_egui::egui::Stroke::new(1.0, Color32::from_hex("#333333").unwrap());
    style.spacing.button_padding = egui::vec2(8.0, 6.0);
    style.visuals.widgets.inactive.rounding = Rounding::same(4.0);
    style.visuals.widgets.active.rounding = Rounding::same(4.0);

    ctx.set_style(style);
}

fn set_window_size(
    mut windows: Query<(Entity, &mut Window), With<PrimaryWindow>>,
    winit_windows: NonSend<WinitWindows>,
) {
    let (e, mut window )= windows.single_mut();
    let scale_factor = window.scale_factor();

    if let Some(winit_window) = winit_windows.get_window(e) {
        winit_window.set_blur(true);
        if let Some(monitor) = winit_window.current_monitor() {
            let size = monitor.size();
            window.resolution.set(size.width as f32 / scale_factor, size.height as f32 / scale_factor);
        }
    }

    if let Some(monitor) = winit_windows.get_window(e).and_then(|w| w.current_monitor()) {
    }
}

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        // resolution: WindowResolution::new(500., 300.).with_scale_factor_override(2.0),
                        title: "Chidori Debugger".to_string(),
                        mode: WindowMode::Windowed,
                        position: WindowPosition::At(IVec2::ZERO),
                        resizable: true,
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
        // .add_plugins(bevy_framepace::FramepacePlugin)
        .add_plugins(EguiPlugin)
        .add_plugins(tokio_tasks::TokioTasksPlugin::default())
        // Insert as resource the initial value for the settings resources
        .insert_resource(CurrentTheme::default())
        // .insert_resource(Volume(7))
        // Declare the game state, whose starting value is determined by the `Default` trait
        .init_state::<GameState>()
        .add_systems(Startup, (setup, set_window_size))
        .add_systems(Update, check_key_input)
        // Adds the plugins for each state
        .add_plugins((chidori::chidori_plugin, code::editor_plugin, traces::trace_plugin, graph::graph_plugin, chat::chat_plugin, logs::logs_plugin))
        .run();
}