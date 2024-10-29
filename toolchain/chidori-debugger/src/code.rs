use crate::bevy_egui::EguiContexts;
use crate::chidori::{CellState, ChidoriState, EguiTree, EguiTreeIdentities};
use crate::json_editor::{JsonEditorExample, Show};
use crate::util::{despawn_screen, egui_label, egui_logs, serialized_value_to_json_value};
use crate::{CurrentTheme, GameState, Theme};
use bevy::app::{App, Update};
use bevy::prelude::{in_state, Component, IntoSystemConfigs, Local, OnExit, Query, Res, ResMut, Window, With};
use bevy::window::PrimaryWindow;
use chidori_core::cells::{CellTypes, CodeCell, LLMCodeGenCell, LLMEmbeddingCell, LLMPromptCell, MemoryCell, SupportedLanguage, SupportedModelProviders, TemplateCell, TextRange};
use chidori_core::chidori_prompt_format::templating::templates::{SchemaItem, SchemaItemType};
use chidori_core::execution::primitives::identifiers::OperationId;
use chidori_core::sdk::entry::CellHolder;
use chidori_core::uuid::Uuid;
use egui;
use egui::{Align, Color32, FontFamily, Frame, Id, Margin, Rounding, Stroke, Ui, Vec2, Vec2b};
use egui_extras::syntax_highlighting::CodeTheme;
use egui_json_tree::JsonTree;
use egui_tiles::Tile;
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

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


fn file_browser(ui: &mut egui::Ui, path: &Path) {
    let metadata = fs::metadata(path).unwrap();
    let file_name = path.file_name().unwrap().to_str().unwrap();

    if metadata.is_dir() {
        let id = ui.make_persistent_id(path);
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);

        state.show_header(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Label::new("📁").sense(egui::Sense::click()));
                ui.label(file_name);
            });
        })
            .body(|ui| {
                if let Ok(entries) = fs::read_dir(path) {
                    for entry in entries {
                        if let Ok(entry) = entry {
                            file_browser(ui, &entry.path());
                        }
                    }
                }
            });
    } else {
        ui.horizontal(|ui| {
            ui.add(egui::Label::new("📄").sense(egui::Sense::click()));
            ui.label(file_name);
        });
    }
}



