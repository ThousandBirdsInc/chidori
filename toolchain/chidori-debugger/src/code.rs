use bevy::app::{App, Startup, Update};
use bevy::DefaultPlugins;
use bevy::math::UVec2;
use bevy::prelude::{ButtonBundle, Camera, Camera2dBundle, ClearColorConfig, Color, Commands, Component, default, in_state, IntoSystemConfigs, Local, OnEnter, OnExit, Query, Res, ResMut, Style, Val, Window, With};
use bevy::render::camera::Viewport;
use bevy::window::PrimaryWindow;
use bevy_cosmic_edit::{Attrs, CosmicBuffer, CosmicColor, CosmicEditBundle, CosmicEditPlugin, CosmicFontConfig, CosmicFontSystem, CosmicPrimaryCamera, CosmicSource, Family, FocusedWidget, Metrics};
use crate::bevy_egui::{EguiContexts};
use egui;
use egui::{Color32, FontFamily, Frame, Margin, Pos2, Ui};
use egui_extras::syntax_highlighting::CodeTheme;
use egui_tiles::Tile;
use chidori_core::cells::{CellTypes, CodeCell, LLMCodeGenCell, LLMEmbeddingCell, LLMPromptCell, LLMPromptCellChatConfiguration, MemoryCell, SupportedLanguage, TemplateCell, TextRange, WebserviceCell};
use crate::chidori::{ChidoriCells, ChidoriExecutionState, EguiTree, EguiTreeIdentities};
use crate::egui_json_tree::JsonTree;
use crate::GameState;
use crate::util::{change_active_editor_ui, deselect_editor_on_esc, despawn_screen, egui_label, egui_logs, egui_rkyv, print_editor_text};

#[derive(Component)]
struct OnEditorScreen;

fn editor_update(
    mut contexts: EguiContexts,
    tree_identities: Res<EguiTreeIdentities>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut execution_state: Res<ChidoriExecutionState>,
    mut tree: ResMut<EguiTree>,
    mut cells: ResMut<ChidoriCells>,
    mut viewing_watched_file_cells: Local<bool>
) {
    let window = q_window.single();
    let mut hide_all = false;

    let mut container_frame = Frame::default().outer_margin(Margin {
        left: 0.0,
        right: 0.0,
        top: 0.0,
        bottom: 0.0,
    }).inner_margin(16.0);
    if let Some(code_tile) = tree_identities.code_tile {
        if let Some(tile) = tree.tree.tiles.get(code_tile) {
            match tile {
                Tile::Pane(p) => {
                    if !tree.tree.active_tiles().contains(&code_tile) {
                        hide_all = true;
                    } else {
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
        }
    }

    if hide_all {
        return;
    }

    egui::CentralPanel::default().frame(container_frame).show(contexts.ctx_mut(), |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut theme = egui_extras::syntax_highlighting::CodeTheme::dark();
            let cells = if *viewing_watched_file_cells {
                cells.editor_cells.iter()
            } else {
                cells.state_cells.iter()
            };

            ui.horizontal(|ui| {
                if ui.radio(*viewing_watched_file_cells, "View Editor Cells").clicked() {
                    *viewing_watched_file_cells = true;
                }
                if ui.radio(!*viewing_watched_file_cells, "View Cells at Current State").clicked() {
                    *viewing_watched_file_cells = false;
                }
            });

            for cell_holder in cells {
                let op_id = cell_holder.op_id;

                let mut frame = egui::Frame::default().fill(Color32::from_hex("#222222").unwrap()).outer_margin(Margin::symmetric(8.0, 16.0)).inner_margin(16.0).rounding(6.0).begin(ui);
                {
                    let mut ui = &mut frame.content_ui;
                    match &cell_holder.cell {
                        CellTypes::Code(CodeCell { name, source_code, language, ..}, _) => {
                            render_code_cell(&mut execution_state, &mut theme, &op_id, ui, name, source_code, language);
                        }
                        CellTypes::CodeGen(LLMCodeGenCell { name, req, .. }, _) => {
                            render_code_gen_cell(&mut execution_state, &op_id, ui, name, req);
                        }
                        CellTypes::Prompt(LLMPromptCell::Completion { .. }, _) => {}
                        CellTypes::Prompt(LLMPromptCell::Chat { name, configuration, req, .. }, _) => {
                            render_prompt_cell(&mut execution_state, &op_id, ui, name, &configuration, req);
                        }
                        CellTypes::Embedding(LLMEmbeddingCell { .. }, _) => {}
                        CellTypes::Web(WebserviceCell { name, configuration, .. }, _) => {
                            render_web_service_cell(&mut execution_state, &op_id, ui, name, configuration);
                        }
                        CellTypes::Template(TemplateCell { name, body }, _) => {
                            render_template_cell(&mut execution_state, &op_id, ui, name, body);
                        }
                        CellTypes::Memory(MemoryCell { .. }, _) => {}
                    }

                }
                frame.end(ui);
            }
        });
    });
}

fn render_template_cell(execution_state: &mut Res<ChidoriExecutionState>, op_id: &Option<usize>, mut ui: &mut Ui, name: &Option<String>, body: &String) {
    let mut s = body.clone();
    ui.horizontal(|ui| {
        egui_label(ui, "Prompt");
        if let Some(name) = name {
            egui_label(ui, name);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if ui.button("Open").clicked() {
                // TODO:
                println!("Should open file");
            }
        });
    });
    // Add widgets inside the frame
    ui.vertical(|ui| {
        ui.add(
            egui::TextEdit::multiline(&mut s)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        );
        render_operation_output(&execution_state, &op_id, ui);
    });
}

