use std::slice::IterMut;
use bevy::app::{App, Startup, Update};
use bevy::DefaultPlugins;
use bevy::math::UVec2;
use bevy::prelude::{ButtonBundle, Camera, Camera2dBundle, ClearColorConfig, Color, Commands, Component, default, in_state, IntoSystemConfigs, Local, OnEnter, OnExit, Query, Res, ResMut, Style, Val, Window, With};
use bevy::render::camera::Viewport;
use bevy::window::PrimaryWindow;
use bevy_cosmic_edit::{Attrs, CosmicBuffer, CosmicColor, CosmicEditBundle, CosmicEditPlugin, CosmicFontConfig, CosmicFontSystem, CosmicPrimaryCamera, CosmicSource, Family, FocusedWidget, Metrics};
use crate::bevy_egui::{EguiContexts};
use egui;
use egui::{Color32, FontFamily, Frame, Margin, Pos2, Rounding, Stroke, Ui, Vec2};
use egui_extras::syntax_highlighting::CodeTheme;
use egui_tiles::Tile;
use chidori_core::uuid::Uuid;
use chidori_core::cells::{CellTypes, CodeCell, LLMCodeGenCell, LLMEmbeddingCell, LLMPromptCell, LLMPromptCellChatConfiguration, MemoryCell, SupportedLanguage, SupportedModelProviders, TemplateCell, TextRange, WebserviceCell};
use chidori_core::chidori_static_analysis::language::{ChidoriStaticAnalysisError, Report};
use chidori_core::execution::primitives::identifiers::OperationId;
use chidori_core::sdk::entry::CellHolder;

use crate::chidori::{ChidoriCells, ChidoriExecutionGraph, ChidoriExecutionState, EguiTree, EguiTreeIdentities, InternalState};
use crate::egui_json_tree::JsonTree;
use crate::GameState;
use crate::util::{change_active_editor_ui, deselect_editor_on_esc, despawn_screen, egui_label, egui_logs, egui_rkyv, print_editor_text};

#[derive(Component)]
struct OnEditorScreen;


struct ViewingWatchedFileCells {
    is_showing_editor_cells: bool
}

impl Default for ViewingWatchedFileCells {
    fn default() -> Self {
        ViewingWatchedFileCells {
            is_showing_editor_cells: true
        }
    }
}

