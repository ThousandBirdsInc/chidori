use crate::code::state::EditorState;
use std::fs;
use std::path::Path;
use bevy_utils::tracing::debug;

pub fn file_browser(ui: &mut egui::Ui, path: &Path, editor_state: &mut EditorState) {
    let metadata = fs::metadata(path).unwrap();
    let file_name = path.file_name().unwrap().to_str().unwrap();
    let path_buf = path.to_path_buf();

    if metadata.is_dir() {
        let id = ui.make_persistent_id(path);
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true);

        state.show_header(ui, |ui| {
            ui.horizontal(|ui| {
                // Make folder icon non-selectable
                ui.add(egui::Label::new("📁")
                    .sense(egui::Sense::click())
                    .selectable(false));
                // Make folder name non-selectable
                ui.add(egui::Label::new(file_name).selectable(false));
            });
        })
            .body(|ui| {
                if let Ok(entries) = fs::read_dir(path) {
                    for entry in entries {
                        if let Ok(entry) = entry {
                            file_browser(ui, &entry.path(), editor_state);
                        }
                    }
                }
            });
    } else {
        let id = ui.make_persistent_id(path);
        ui.push_id(id, |ui| {
            // Create a non-interactive frame that prevents text selection
            let frame = egui::Frame::none()
                .inner_margin(egui::vec2(0.0, 0.0))
                .fill(if editor_state.selected_file.as_ref().map_or(false, |p| p == path) {
                    ui.style().visuals.selection.bg_fill
                } else {
                    egui::Color32::TRANSPARENT
                });

            let response = frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Make file icon non-selectable
                    ui.add(egui::Label::new("📄").selectable(false));
                    // Make filename non-selectable
                    ui.add(egui::Label::new(file_name).selectable(false));
                })
            })
                .response
                .interact(egui::Sense::click());

            // Handle click with the full row response
            if response.clicked() {
                debug!("Clicked select file");
                editor_state.selected_file = Some(path_buf);
            }
        });
    }
} 