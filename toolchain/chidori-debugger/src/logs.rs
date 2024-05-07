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
struct OnChatScreen;

fn logs_update(
    mut contexts: EguiContexts,
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut tree: ResMut<EguiTree>,
    mut cells: ResMut<ChidoriCells>
) {
    let language = "python";


    let mut container_frame = Frame::default().outer_margin(Margin {
        left: 0.0,
        right: 0.0,
        top: 0.0,
        bottom: 0.0,
    });
    tree.tree.tiles.iter().for_each(|(_, tile)| {
        match tile {
            Tile::Pane(p) => {
                if &p.nr == &"Code" {
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
    });

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

pub fn logs_plugin(app: &mut App) {
    app
        .add_systems(OnExit(crate::GameState::Chat), despawn_screen::<OnChatScreen>)
        .add_systems(
            Update,
            (
                logs_update
            ).run_if(in_state(GameState::Chat)),
        );
}