fn editor_update(
    mut contexts: EguiContexts,
    tree_identities: Res<EguiTreeIdentities>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    execution_state: Res<ChidoriExecutionState>,
    execution_graph: Res<ChidoriExecutionGraph>,
    internal_state: Res<InternalState>,
    mut tree: ResMut<EguiTree>,
    mut chidori_cells: ResMut<ChidoriCells>,
    mut viewing_watched_file_cells: Local<ViewingWatchedFileCells>
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
            let cells = if viewing_watched_file_cells.is_showing_editor_cells {
                chidori_cells.editor_cells.iter_mut()
            } else {
                chidori_cells.state_cells.iter_mut()
            };

            ui.horizontal(|ui| {
                if ui.radio(viewing_watched_file_cells.is_showing_editor_cells, "View Editor Cells").clicked() {
                    viewing_watched_file_cells.is_showing_editor_cells = true;
                }
                if ui.radio(!viewing_watched_file_cells.is_showing_editor_cells, "View Cells at Current State").clicked() {
                    viewing_watched_file_cells.is_showing_editor_cells = false;
                }
            });

            for mut cell_holder in cells {
                let op_id = cell_holder.op_id;

                // let mut frame = egui::Frame::default().fill(Color32::from_hex("#222222").unwrap()).outer_margin(Margin::symmetric(8.0, 16.0)).inner_margin(16.0).rounding(6.0).begin(ui);
                let mut frame = egui::Frame::default().outer_margin(Margin::symmetric(8.0, 16.0)).inner_margin(16.0).rounding(6.0).begin(ui);
                {
                    let ui = &mut frame.content_ui;
                    let mut exists_in_current_tree = false;
                    if let Some(applied_at) = &cell_holder.applied_at {
                        ui.label(format!("{:?}", applied_at));
                        exists_in_current_tree = execution_graph.exists_in_current_tree(applied_at);
                    } else {
                        if ui.button("Needs Update").clicked() {
                            internal_state.update_cell(cell_holder.clone());
                        }
                    }
                    ui.label(format!("{:?}", op_id));
                    match &mut cell_holder.cell {
                        CellTypes::Code(_, ..) => {
                            render_code_cell(
                                &execution_state,
                                &mut theme,
                                &op_id,
                                ui,
                                cell_holder,
                                exists_in_current_tree
                            );
                        }
                        CellTypes::CodeGen(..) => {
                            render_code_gen_cell(&execution_state, &op_id, ui, cell_holder, exists_in_current_tree);
                        }
                        CellTypes::Prompt(LLMPromptCell::Completion { .. }, _) => {}
                        CellTypes::Prompt(LLMPromptCell::Chat {..}, _) => {
                            render_prompt_cell(&execution_state, &op_id, ui, cell_holder, exists_in_current_tree);
                        }
                        CellTypes::Embedding(LLMEmbeddingCell { .. }, _) => {}
                        CellTypes::Template(..) => {
                            render_template_cell(&execution_state, &op_id, ui, cell_holder, exists_in_current_tree);
                        }
                        CellTypes::Memory(MemoryCell { .. }, _) => {}
                    }
                }
                frame.end(ui);
            }

            if !internal_state.display_example_modal {
                let mut frame = egui::Frame::default().outer_margin(Margin::symmetric(8.0, 16.0)).inner_margin(16.0).rounding(6.0).begin(ui);
                {
                    let mut ui = &mut frame.content_ui;
                    render_new_cell_interface(&execution_state, ui, &mut chidori_cells);
                }
                frame.end(ui);
            }

        });
    });
}

fn render_template_cell(execution_state: &ChidoriExecutionState, op_id: &OperationId, mut ui: &mut Ui, cell_holder: &mut CellHolder, exists_in_current_tree: bool) {
    let CellTypes::Template(TemplateCell { name, body }, _) = &mut cell_holder.cell else { panic!("Must be template cell")};
    ui.horizontal(|ui| {
        egui_label(ui, "Prompt");
        if let Some(name) = name {
            egui_label(ui, name);
        }
        render_applied_status(ui, exists_in_current_tree);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if ui.button("Open").clicked() {
                println!("Should open file");
            }
        });
    });
    // Add widgets inside the frame
    ui.vertical(|ui| {
        if ui.add(
            egui::TextEdit::multiline(body)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.needs_update = true;
            cell_holder.applied_at = None;
        }
        render_operation_output(&execution_state, &op_id, ui);
    });
}

