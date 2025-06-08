use bevy::app::{App, Startup, Update};

use super::{setup, tree, ui, types};

pub fn chidori_plugin(app: &mut App) {
    app.init_resource::<types::EguiTree>()
        .init_resource::<types::EguiTreeIdentities>()
        .add_systems(Update, (
            ui::handle_menu_actions,
            ui::root_gui,
            ui::initial_save_notebook_dialog,
            tree::maintain_egui_tree_identities,
            tree::keyboard_shortcut_tab_focus
        ))
        .add_systems(Startup, setup::setup);
} 