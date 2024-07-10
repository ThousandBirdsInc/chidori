use bevy::app::{App, Startup, Update};
use bevy::DefaultPlugins;
use bevy::input::ButtonInput;
use bevy::prelude::{ButtonBundle, Camera, Camera2dBundle, ClearColorConfig, Color, Commands, Component, default, in_state, IntoSystemConfigs, KeyCode, Local, OnEnter, OnExit, Query, Res, ResMut, Resource, Style, Val, Window, With};
use bevy::window::PrimaryWindow;
use bevy_cosmic_edit::{Attrs, CosmicBuffer, CosmicColor, CosmicEditBundle, CosmicEditPlugin, CosmicFontConfig, CosmicFontSystem, CosmicPrimaryCamera, CosmicSource, Family, FocusedWidget, Metrics};
use crate::bevy_egui::{EguiContexts};
use egui;
use egui::{FontFamily, Frame, Margin};
use egui_tiles::Tile;
use chidori_core::cells::{CellTypes, CodeCell, LLMCodeGenCell, LLMEmbeddingCell, LLMPromptCell, MemoryCell, TemplateCell, TextRange, WebserviceCell};
use crate::chidori::{ChidoriCells, ChidoriLogMessages, EguiTree, EguiTreeIdentities};
use crate::GameState;
use crate::util::{change_active_editor_ui, deselect_editor_on_esc, despawn_screen, print_editor_text};
use std::borrow::BorrowMut;

#[derive(Component)]
struct OnLogsScreen;

struct LogsMessage(String);

#[derive(Default, Resource)]
struct LogsHistory {
    messages: Vec<LogsMessage>,
}

fn logs_update(
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut tree: ResMut<EguiTree>,
    tree_identities: Res<EguiTreeIdentities>,
    mut contexts: EguiContexts,
    mut logs_history: ResMut<LogsHistory>,
    mut log_messages: ResMut<ChidoriLogMessages>,
    mut input_text: Local<String>,
    keyboard_input: Res<ButtonInput<KeyCode>>,
) {
    let window = q_window.single();
    let mut hide_all = false;
    let mut container_frame = Frame::default().outer_margin(Margin {
        left: 0.0,
        right: 0.0,
        top: 0.0,
        bottom: 0.0,
    });
    if let Some(logs_tile) = tree_identities.logs_tile {
        if let Some(tile) = tree.tree.tiles.get(logs_tile) {
            match tile {
                Tile::Pane(p) => {
                    if !tree.tree.active_tiles().contains(&logs_tile) {
                        hide_all = true;
                    } else {
                        if let Some(r) = p.rect {
                            container_frame = container_frame.outer_margin(Margin {
                                left: r.min.x,
                                right: window.width() - r.max.x,
                                top: r.min.y,
                                bottom: window.height() - r.max.y,
                            });
                        }
                    }
                }
                Tile::Container(_) => {}
            }
        }
    }
    container_frame = container_frame.inner_margin(Margin::symmetric(10.0, 10.0));

    if hide_all {
        return;
    }

    let ctx = contexts.ctx_mut();

    egui::CentralPanel::default().frame(container_frame).show(ctx, |ui| {
        let mut frame = egui::Frame::default().inner_margin(Margin::symmetric(20.0, 20.0)).begin(ui);
        {
            ui.vertical(|ui| {
                ui.label("Search:");
                let mut text_edit = egui::TextEdit::singleline(&mut *input_text)
                    .hint_text("Type a message...");
                let response = ui.add(text_edit);
                if ui.button("Send").clicked() {
                    logs_history.messages.push(LogsMessage(input_text.clone()));
                    input_text.clear();
                }
                if response.changed() {
                    // …
                }
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    // …
                }
            });


            // if keyboard_input.just_pressed(KeyCode::Enter) && !input_text.is_empty() {
            //     logs_history.messages.push(LogsMessage(input_text.clone()));
            //     input_text.clear();
            // }
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                for message in &log_messages.inner {
                    let formatted_message = message.replace("\\n", "\n");
                    ui.label(formatted_message);
                    ui.add_space(5.0);
                }
            });
        }
        frame.end(ui);
    });
}


pub fn logs_plugin(app: &mut App) {
    app
        .init_resource::<LogsHistory>()
        .add_systems(OnExit(crate::GameState::Graph), despawn_screen::<OnLogsScreen>)
        .add_systems(
            Update,
            (
                logs_update
            ).run_if(in_state(GameState::Graph)),
        );
}