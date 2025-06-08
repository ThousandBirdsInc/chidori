use std::cmp::Ordering;
use std::sync::MutexGuard;
use crate::application::{CellState, ChidoriState};
use crate::components::json_editor::{JsonEditorExample, Show};
use crate::util::{egui_label, egui_logs, serialized_value_to_json_value};
use crate::{CurrentTheme, Theme};
use crate::code::utils::populate_json_content;
use chidori_core::cells::{CellTypes, CodeCell, LLMCodeGenCell, LLMPromptCell, PlainTextCell, SupportedLanguage, SupportedModelProviders, TemplateCell};
use chidori_core::execution::primitives::identifiers::OperationId;
use chidori_core::sdk::interactive_chidori_wrapper::CellHolder;
use egui::{Align, Color32, FontFamily, Frame, Id, Margin, Rounding, Stroke, Ui, Vec2, Vec2b};
use egui_extras::syntax_highlighting::CodeTheme;
use egui_json_tree::JsonTree;

pub fn editable_chidori_cell_content(
    mut chidori_state: &mut ChidoriState,
    theme: &Theme,
    ui: &mut Ui,
    mut code_theme: &mut CodeTheme,
    op_id: OperationId,
    is_embedded: bool
) {
    let Some(mut binding) = chidori_state.local_cell_state.get(&op_id).map(|x| x.value().clone()) else {
        return;
    };
    let mut binding = binding.lock();
    let mut cell_state = binding.as_mut().unwrap();
    let Some(mut cell_holder)= cell_state.cell.as_mut() else {
        return;
    };
    let is_plain_text = {
        matches!(cell_holder.cell, CellTypes::PlainText(..))
    };

    let mut frame = if is_embedded {
        egui::Frame::default()
            .begin(ui)
    } else {
        egui::Frame::default()
            .fill(if !is_plain_text {theme.card } else { theme.background})
            .stroke(theme.card_border)
            .outer_margin( Margin::symmetric(8.0, 16.0))
            .inner_margin(16.0)
            .rounding(theme.radius as f32)
            .begin(ui)
    };
    {
        let ui = &mut frame.content_ui;
        if !is_embedded {
            ui.set_max_width(800.0);
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
                    cell_state
                );
            }
            CellTypes::CodeGen(..) => {
                render_code_gen_cell(&mut chidori_state, &op_id, ui, cell_state);
            }
            CellTypes::Prompt(LLMPromptCell::Completion { .. }, _) => {}
            CellTypes::Prompt(LLMPromptCell::Chat { .. }, _) => {
                render_prompt_cell(&mut chidori_state, &op_id, ui, cell_state);
            }
            CellTypes::Template(..) => {
                render_template_cell(&mut chidori_state, &op_id, ui, cell_state);
            }
            CellTypes::PlainText(..) => {
                render_plaintext_cell(&mut chidori_state, &op_id, ui, cell_state);
            }
        }

        if !is_plain_text {
            ui.push_id(("add_cell", op_id), |ui| {
                egui::CollapsingHeader::new("Add Cell")
                    .show(ui, |ui| {
                        crate::code::new_cell::render_new_cell_interface(
                            ui,
                            chidori_state,
                            &mut cell_state,
                            &mut code_theme,
                        );
                    });
            });
        }
    }
    frame.end(ui);
}

pub fn render_plaintext_cell(
    execution_state: &ChidoriState,
    op_id: &OperationId,
    mut ui: &mut Ui,
    state: &mut MutexGuard<CellState>
) {
    let Some(mut cell_holder)= state.cell.as_mut() else {
        return;
    };
    let CellTypes::PlainText(PlainTextCell { text, ..}, _) = &mut cell_holder.cell else { panic!("Must be plain text cell")};
    ui.vertical(|ui| {
        if ui.add(
            egui::TextEdit::multiline(text)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.is_dirty_editor = true;
        }
        render_operation_output(&execution_state, &op_id, ui);
    });
}

