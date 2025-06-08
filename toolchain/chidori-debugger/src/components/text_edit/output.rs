use std::sync::Arc;

use crate::text::CursorRange;
use super::vim_mode::{VimMode, VimState};

/// The output from a [`TextEdit`](crate::TextEdit).
pub struct TextEditOutput {
    /// The interaction response.
    pub response: crate::Response,

    /// How the text was displayed.
    pub galley: Arc<crate::Galley>,

    /// Where the text in [`Self::galley`] ended up on the screen.
    pub galley_pos: crate::Pos2,

    /// The text was clipped to this rectangle when painted.
    pub text_clip_rect: crate::Rect,

    /// The state we stored after the run.
    pub state: super::TextEditState,

    /// Where the text cursor is.
    pub cursor_range: Option<CursorRange>,
}

impl TextEditOutput {
    #[deprecated = "Renamed `self.galley_pos`"]
    pub fn text_draw_pos(&self) -> crate::Pos2 {
        self.galley_pos
    }

    /// Get the current vim mode if vim mode is enabled
    pub fn vim_mode(&self) -> VimMode {
        self.state.vim_state.mode
    }

    /// Check if vim mode is enabled
    pub fn is_vim_mode_enabled(&self) -> bool {
        self.state.vim_state.is_normal_mode() || 
        self.state.vim_state.is_insert_mode() || 
        self.state.vim_state.is_operator_pending()
    }

    /// Get the vim mode status text for display
    pub fn vim_mode_status(&self) -> String {
        match self.state.vim_state.mode {
            VimMode::Normal => {
                if !self.state.vim_state.command_buffer.is_empty() {
                    format!("NORMAL {}", self.state.vim_state.command_buffer)
                } else {
                    "NORMAL".to_string()
                }
            }
            VimMode::Insert => "INSERT".to_string(),
            VimMode::Visual => "VISUAL".to_string(),
            VimMode::VisualLine => "VISUAL LINE".to_string(),
            VimMode::Command => format!(":{}", self.state.vim_state.command_line_input),
            VimMode::OperatorPending => {
                format!("OPERATOR PENDING {}", self.state.vim_state.command_buffer)
            }
        }
    }
}

// TODO(emilk): add `output.paint` and `output.store` and split out that code from `TextEdit::show`.
