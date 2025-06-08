use bevy::prelude::*;
use std::path::PathBuf;

#[derive(Component)]
pub struct OnEditorScreen;

#[derive(Resource)]
pub struct EditorState {
    pub selected_file: Option<PathBuf>
}

impl Default for EditorState {
    fn default() -> Self {
        EditorState {
            selected_file: None
        }
    }
}

pub struct ViewingWatchedFileCells {
    pub is_showing_editor_cells: bool
}

impl Default for ViewingWatchedFileCells {
    fn default() -> Self {
        ViewingWatchedFileCells {
            is_showing_editor_cells: true
        }
    }
} 