fn render_operation_output(execution_state: &ChidoriExecutionState, op_id: &&OperationId, ui: &mut Ui) {
    if let Some(state) = &execution_state.inner {
        if let Some((exec_id, o)) = state.0.get(op_id) {
            if ui.button(format!("Go To Most Recent Execution")).clicked() {
                // TODO: move visualized head of graph to this point
            }
            if ui.button(format!("Revert to: {:?} {:?}", exec_id, op_id)).clicked() {}
            ui.push_id((exec_id, op_id), |ui| {
                ui.collapsing("Values", |ui| {
                    let response = JsonTree::new(format!("{} {} values", exec_id, op_id), &o.output.clone().unwrap())
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



fn render_prompt_cell(
    execution_state: &ChidoriExecutionState,
    op_id: &OperationId,
    mut ui: &mut Ui,
    cell_holder: &mut CellHolder,
    exists_in_current_tree: bool
) {

    // TODO: split the req, allow mutation of separate configuration text - validation of parsing

    let CellTypes::Prompt(LLMPromptCell::Chat { name, configuration, req, .. }, _) = &mut cell_holder.cell else {panic!("Must be llm prompt cell")};
    let mut cfg = serde_yaml::to_string(&configuration.clone()).unwrap();
    ui.horizontal(|ui| {
        egui_label(ui, "Prompt");
        if let Some(name) = name {
            egui_label(ui, name);
        }
        render_applied_status(ui, exists_in_current_tree);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if ui.button("Open").clicked() {
                // TODO:
                println!("Should open file");
            }
        });
    });

    // Add widgets inside the frame
    ui.vertical(|ui| {
        if ui.add(
            egui::TextEdit::multiline(&mut cfg)
                .font(egui::FontId::new(14.0, FontFamily::Monospace))
                .code_editor()
                .lock_focus(true)
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(8.0, 8.0))
        ).changed() {
            cell_holder.needs_update = true;
            cell_holder.applied_at = None;
        }
        ui.add_space(10.0);
        if ui.add(
            egui::TextEdit::multiline(req)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.needs_update = true;
            cell_holder.applied_at = None;
        }
        ui.add_space(10.0);

        render_operation_output(&execution_state, &op_id, ui);

    });
}

fn render_code_gen_cell(
    execution_state: &ChidoriExecutionState,
    op_id: &OperationId,
    mut ui: &mut Ui,
    mut cell_holder: &mut CellHolder,
    exists_in_current_tree: bool
) {
    let CellTypes::CodeGen(LLMCodeGenCell { name, req, .. }, _) = &mut cell_holder.cell else { panic!("Must be code gen cell") };
    let mut s = req.clone();
    ui.horizontal(|ui| {
        egui_label(ui, "Prompt");
        if let Some(name) = name {
            egui_label(ui, name);
        }
        render_applied_status(ui, exists_in_current_tree);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if ui.button("Open").clicked() {
                // TODO:
                println!("Should open file");
            }
        });
    });
    // Add widgets inside the frame
    ui.vertical(|ui| {
        if ui.add(
            egui::TextEdit::multiline(&mut s)
                .code_editor()
                .lock_focus(true)
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(8.0, 8.0))
        ).changed() {
            cell_holder.needs_update = true;
            cell_holder.applied_at = None;
        }
        ui.add_space(10.0);

        render_operation_output(&execution_state, &op_id, ui);

    });
}

fn render_applied_status(mut ui: &mut Ui, exists_in_current_tree: bool) {
    if exists_in_current_tree {
        egui_label(ui, "Applied");
    } else {
        if ui.button("Push Update").clicked() {
            println!("Attempting to push update");
        }
    }
}

fn render_code_cell(
    execution_state: &ChidoriExecutionState,
    theme: &mut CodeTheme,
    op_id: &OperationId,
    mut ui: &mut Ui,
    mut cell_holder: &mut CellHolder,
    exists_in_current_tree: bool
) {
    let CellTypes::Code(CodeCell { name, source_code, language, ..}, _) = &mut cell_holder.cell else { panic!("Mut be cell_holder") };
    let mut local_language = language.clone();
    let language_string = match language {
        SupportedLanguage::PyO3 => "python",
        SupportedLanguage::Starlark => "starlark",
        SupportedLanguage::Deno => "javascript/typescript"
    };

    let report = match language {
        SupportedLanguage::PyO3 | SupportedLanguage::Starlark => {
            let d = chidori_core::chidori_static_analysis::language::python::parse::extract_dependencies_python(&source_code);
            d.map(|d| chidori_core::chidori_static_analysis::language::python::parse::build_report(&d))
        },
        SupportedLanguage::Deno => {
            let d = chidori_core::chidori_static_analysis::language::javascript::parse::extract_dependencies_js(&source_code);
            d.map(|d| chidori_core::chidori_static_analysis::language::javascript::parse::build_report(&d))
        }
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

    ui.horizontal(|ui| {
        egui_label(ui, "Code");
        render_applied_status(ui, exists_in_current_tree);
        let old_button_padding = ui.spacing().button_padding;
        ui.spacing_mut().button_padding = Vec2::new(4.0, 2.0);
        let result = egui::ComboBox::from_id_source(format!("{:?} LangSelect", cell_holder.op_id))
            .height(16.0)
            .selected_text(language_string)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut local_language, SupportedLanguage::PyO3, "Python");
                ui.selectable_value(&mut local_language, SupportedLanguage::Deno, "JavaScript");
            });
        ui.spacing_mut().button_padding = old_button_padding;

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
        let mut code_frame = Frame::default().stroke(
            Stroke {
                width: 1.0,
                color: if report.is_err() {
                    Color32::from_hex("#FF0000").unwrap()
                } else {
                    Color32::from_hex("#222222").unwrap()
                },
            }
        )
            .rounding(Rounding::same(6.0))
            .inner_margin(16.0).begin(ui);
        {
            let mut ui = &mut code_frame.content_ui;
            if ui.add(
                egui::TextEdit::multiline(source_code)
                    .font(egui::FontId::new(14.0, FontFamily::Monospace))
                    .code_editor()
                    .lock_focus(true)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(8.0, 8.0))
                    .layouter(&mut layouter),
            ).changed() {
                cell_holder.needs_update = true;
                cell_holder.applied_at = None;
            }
        }
        code_frame.end(ui);

        ui.add_space(10.0);
        render_operation_output(&execution_state, &op_id, ui);
        ui.add_space(10.0);
        match report {
            Ok(report) => {
                ui.push_id((op_id, 0), |ui| {
                    ui.collapsing("Report", |ui| {
                        let response = JsonTree::new(format!("{:?} report", op_id), &serde_json::json!(&report))
                            .show(ui);
                    });
                });
            }
            Err(e) => {
                egui_label(ui, &format!("{:}", e));
            }
        }
    });
}

