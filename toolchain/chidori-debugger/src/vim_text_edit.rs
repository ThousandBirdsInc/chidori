use egui::{Response, Ui, Widget, InputState};
use egui::text_edit::TextEdit;

pub struct VimTextEdit {
    text: String,
    cursor: usize,
    mode: VimMode,
}

enum VimMode {
    Normal,
    Insert,
}

impl VimTextEdit {
    pub fn new(text: String) -> Self {
        Self {
            text,
            cursor: 0,
            mode: VimMode::Normal,
        }
    }

    fn handle_normal_mode(&mut self, input: &InputState) {
        if input.key_pressed(egui::Key::I) {
            self.mode = VimMode::Insert;
        } else if input.key_pressed(egui::Key::H) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if input.key_pressed(egui::Key::L) {
            self.cursor = (self.cursor + 1).min(self.text.len());
        } else if input.key_pressed(egui::Key::J) {
            // Move down a line (simplified)
            if let Some(pos) = self.text[self.cursor..].find('\n') {
                self.cursor += pos + 1;
            }
        } else if input.key_pressed(egui::Key::K) {
            // Move up a line (simplified)
            if let Some(pos) = self.text[..self.cursor].rfind('\n') {
                self.cursor = pos + 1;
            }
        }
    }

    fn handle_insert_mode(&mut self, input: &InputState) {
        if input.key_pressed(egui::Key::Escape) {
            self.mode = VimMode::Normal;
        }
    }
}

impl Widget for &mut VimTextEdit {
    fn ui(self, ui: &mut Ui) -> Response {
        let mut layouter = |ui: &Ui, string: &str, wrap_width: f32| {
            let mut layout_job = egui::text::LayoutJob::default();
            layout_job.append(string, 0.0, egui::TextFormat::default());
            ui.fonts(|f| f.layout_job(layout_job))
        };

        let response = TextEdit::multiline(&mut self.text)
            .desired_width(f32::INFINITY)
            .layouter(&mut layouter)
            .show(ui);

        ui.input(|input| {
            match self.mode {
                VimMode::Normal => self.handle_normal_mode(input),
                VimMode::Insert => self.handle_insert_mode(input),
            }
        });

        response.response
    }
}