pub fn render_template_cell(
    execution_state: &ChidoriState,
    op_id: &OperationId,
    mut ui: &mut Ui,
    state: &mut MutexGuard<CellState>
) {
    let Some(mut cell_holder)= state.cell.as_mut() else {
        return;
    };
    let CellTypes::Template(TemplateCell { name, body , backing_file_reference, ..}, _) = &mut cell_holder.cell else { panic!("Must be template cell")};
    if let Some(name) = name {
        if ui.add(
            egui::TextEdit::singleline(name)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.is_dirty_editor = true;
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
    ui.vertical(|ui| {
        if ui.add(
            egui::TextEdit::multiline(body)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.is_dirty_editor = true;
        }
        render_operation_output(&execution_state, &op_id, ui);
    });
}

pub fn render_prompt_cell(
    execution_state: &mut ChidoriState,
    op_id: &OperationId,
    mut ui: &mut Ui,
    state: &mut MutexGuard<CellState>
) {
    let Some(mut cell_holder) = state.cell.take() else {
        return;
    };
    let CellTypes::Prompt(LLMPromptCell::Chat { name, configuration, req, complete_body, backing_file_reference,  .. }, _) = &mut cell_holder.cell else {panic!("Must be llm prompt cell")};

    if let Some(name) = name {
        if ui.add(
            egui::TextEdit::singleline(name)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.is_dirty_editor = true;
        }
    }

    let (frontmatter, req) = chidori_core::chidori_prompt_format::templating::templates::split_frontmatter(&complete_body).map_err(|e| {
        anyhow::Error::msg(e.to_string())
    }).unwrap_or((String::new(), String::new()));
    let schema = chidori_core::chidori_prompt_format::templating::templates::analyze_referenced_partials(&&req);
    if !schema.is_err() {
        state.json_content = populate_json_content(&schema.unwrap(), Some(&state.json_content));
    }

    ui.horizontal(|ui| {
        egui_label(ui, "Prompt");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if backing_file_reference.is_some() {
                if ui.button("Open File").clicked() {
                    println!("Should open file");
                }
            }
            if ui.button("Open Local Repl").clicked() {
                state.is_repl_open = !state.is_repl_open;
            }
        });
    });

    ui.vertical(|ui| {
        if ui.add(
            egui::TextEdit::multiline(complete_body)
                .id(Id::new(format!("{:?} prompt_editor", op_id)))
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.is_dirty_editor = true;
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
    state.cell = Some(cell_holder);
}

pub fn render_code_gen_cell(
    execution_state: &mut ChidoriState,
    op_id: &OperationId,
    mut ui: &mut Ui,
    state: &mut MutexGuard<CellState>
) {
    let Some(mut cell_holder)= state.cell.take() else {
        return;
    };
    let CellTypes::CodeGen(LLMCodeGenCell { name, req, complete_body, backing_file_reference, .. }, _) = &mut cell_holder.cell else { panic!("Must be code gen cell") };
    if let Some(name) = name {
        if ui.add(
            egui::TextEdit::singleline(name)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.is_dirty_editor = true;
        }
    }
    ui.horizontal(|ui| {
        egui_label(ui, "Code Generation");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if backing_file_reference.is_some() {
                if ui.button("Open File").clicked() {
                    println!("Should open file");
                }
            }
            if ui.button("Open Local Repl").clicked() {
                state.is_repl_open = !state.is_repl_open;
            }
        });
    });
    ui.vertical(|ui| {
        if ui.add(
            egui::TextEdit::multiline(complete_body)
                .code_editor()
                .lock_focus(true)
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(8.0, 8.0))
        ).changed() {
            cell_holder.is_dirty_editor = true;
        }
        ui.add_space(10.0);

        render_operation_output(&execution_state, &op_id, ui);
    });

    state.cell = Some(cell_holder);
}

pub fn render_code_cell(
    chidori_state: &mut ChidoriState,
    theme: &mut CodeTheme,
    op_id: &OperationId,
    mut ui: &mut Ui,
    state: &mut MutexGuard<CellState>
) {
    let Some(mut cell_holder)= state.cell.take() else {
        return;
    };
    let CellTypes::Code(CodeCell { name, source_code, language, backing_file_reference, ..}, _) = &mut cell_holder.cell else { panic!("Mut be cell_holder") };
    if let Some(name) = name {
        if ui.add(
            egui::TextEdit::singleline(name)
                .code_editor()
                .lock_focus(true)
                .margin(Margin::symmetric(8.0, 8.0))
                .desired_width(f32::INFINITY)
        ).changed() {
            cell_holder.is_dirty_editor = true;
        }
    }

    let mut local_language = language.clone();
    let language_string = match language {
        SupportedLanguage::PyO3 => "python",
        SupportedLanguage::Deno => "javascript/typescript"
    };

    let report = match language {
        SupportedLanguage::PyO3 => {
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
                cell_holder.is_dirty_editor = true;
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
                    cell_holder.is_dirty_editor = true;
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
    state.cell = Some(cell_holder);
}

fn render_operation_output(execution_state: &ChidoriState, op_id: &&OperationId, ui: &mut Ui) {
    // if let Some(state) = &execution_state.merged_state_history {
    //     if let Some((exec_id, o)) = state.0.get(op_id) {
    //         if ui.button(format!("View Most Recent Execution")).clicked() {
    //             // TODO: move visualized head of graph to this point
    //         }
    //         if ui.button(format!("Revert To Most Recent Execution")).clicked() {}
    //         ui.push_id((exec_id, op_id), |ui| {
    //             ui.collapsing("Output", |ui| {
    //                 let response = JsonTree::new(format!("{} {} values", exec_id, op_id), &serialized_value_to_json_value(&o.output.clone().unwrap()))
    //                     .show(ui);
    //             });
    //             ui.collapsing("Logs", |ui| {
    //                 egui_logs(ui, &o.stdout);
    //                 egui_logs(ui, &o.stderr);
    //             });
    //         });
    //     }
    // }
} 