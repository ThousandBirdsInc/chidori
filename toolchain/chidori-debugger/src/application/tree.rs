use bevy::input::ButtonInput;
use bevy::prelude::{KeyCode, Res, ResMut};
use egui_tiles::{Tile, TileId};

use super::types::{EguiTree, EguiTreeIdentities};

pub fn keyboard_shortcut_tab_focus(
    mut identities: ResMut<EguiTreeIdentities>,
    mut tree: ResMut<EguiTree>,
    button_input: Res<ButtonInput<KeyCode>>,
) {
    if button_input.pressed(KeyCode::SuperLeft) {
        if button_input.just_pressed(KeyCode::KeyT) {
            tree.tree.make_active(|id, _| {
                id == identities.traces_tile.unwrap()
            });
        }
        if button_input.just_pressed(KeyCode::KeyG) {
            tree.tree.make_active(|id, _| {
                id == identities.graph_tile.unwrap()
            });
        }
        if button_input.just_pressed(KeyCode::KeyC) {
            tree.tree.make_active(|id, _| {
                id == identities.code_tile.unwrap()
            });
        }
    }
}

pub fn maintain_egui_tree_identities(
    mut identities: ResMut<EguiTreeIdentities>,
    tree: ResMut<EguiTree>
) {
    tree.tree.tiles.iter().for_each(|(tile_id, tile)| {
        match tile {
            Tile::Pane(p) => {
                if &p.nr == &"Code" {
                    identities.code_tile = Some(tile_id.clone());
                }
                if &p.nr == &"Graph" {
                    identities.graph_tile = Some(tile_id.clone());
                }
                if &p.nr == &"Traces" {
                    identities.traces_tile = Some(tile_id.clone());
                }
            }
            _ => {}
        }
    })
} 