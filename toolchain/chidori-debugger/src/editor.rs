use bevy::app::{App, Startup, Update};
use bevy::DefaultPlugins;
use bevy::prelude::{ButtonBundle, Camera, Camera2dBundle, ClearColorConfig, Color, Commands, Component, default, in_state, IntoSystemConfigs, OnEnter, OnExit, ResMut, Style, Val};
use bevy_cosmic_edit::{Attrs, CosmicBuffer, CosmicColor, CosmicEditBundle, CosmicEditPlugin, CosmicFontConfig, CosmicFontSystem, CosmicPrimaryCamera, CosmicSource, Family, FocusedWidget, Metrics};
use bevy_egui::{egui, EguiContexts};
use bevy_egui::egui::FontFamily;
use chidori_core::cells::{CellTypes, CodeCell, LLMCodeGenCell, LLMEmbeddingCell, LLMPromptCell, MemoryCell, TemplateCell, TextRange, WebserviceCell};
use crate::chidori::ChidoriCells;
use crate::GameState;
use crate::util::{change_active_editor_ui, deselect_editor_on_esc, despawn_screen, print_editor_text};

#[derive(Component)]
struct OnEditorScreen;

fn editor_update(
    mut contexts: EguiContexts,
    mut cells: ResMut<ChidoriCells>
) {
    let language = "python";

    egui::CentralPanel::default().show(contexts.ctx_mut(), |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut theme = egui_extras::syntax_highlighting::CodeTheme::dark();

            let mut layouter = |ui: &egui::Ui, string: &str, wrap_width: f32| {
                let mut layout_job =
                    egui_extras::syntax_highlighting::highlight(ui.ctx(), &theme, string, language);
                layout_job.wrap.max_width = wrap_width;

                // Fix font size
                for mut section in &mut layout_job.sections {
                    section.format.font_id = egui::FontId::new(14.0, FontFamily::Monospace);
                }

                ui.fonts(|f| f.layout_job(layout_job))
            };
            for cell in &cells.inner {
                match &cell.cell {
                    CellTypes::Code(CodeCell { name, source_code, ..}, _) => {
                        let mut s = source_code.clone();
                        let mut frame = egui::Frame::default().inner_margin(16.0).begin(ui);
                        {
                            let mut ui = &mut frame.content_ui;
                            ui.horizontal(|ui| {
                                ui.label("Code");
                                if let Some(name) = name {
                                    ui.label(name);
                                }
                            });
                            // Add widgets inside the frame
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut s)
                                        .code_editor()
                                        .lock_focus(true)
                                        .desired_width(f32::INFINITY)
                                        .layouter(&mut layouter),
                                );
                            });
                        }
                        frame.end(ui);
                    }
                    CellTypes::CodeGen(LLMCodeGenCell {..}, _) => {
                    }
                    CellTypes::Prompt(LLMPromptCell::Completion {..}   , _) => {
                    }
                    CellTypes::Prompt(LLMPromptCell::Chat {name, configuration, req, ..}   , _) => {
                        let mut s = req.clone();
                        let mut frame = egui::Frame::default().inner_margin(16.0).begin(ui);
                        {
                            let mut ui = &mut frame.content_ui;
                            ui.horizontal(|ui| {
                                ui.label("Prompt");
                                if let Some(name) = name {
                                    ui.label(name);
                                }
                            });
                            // Add widgets inside the frame
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut s)
                                        .code_editor()
                                        .lock_focus(true)
                                        .desired_width(f32::INFINITY)
                                        .layouter(&mut layouter),
                                );
                            });
                        }
                        frame.end(ui);
                    }
                    CellTypes::Embedding(LLMEmbeddingCell {..}, _) => {}
                    CellTypes::Web(WebserviceCell {..}, _) => {}
                    CellTypes::Template(TemplateCell {..}, _) => {}
                    CellTypes::Memory(MemoryCell {..}, _) => {}
                }
            }
        });
    });
}

pub fn editor_plugin(app: &mut App) {
    app
        .add_systems(OnExit(crate::GameState::Editor), despawn_screen::<OnEditorScreen>)
        .add_systems(
            Update,
            (
                editor_update
            ).run_if(in_state(GameState::Editor)),
        );
}
