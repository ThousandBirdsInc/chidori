use crate::bevy_egui::EguiContexts;
use crate::chidori::{EguiTree, EguiTreeIdentities};
use crate::util::despawn_screen;
use crate::GameState;
use bevy::app::{App, Update};
use bevy::input::ButtonInput;
use bevy::prelude::{in_state, Component, IntoSystemConfigs, KeyCode, Local, OnExit, Query, Res, ResMut, Resource, Window, With};
use bevy::window::PrimaryWindow;
use egui;
use egui::{Frame, Margin};
use egui_tiles::Tile;

#[derive(Component)]
struct OnChatScreen;

struct ChatMessage(String);

#[derive(Default, Resource)]
struct ChatHistory {
    messages: Vec<ChatMessage>,
}

fn chat_update(
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut tree: ResMut<EguiTree>,
    tree_identities: Res<EguiTreeIdentities>,
    mut contexts: EguiContexts,
    mut chat_history: ResMut<ChatHistory>,
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
    if let Some(chat_tile) = tree_identities.chat_tile {
        if let Some(tile) = tree.tree.tiles.get(chat_tile) {
            match tile {
                Tile::Pane(p) => {
                    if !tree.tree.active_tiles().contains(&chat_tile) {
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
            egui::ScrollArea::vertical().show(ui, |ui| {
                for message in &chat_history.messages {
                    ui.label(&message.0);
                }
            });

            ui.separator();

            ui.horizontal(|ui| {
                let mut text_edit = egui::TextEdit::singleline(&mut *input_text)
                    .hint_text("Type a message...")
                    .desired_width(f32::INFINITY);
                let response = ui.add(text_edit);
                if ui.button("Send").clicked() {
                    chat_history.messages.push(ChatMessage(input_text.clone()));
                    input_text.clear();
                }
                if response.changed() {
                    // …
                }
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    // …
                }
            });


            if keyboard_input.just_pressed(KeyCode::Enter) && !input_text.is_empty() {
                chat_history.messages.push(ChatMessage(input_text.clone()));
                input_text.clear();
            }
        }
        frame.end(ui);
    });
}


pub fn chat_plugin(app: &mut App) {
    app
        .init_resource::<ChatHistory>()
        .add_systems(OnExit(crate::GameState::Graph), despawn_screen::<OnChatScreen>)
        .add_systems(
            Update,
            (
                chat_update
            ).run_if(in_state(GameState::Graph)),
        );
}
