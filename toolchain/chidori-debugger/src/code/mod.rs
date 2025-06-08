//! Code editor module for the Chidori debugger.
//! 
//! This module provides a complete code editing interface for Chidori cells,
//! including file browsing, cell rendering, and cell creation functionality.

use std::cmp::Ordering;
use crate::bevy_egui::EguiContexts;
use crate::application::{CellState, ChidoriState, EguiTree, EguiTreeIdentities};
use crate::util::despawn_screen;
use crate::{CurrentTheme, GameState};
use bevy::app::{App, Update};
use bevy::prelude::{in_state, IntoSystemConfigs, Local, OnExit, Query, Res, ResMut, Window, With};
use bevy::window::PrimaryWindow;
use egui::{Frame, Margin, Vec2b};
use egui_tiles::Tile;
use std::path::Path;
use std::sync::Arc;

pub mod state;
pub mod file_browser;
pub mod cell_rendering;
pub mod new_cell;
pub mod utils;

pub use state::*;
pub use cell_rendering::editable_chidori_cell_content;

fn editor_update(
    mut contexts: EguiContexts,
    tree_identities: Res<EguiTreeIdentities>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut chidori_state: ResMut<ChidoriState>,
    current_theme: Res<CurrentTheme>,
    mut tree: ResMut<EguiTree>,
    mut editor_state: ResMut<EditorState>,
    mut viewing_watched_file_cells: Local<state::ViewingWatchedFileCells>,
) {
    let window = q_window.single();
    let mut hide_all = false;

    let mut container_frame = Frame::default().outer_margin(Margin {
        left: 0.0,
        right: 0.0,
        top: 0.0,
        bottom: 0.0,
    }).inner_margin(16.0);
    if let Some(code_tile) = tree_identities.code_tile {
        if let Some(tile) = tree.tree.tiles.get(code_tile) {
            match tile {
                Tile::Pane(p) => {
                    if !tree.tree.active_tiles().contains(&code_tile) {
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

    if hide_all || chidori_state.application_state_is_displaying_example_modal {
        return;
    }

    egui::CentralPanel::default().frame(container_frame).show(contexts.ctx_mut(), |ui| {
        let available_height = ui.available_height();
        ui.horizontal(|ui| {
            if window.width() > 600.0 {
                ui.vertical(|ui| {
                    ui.set_height(available_height);
                    let path = &chidori_state.watched_path.lock().unwrap().clone();
                    if path.is_some() {
                        ui.push_id("file_browser", |ui| {
                            egui::ScrollArea::vertical().auto_shrink(Vec2b::new(true, false)).show(ui, |ui| {
                                ui.set_width(200.0);
                                file_browser::file_browser(ui, Path::new(&path.as_ref().unwrap()), &mut editor_state);
                            });
                        });
                    }
                });
                ui.add_space(20.0);
            }
            ui.vertical(|ui| {
                ui.set_height(available_height);
                if let Some(selected_file) = &editor_state.selected_file.as_ref().and_then(|f| f.file_name()) {
                    let mut frame = egui::Frame::default()
                        .fill(current_theme.theme.card)
                        .stroke(current_theme.theme.card_border)
                        .outer_margin(Margin::symmetric(8.0, 16.0))
                        .inner_margin(16.0)
                        .rounding(current_theme.theme.radius as f32)
                        .begin(ui);
                    {
                        let mut ui = &mut frame.content_ui;
                        ui.set_width(875.0);
                        ui.label(format!("{}", selected_file.to_string_lossy()));
                    }
                    frame.end(ui);
                }
                ui.push_id("notebook", |ui| {
                    egui::ScrollArea::vertical().min_scrolled_width(f32::INFINITY).auto_shrink(Vec2b::new(true, false)).show(ui, |ui| {
                        let mut theme = egui_extras::syntax_highlighting::CodeTheme::dark();
                        ui.set_width(875.0);

                        let ids : Vec<_> = {
                            let mut cells_ids: Vec<_> = chidori_state.local_cell_state
                                .iter()
                                .filter(|x| {
                                    let Some(selected_file) = editor_state.selected_file.as_ref() else { return true };
                                    let Ok(cell) = x.value().lock() else { return false };
                                    let Some(cell) = cell.cell.as_ref() else { return false };
                                    let Some(backing_file) = cell.cell.backing_file_reference().as_ref() else { return false };
                                    backing_file.path == selected_file.to_string_lossy()
                                })
                                .collect();
                            cells_ids.sort_by(|a, b| {
                                let a_cell = a.value().lock().unwrap();
                                let b_cell = b.value().lock().unwrap();
                                let Some(a_cell) = a_cell.cell.as_ref() else {
                                    return Ordering::Less;
                                };
                                let Some( b_cell) = b_cell.cell.as_ref() else {
                                    return Ordering::Less;
                                };
                                a_cell.cmp(b_cell)
                            });
                            cells_ids.iter().map(|x| x.key().clone()).collect()
                        };
                        for cell_id in &ids {
                            cell_rendering::editable_chidori_cell_content(&mut chidori_state, &current_theme.theme, ui, &mut theme, *cell_id, false);
                        }

                        if !chidori_state.application_state_is_displaying_example_modal {
                            let mut frame = egui::Frame::default()
                                .fill(current_theme.theme.card)
                                .stroke(current_theme.theme.card_border)
                                .outer_margin(Margin::symmetric(8.0, 16.0))
                                .inner_margin(16.0)
                                .rounding(current_theme.theme.radius as f32)
                                .begin(ui);
                            {
                                let mut ui = &mut frame.content_ui;
                                let state_binding = chidori_state.local_cell_state.entry(chidori_core::uuid::Uuid::nil()).or_insert(Arc::new(std::sync::Mutex::new(CellState::default()))).clone();
                                let mut state = state_binding.lock();
                                let mut state = state.as_mut().unwrap();
                                new_cell::render_new_cell_interface(
                                    ui,
                                    &mut chidori_state,
                                    state,
                                    &mut theme
                                );
                            }
                            frame.end(ui);
                        }
                    });
                });
            });
        });
    });
}

pub fn editor_plugin(app: &mut App) {
    app
        .init_resource::<EditorState>()
        .add_systems(OnExit(GameState::Graph), despawn_screen::<OnEditorScreen>)
        .add_systems(
            Update,
            (
                editor_update
            ).run_if(in_state(GameState::Graph)),
        );
} 