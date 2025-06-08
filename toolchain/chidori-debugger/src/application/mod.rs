//! Application module for the Chidori debugger.
//! 
//! This module contains the main application logic, state management, UI components,
//! and plugin registration for the Chidori debugger. It's organized into several
//! sub-modules for better maintainability and separation of concerns.

pub mod types;
pub mod state;
pub mod tree;
pub mod ui;
pub mod setup;
pub mod plugin;
pub mod tests;

// Re-export commonly used types and functions
pub use types::{
    ChidoriState, 
    CellState, 
    EguiTree, 
    EguiTreeIdentities, 
    Pane, 
    TreeBehavior
};

pub use plugin::chidori_plugin;

// Re-export key functions for external use
pub use setup::setup;
pub use tree::{keyboard_shortcut_tab_focus, maintain_egui_tree_identities};
pub use ui::{handle_menu_actions, root_gui, initial_save_notebook_dialog}; 