fn editor_update(
    mut contexts: EguiContexts,
    tree_identities: Res<EguiTreeIdentities>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut chidori_state: ResMut<ChidoriState>,
    current_theme: Res<CurrentTheme>,
    mut tree: ResMut<EguiTree>,
    mut viewing_watched_file_cells: Local<ViewingWatchedFileCells>,
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

    if hide_all || chidori_state.display_example_modal {
        return;
    }


    egui::CentralPanel::default().frame(container_frame).show(contexts.ctx_mut(), |ui| {
        let available_height = ui.available_height();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_height(available_height);
                ui.push_id("file_browser", |ui| {
                    egui::ScrollArea::vertical().auto_shrink(Vec2b::new(true, false)).show(ui, |ui| {
                        ui.set_max_width(200.0);
                        let path = &chidori_state.watched_path.lock().unwrap().clone();
                        if path.is_some() {
                            file_browser(ui, Path::new(&path.as_ref().unwrap()));
                        }
                    });
                });
            });

            ui.vertical(|ui| {
                ui.set_height(available_height);
                ui.push_id("notebook", |ui| {
                    egui::ScrollArea::vertical().min_scrolled_width(f32::INFINITY).auto_shrink(Vec2b::new(true, false)).show(ui, |ui| {
                        let mut theme = egui_extras::syntax_highlighting::CodeTheme::dark();

                        let mut frame = egui::Frame::default()
                            .fill(current_theme.theme.card)
                            .stroke(current_theme.theme.card_border)
                            .outer_margin(Margin::symmetric(8.0, 16.0))
                            .inner_margin(16.0)
                            .rounding(current_theme.theme.radius as f32)
                            .begin(ui);
                        {
                            ui.set_width(800.0);
                            let ui = &mut frame.content_ui;
                            ui.horizontal(|ui| {
                                if ui.radio(viewing_watched_file_cells.is_showing_editor_cells, "View Editor Cells").clicked() {
                                    viewing_watched_file_cells.is_showing_editor_cells = true;
                                }
                                if ui.radio(!viewing_watched_file_cells.is_showing_editor_cells, "View Cells at Current State").clicked() {
                                    viewing_watched_file_cells.is_showing_editor_cells = false;
                                }
                            });
                        }
                        frame.end(ui);

                        // let cells = if viewing_watched_file_cells.is_showing_editor_cells {
                        //     chidori_state.editor_cells.iter_mut()
                        // } else {
                        //     chidori_state.state_cells.iter_mut()
                        // };
                        let mut cells_ids: Vec<_> = chidori_state.editor_cells.keys().cloned().collect();
                        cells_ids.sort();
                        for cell_id in &cells_ids {
                            editable_chidori_cell_content(&mut chidori_state, &current_theme.theme, ui, &mut theme, *cell_id);
                        }

                        if !chidori_state.display_example_modal {
                            let mut frame = egui::Frame::default()
                                .fill(current_theme.theme.card)
                                .stroke(current_theme.theme.card_border)
                                .outer_margin(Margin::symmetric(8.0, 16.0))
                                .inner_margin(16.0)
                                .rounding(current_theme.theme.radius as f32)
                                .begin(ui);
                            {
                                let mut ui = &mut frame.content_ui;
                                let state_binding = chidori_state.local_cell_state.entry(Uuid::nil()).or_insert(Arc::new(Mutex::new(CellState::default()))).clone();
                                let mut state = state_binding.lock().unwrap();
                                render_new_cell_interface(
                                    ui,
                                    &mut chidori_state,
                                    &mut state,
                                    &mut theme
                                );
                            }
                            frame.end(ui);
                        }

                    });
                });
            });

        });

    });
}

pub fn editable_chidori_cell_content(
    mut chidori_state: &mut ChidoriState,
    theme: &Theme,
    ui: &mut Ui,
    mut code_theme: &mut CodeTheme,
    op_id: OperationId
) {
    let binding = if let Some(binding) = chidori_state.editor_cells.get(&op_id) {
        binding.clone()
    } else {
        return;
    };
    let mut cell_holder = &mut binding.lock().unwrap();

    // let mut frame = egui::Frame::default().fill(Color32::from_hex("#222222").unwrap()).outer_margin(Margin::symmetric(8.0, 16.0)).inner_margin(16.0).rounding(6.0).begin(ui);
    let mut frame = egui::Frame::default()
        .fill(theme.card)
        .stroke(theme.card_border)
        .outer_margin(Margin::symmetric(8.0, 16.0))
        .inner_margin(16.0)
        .rounding(theme.radius as f32)
        .begin(ui);
    {
        let ui = &mut frame.content_ui;
        ui.set_max_width(800.0);
        let mut exists_in_current_tree = false;
        if let Some(applied_at) = &cell_holder.applied_at {
            if chidori_state.debug_mode {
                ui.label(format!("Applied At: {:?}", applied_at));
            }
            exists_in_current_tree = chidori_state.exists_in_current_tree(applied_at);
        } else {
            if ui.button("Pending Application To Graph").clicked() {
                chidori_state.update_cell(cell_holder.clone());
            }
        }
        if chidori_state.debug_mode {
            ui.label(format!("Operation Id: {:?}", op_id));
        }
        match &mut cell_holder.cell {
            CellTypes::Code(_, ..) => {
                render_code_cell(
                    &mut chidori_state,
                    &mut code_theme,
                    &op_id,
                    ui,
                    cell_holder,
                    exists_in_current_tree
                );
            }
            CellTypes::CodeGen(..) => {
                render_code_gen_cell(&mut chidori_state, &op_id, ui, cell_holder, exists_in_current_tree);
            }
            CellTypes::Prompt(LLMPromptCell::Completion { .. }, _) => {}
            CellTypes::Prompt(LLMPromptCell::Chat { .. }, _) => {
                render_prompt_cell(&mut chidori_state, &op_id, ui, cell_holder, exists_in_current_tree);
            }
            CellTypes::Template(..) => {
                render_template_cell(&mut chidori_state, &op_id, ui, cell_holder, exists_in_current_tree);
            }
        }

        let state_binding = chidori_state.local_cell_state.entry(op_id).or_insert(Arc::new(Mutex::new(CellState::default()))).clone();
        let mut state = state_binding.lock().unwrap();

        ui.push_id(("add_cell", op_id), |ui| {
            egui::CollapsingHeader::new("Add Cell")
                .show(ui, |ui| {
                    render_new_cell_interface(
                        ui,
                        chidori_state,
                        &mut state,
                        &mut code_theme,
                    );
                });
        });
        //
        // if !state.is_new_cell_open {
        //     if ui.button("Add Cell").clicked() {
        //         state.is_new_cell_open = true;
        //     }
        // } else {
        //     if ui.button("Close").clicked() {
        //         state.is_new_cell_open = false;
        //     }
        // }

    }
    frame.end(ui);
}