fn render_operation_output(execution_state: &&mut Res<ChidoriExecutionState>, op_id: &&Option<usize>, ui: &mut Ui) {
    if let Some(state) = &execution_state.inner {
        if let Some(op_id) = &op_id {
            if let Some((exec_id, o)) = state.0.get(op_id) {
                if ui.button(format!("Go To Most Recent Execution")).clicked() {}
                if ui.button(format!("Revert to: {:?} {:?}", exec_id, op_id)).clicked() {}
                ui.push_id((exec_id, op_id), |ui| {
                    ui.collapsing("Values", |ui| {
                        let response = JsonTree::new(format!("{} {} values", exec_id, op_id), &o.output)
                            .show(ui);
                    });
                    ui.collapsing("Logs", |ui| {
                        egui_logs(ui, &o.stdout);
                        egui_logs(ui, &o.stderr);
                    });
                });
            }
        }
    }
}

fn render_web_service_cell(execution_state: &mut Res<ChidoriExecutionState>, op_id: &Option<usize>, mut ui: &mut Ui, name: &Option<String>, configuration: &String) {
    let mut s = configuration.clone();
    ui.horizontal(|ui| {
        egui_label(ui, "Prompt");
        if let Some(name) = name {
            egui_label(ui, name);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if ui.button("Open").clicked() {
                // TODO:
                println!("Should open file");
            }
        });
    });
    // Add widgets inside the frame
    ui.vertical(|ui| {
        ui.add(
            egui::TextEdit::multiline(&mut s)
                .code_editor()
                .lock_focus(true)
                .desired_width(f32::INFINITY)
        );
        render_operation_output(&execution_state, &op_id, ui);
    });
}

fn render_prompt_cell(execution_state: &mut Res<ChidoriExecutionState>, op_id: &Option<usize>, mut ui: &mut Ui, name: &Option<String>, configuration: &&LLMPromptCellChatConfiguration, req: &String) {
    let mut s = req.clone();
    let mut cfg = serde_yaml::to_string(&configuration.clone()).unwrap();
    ui.horizontal(|ui| {
        egui_label(ui, "Prompt");
        if let Some(name) = name {
            egui_label(ui, name);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if ui.button("Open").clicked() {
                // TODO:
                println!("Should open file");
            }
        });
    });
    // Add widgets inside the frame
    ui.vertical(|ui| {
        ui.add(
            egui::TextEdit::multiline(&mut cfg)
                .font(egui::FontId::new(14.0, FontFamily::Monospace))
                .code_editor()
                .lock_focus(true)
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(8.0, 8.0))
        );
        ui.add_space(10.0);
        ui.add(
            egui::TextEdit::multiline(&mut s)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        );
        ui.add_space(10.0);

        render_operation_output(&execution_state, &op_id, ui);

    });
}

fn render_code_gen_cell(execution_state: &mut Res<ChidoriExecutionState>, op_id: &Option<usize>, mut ui: &mut Ui, name: &Option<String>, req: &String) {
    let mut s = req.clone();
    ui.horizontal(|ui| {
        egui_label(ui, "Prompt");
        if let Some(name) = name {
            egui_label(ui, name);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if ui.button("Open").clicked() {
                // TODO:
                println!("Should open file");
            }
        });
    });
    // Add widgets inside the frame
    ui.vertical(|ui| {
        ui.add(
            egui::TextEdit::multiline(&mut s)
                .code_editor()
                .lock_focus(true)
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(8.0, 8.0))
        );
        ui.add_space(10.0);

        render_operation_output(&execution_state, &op_id, ui);

    });
}

fn render_code_cell(execution_state: &mut Res<ChidoriExecutionState>, theme: &mut CodeTheme, op_id: &Option<usize>, mut ui: &mut Ui, name: &Option<String>, source_code: &String, language: &SupportedLanguage) {
    let language_string = match language {
        SupportedLanguage::PyO3 => "python",
        SupportedLanguage::Starlark => "starlark",
        SupportedLanguage::Deno => "javascript/typescript"
    };

    let mut layouter = |ui: &egui::Ui, text_string: &str, wrap_width: f32| {
        let syntax_language = match language {
            SupportedLanguage::PyO3 => "py",
            SupportedLanguage::Starlark => "py",
            SupportedLanguage::Deno => "js"
        };
        let mut layout_job =
            egui_extras::syntax_highlighting::highlight(ui.ctx(), &theme, text_string, syntax_language);
        layout_job.wrap.max_width = wrap_width;

        // Fix font size
        for mut section in &mut layout_job.sections {
            section.format.font_id = egui::FontId::new(14.0, FontFamily::Monospace);
        }

        ui.fonts(|f| f.layout_job(layout_job))
    };

    let mut s = source_code.clone();
    ui.horizontal(|ui| {
        egui_label(ui, "Code");
        egui_label(ui, language_string);
        if let Some(name) = name {
            egui_label(ui, name);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if ui.button("Open").clicked() {
                // TODO:
                println!("Should open file");
            }
        });
    });
    ui.vertical(|ui| {
        ui.add(
            egui::TextEdit::multiline(&mut s)
                .font(egui::FontId::new(14.0, FontFamily::Monospace))
                .code_editor()
                .lock_focus(true)
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(8.0, 8.0))
                .layouter(&mut layouter),
        );
        ui.add_space(10.0);
        render_operation_output(&execution_state, &op_id, ui);
    });
}

pub fn editor_plugin(app: &mut App) {
    app
        .add_systems(OnExit(crate::GameState::Graph), despawn_screen::<OnEditorScreen>)
        .add_systems(
            Update,
            (
                editor_update
            ).run_if(in_state(GameState::Graph)),
        );
}