fn render_new_cell_interface(
    execution_state: &ChidoriExecutionState,
    mut ui: &mut Ui,
    x: &mut ResMut<ChidoriCells>
) {
    ui.horizontal(|ui| {
        egui_label(ui, "New Cell");
    });
    if ui.button("Add Code Cell").clicked() {
        x.editor_cells.push(CellHolder {
            cell: CellTypes::Code(CodeCell {
                name: None,
                language: SupportedLanguage::PyO3,
                source_code: "".to_string(),
                function_invocation: None,
            }, TextRange::default()),
            op_id: Uuid::new_v4(),
            applied_at: Default::default(),
            needs_update: false,
        })
    }
    if ui.button("Add Prompt Cell").clicked() {
        x.editor_cells.push(CellHolder {
            cell: CellTypes::Prompt(LLMPromptCell::Chat {
                function_invocation: false,
                configuration: Default::default(),
                name: None,
                provider: SupportedModelProviders::OpenAI,
                req: "".to_string(),
            }, TextRange::default()),
            op_id: Uuid::new_v4(),
            applied_at: Default::default(),
            needs_update: false,
        })
    }
    if ui.button("Add Template Cell").clicked() {
        x.editor_cells.push(CellHolder {
            cell: CellTypes::Template(TemplateCell {
                name: None,
                body: "".to_string(),
            }, TextRange::default()),
            op_id: Uuid::new_v4(),
            applied_at: Default::default(),
            needs_update: false,
        })
    }
    if ui.button("Add Code Generation Cell").clicked() {
        x.editor_cells.push(CellHolder {
            cell: CellTypes::CodeGen(LLMCodeGenCell {
                function_invocation: false,
                configuration: Default::default(),
                name: None,
                provider: SupportedModelProviders::OpenAI,
                req: "".to_string(),
            }, TextRange::default()),
            op_id: Uuid::new_v4(),
            applied_at: Default::default(),
            needs_update: false,
        })
    }
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