fn render_template_cell(
    execution_state: &ChidoriState,
    op_id: &OperationId,
    mut ui: &mut Ui,
    cell_holder: &mut CellHolder,
    exists_in_current_tree: bool
) {
    let CellTypes::Template(TemplateCell { name, body , backing_file_reference, ..}, _) = &mut cell_holder.cell else { panic!("Must be template cell")};
    if let Some(name) = name {
        if ui.add(
            egui::TextEdit::singleline(name)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.needs_update = true;
            cell_holder.applied_at = None;
        }
    }
    ui.horizontal(|ui| {
        egui_label(ui, "Template");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if backing_file_reference.is_some() {
                if ui.button("Open File").clicked() {
                    println!("Should open file");
                }
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

fn render_operation_output(execution_state: &ChidoriState, op_id: &&OperationId, ui: &mut Ui) {
    if let Some(state) = &execution_state.merged_state_history {
        if let Some((exec_id, o)) = state.0.get(op_id) {
            if ui.button(format!("View Most Recent Execution")).clicked() {
                // TODO: move visualized head of graph to this point
            }
            if ui.button(format!("Revert To Most Recent Execution")).clicked() {}
            ui.push_id((exec_id, op_id), |ui| {
                ui.collapsing("Output", |ui| {
                    let response = JsonTree::new(format!("{} {} values", exec_id, op_id), &serialized_value_to_json_value(&o.output.clone().unwrap()))
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


fn populate_json_content(schema: &SchemaItem, existing_content: Option<&Value>) -> Value {
    match schema.ty {
        SchemaItemType::Object => {
            let mut obj = match existing_content {
                Some(Value::Object(map)) => map.clone(),
                _ => Map::new(),
            };

            // Remove keys that are not in the schema
            obj.retain(|key, _| schema.items.contains_key(key));

            for (key, item) in &schema.items {
                let existing_value = obj.get(key).cloned();
                obj.insert(key.clone(), populate_json_content(item, existing_value.as_ref()));
            }
            Value::Object(obj)
        },
        SchemaItemType::Array => {
            match existing_content {
                Some(Value::Array(arr)) => {
                    if let Some((_, item)) = schema.items.iter().next() {
                        Value::Array(arr.iter().map(|v| populate_json_content(item, Some(v))).collect())
                    } else {
                        Value::Array(vec![])
                    }
                },
                _ => {
                    if let Some((_, item)) = schema.items.iter().next() {
                        Value::Array(vec![populate_json_content(item, None)])
                    } else {
                        Value::Array(vec![])
                    }
                }
            }
        },
        SchemaItemType::String => {
            match existing_content {
                Some(Value::String(s)) => Value::String(s.clone()),
                _ => Value::String(String::new()),
            }
        },
    }
}




fn render_prompt_cell(
    execution_state: &mut ChidoriState,
    op_id: &OperationId,
    mut ui: &mut Ui,
    cell_holder: &mut CellHolder,
    exists_in_current_tree: bool
) {
    let CellTypes::Prompt(LLMPromptCell::Chat { name, configuration, req, complete_body, backing_file_reference,  .. }, _) = &mut cell_holder.cell else {panic!("Must be llm prompt cell")};

    if let Some(name) = name {

        if ui.add(
            egui::TextEdit::singleline(name)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.needs_update = true;
            cell_holder.applied_at = None;
        }
    }

    let state_binding = execution_state.local_cell_state.entry(*op_id).or_insert(Arc::new(Mutex::new(CellState::default()))).clone();
    let mut state = state_binding.lock().unwrap();
    let (frontmatter, req) = chidori_core::chidori_prompt_format::templating::templates::split_frontmatter(&complete_body).map_err(|e| {
        anyhow::Error::msg(e.to_string())
    }).unwrap_or((String::new(), String::new()));
    let schema = chidori_core::chidori_prompt_format::templating::templates::analyze_referenced_partials(&&req);
    if !schema.is_err() {
        state.json_content = populate_json_content(&schema.unwrap(), Some(&state.json_content));
    }

    // let mut cfg = serde_yaml::to_string(&configuration.clone()).unwrap();
    ui.horizontal(|ui| {
        egui_label(ui, "Prompt");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if backing_file_reference.is_some() {
                if ui.button("Open File").clicked() {
                    // TODO:
                    println!("Should open file");
                }
            }
            if ui.button("Open Local Repl").clicked() {
                state.is_repl_open = !state.is_repl_open;
            }
        });
    });

    // Add widgets inside the frame
    ui.vertical(|ui| {
        if ui.add(
            egui::TextEdit::multiline(complete_body)
                .id(Id::new(format!("{:?} prompt_editor", op_id)))
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.needs_update = true;
            cell_holder.applied_at = None;
        }
        ui.add_space(10.0);

        if state.is_repl_open {
            let mut code_frame = Frame::default()
                .begin(ui);
            {
                let mut ui = &mut code_frame.content_ui;
                let editor_id = ui.make_persistent_id(format!("{:?} prompt_payload_editor", op_id));
                ui.push_id(editor_id, |ui| {
                    let mut editor = JsonEditorExample::new(&mut state.json_content, editor_id);
                    editor.show(ui);
                });
                if ui.button("Execute").clicked() {

                }
            }
            code_frame.end(ui);
        }

        render_operation_output(&execution_state, &op_id, ui);

    });
}

fn render_code_gen_cell(
    execution_state: &mut ChidoriState,
    op_id: &OperationId,
    mut ui: &mut Ui,
    mut cell_holder: &mut CellHolder,
    exists_in_current_tree: bool
) {
    let CellTypes::CodeGen(LLMCodeGenCell { name, req, complete_body, backing_file_reference, .. }, _) = &mut cell_holder.cell else { panic!("Must be code gen cell") };
    let state_binding = execution_state.local_cell_state.entry(*op_id).or_insert(Arc::new(Mutex::new(CellState::default()))).clone();
    let mut state = state_binding.lock().unwrap();
    if let Some(name) = name {
        if ui.add(
            egui::TextEdit::singleline(name)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.needs_update = true;
            cell_holder.applied_at = None;
        }
    }
    ui.horizontal(|ui| {
        egui_label(ui, "Code Generation");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if backing_file_reference.is_some() {
                if ui.button("Open File").clicked() {
                    // TODO:
                    println!("Should open file");
                }
            }
            if ui.button("Open Local Repl").clicked() {
                state.is_repl_open = !state.is_repl_open;
            }
        });
    });
    // Add widgets inside the frame
    ui.vertical(|ui| {
        if ui.add(
            egui::TextEdit::multiline(complete_body)
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


fn render_code_cell(
    chidori_state: &mut ChidoriState,
    theme: &mut CodeTheme,
    op_id: &OperationId,
    mut ui: &mut Ui,
    mut cell_holder: &mut CellHolder,
    exists_in_current_tree: bool
) {
    let state_binding = chidori_state.local_cell_state.entry(*op_id).or_insert(Arc::new(Mutex::new(CellState::default()))).clone();
    let mut state = state_binding.lock().unwrap();
    let CellTypes::Code(CodeCell { name, source_code, language, backing_file_reference, ..}, _) = &mut cell_holder.cell else { panic!("Mut be cell_holder") };
    if let Some(name) = name {
        if ui.add(
            egui::TextEdit::singleline(name)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.needs_update = true;
            cell_holder.applied_at = None;
        }
    }

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

    let language_clone =  language.clone();
    let mut layouter = |ui: &egui::Ui, text_string: &str, wrap_width: f32| {
        let syntax_language = match language_clone {
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
        ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
            egui_label(ui, "Code");
            let old_button_padding = ui.spacing().button_padding;
            ui.spacing_mut().button_padding = Vec2::new(4.0, 2.0);
            let result = egui::ComboBox::from_id_source(format!("{:?} LangSelect", cell_holder.op_id))
                .height(16.0)
                .selected_text(language_string)
                .show_ui(ui, |ui| {
                    let py = ui.selectable_value(&mut local_language, SupportedLanguage::PyO3, "Python");
                    let js = ui.selectable_value(&mut local_language, SupportedLanguage::Deno, "JavaScript");
                    if py.clicked() || js.clicked() {
                        *language = local_language;
                    }
                });
            ui.spacing_mut().button_padding = old_button_padding;
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if backing_file_reference.is_some() {
                if ui.button("Open File").clicked() {
                    // TODO:
                    println!("Should open file");
                }
            }
            if ui.button("Open Local Repl").clicked() {
                state.is_repl_open = !state.is_repl_open;
            }
        });
    });
    ui.vertical(|ui| {
        let mut code_frame = Frame::default().stroke(
            Stroke {
                width: 0.5,
                color: if report.is_err() {
                    Color32::from_hex("#FF0000").unwrap()
                } else {
                    Color32::from_hex("#222222").unwrap()
                },
            }
        )
            .rounding(Rounding::same(6.0))
            .begin(ui);
        {
            let mut ui = &mut code_frame.content_ui;
            if ui.add(
                egui::TextEdit::multiline(source_code)
                    .id(Id::new(format!("{:?} source_code", op_id)))
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

        if state.is_repl_open {
            let mut code_frame = Frame::default().stroke(
                Stroke {
                    width: 0.5,
                    color: if report.is_err() {
                        Color32::from_hex("#FF0000").unwrap()
                    } else {
                        Color32::from_hex("#222222").unwrap()
                    },
                }
            )
                .rounding(Rounding::same(6.0))
                .begin(ui);
            {
                let mut ui = &mut code_frame.content_ui;
                if ui.add(
                    egui::TextEdit::multiline(&mut state.repl_content)
                        .id(Id::new(format!("{:?} repl_content", op_id)))
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

                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    if ui.button("Execute").clicked() {
                        println!("Should run repl content");
                    }
                });
            }
            code_frame.end(ui);
        }

        render_operation_output(&chidori_state, &op_id, ui);

        if chidori_state.debug_mode {
            ui.add_space(10.0);
            match report {
                Ok(report) => {
                    ui.push_id((op_id, 0), |ui| {
                        ui.collapsing("Cell Analysis", |ui| {
                            // let response = JsonTree::new(format!("{:?} report", op_id), &serde_json::json!(&report))
                            //     .show(ui);
                        });
                    });
                }
                Err(e) => {
                    egui_label(ui, &format!("{:}", e));
                }
            }
        }
    });
}

fn render_new_cell_interface(
    mut ui: &mut Ui,
    mut chidori_state: &mut ChidoriState,
    mut state: &mut CellState,
    code_theme: &mut CodeTheme
) {
    if state.temp_cell.is_some() {
        if ui.button("Cancel").clicked() {
            state.temp_cell = None;
            state.is_new_cell_open = false;
        }
    }

    if state.temp_cell.is_none() {
        ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
        if ui.button("Add Code Cell").clicked() {
            let op_id = Uuid::now_v7();
            state.temp_cell = Some(CellHolder {
                cell: CellTypes::Code(CodeCell {
                    backing_file_reference: None,
                    name: None,
                    language: SupportedLanguage::PyO3,
                    source_code: "".to_string(),
                    function_invocation: None,
                }, TextRange::default()),
                op_id,
                applied_at: Default::default(),
                needs_update: false,
            });
        }
        if ui.button("Add Prompt Cell").clicked() {
            let op_id = Uuid::now_v7();
            state.temp_cell = Some(CellHolder {
                cell: CellTypes::Prompt(LLMPromptCell::Chat {
                    backing_file_reference: None,
                    function_invocation: false,
                    configuration: Default::default(),
                    name: None,
                    provider: SupportedModelProviders::OpenAI,
                    complete_body: "".to_string(),
                    req: "".to_string(),
                }, TextRange::default()),
                op_id,
                applied_at: Default::default(),
                needs_update: false,
            });
        }
        if ui.button("Add Template Cell").clicked() {
            let op_id = Uuid::now_v7();
            state.temp_cell = Some(CellHolder {
                cell: CellTypes::Template(TemplateCell {
                    backing_file_reference: None,
                    name: None,
                    body: "".to_string(),
                }, TextRange::default()),
                op_id,
                applied_at: Default::default(),
                needs_update: false,
            });
        }
        if ui.button("Add Code Generation Cell").clicked() {
            let op_id = Uuid::now_v7();
            state.temp_cell = Some((CellHolder {
                cell: CellTypes::CodeGen(LLMCodeGenCell {
                    backing_file_reference: None,
                    function_invocation: false,
                    configuration: Default::default(),
                    name: None,
                    provider: SupportedModelProviders::OpenAI,
                    req: "".to_string(),
                    complete_body: "".to_string(),
                }, TextRange::default()),
                op_id,
                applied_at: Default::default(),
                needs_update: false,
            }));
        }
    }

let exists_in_current_tree = false;
    let mut temp_cell_created = false;
    if let Some(temp_cell) = state.temp_cell.as_mut() {
        let op_id = temp_cell.op_id.clone();
        match &mut temp_cell.cell {
            CellTypes::Code(_, ..) => {
                render_code_cell(
                    &mut chidori_state,
                    code_theme,
                    &op_id,
                    ui,
                    temp_cell,
                    exists_in_current_tree
                );
            }
            CellTypes::CodeGen(..) => {
                render_code_gen_cell(&mut chidori_state, &op_id, ui, temp_cell, exists_in_current_tree);
            }
            CellTypes::Prompt(LLMPromptCell::Completion { .. }, _) => {}
            CellTypes::Prompt(LLMPromptCell::Chat { .. }, _) => {
                render_prompt_cell(&mut chidori_state, &op_id, ui, temp_cell, exists_in_current_tree);
            }
            CellTypes::Template(..) => {
                render_template_cell(&mut chidori_state, &op_id, ui, temp_cell, exists_in_current_tree);
            }
        }

        if ui.button("Save and Push To Graph").clicked() {
            // chidori_state.editor_cells.insert(op_id, Arc::new(Mutex::new(temp_cell.clone())));
            chidori_state.update_cell(temp_cell.clone());
            temp_cell_created = true;
        }

    }
    if temp_cell_created {
        state.temp_cell = None;
        state.is_new_cell_open = false;
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
