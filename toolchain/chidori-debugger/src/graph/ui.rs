//! UI components and interface elements for the graph system.
//! 
//! This file manages the user interface components that are displayed alongside
//! the graph visualization, including sidebars, panels, and other UI elements
//! that provide additional information and controls for interacting with the
//! execution graph and debugger state.

use crate::application::{ChidoriState, EguiTree, EguiTreeIdentities};
use crate::CurrentTheme;
use crate::bevy_egui::EguiContexts;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use egui::{Frame, Margin};
use egui_tiles::Tile;

pub fn ui_window(
    mut contexts: EguiContexts,
    tree_identities: Res<EguiTreeIdentities>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut chidori_state: ResMut<ChidoriState>,
    current_theme: Res<CurrentTheme>,
    mut tree: ResMut<EguiTree>,
) {
    let window = q_window.single();
    let mut hide_all = false;

    let sidebar_width = 275.0;
    let mut container_frame = Frame::default()
        .fill(current_theme.theme.card)
        .outer_margin(Margin {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        })
        .inner_margin(16.0);
    if let Some(graph_title) = tree_identities.graph_tile {
        if let Some(tile) = tree.tree.tiles.get(graph_title) {
            match tile {
                Tile::Pane(p) => {
                    if !tree.tree.active_tiles().contains(&graph_title) {
                        hide_all = true;
                    } else {
                        if let Some(r) = p.rect {
                            container_frame = container_frame.outer_margin(Margin {
                                left: r.min.x,
                                right: (window.width() - sidebar_width),
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

    // if window.width() > 600.0 {
    //     egui::CentralPanel::default().frame(container_frame).show(contexts.ctx_mut(), |ui| {
    //         ui.set_width(sidebar_width);
    //         ui.horizontal(|ui| {
    //             ui.add_space(16.0);
    //             ui.vertical(|ui| {
    //                 ui.button("Collapse Alternate Branches");
    //             });
    //         });
    //     });
    // }
} 