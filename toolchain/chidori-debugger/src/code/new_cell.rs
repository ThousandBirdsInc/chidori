use std::sync::MutexGuard;
use crate::application::{CellState, ChidoriState};
use chidori_core::cells::{CellTypes, CodeCell, LLMCodeGenCell, LLMPromptCell, SupportedLanguage, SupportedModelProviders, TemplateCell, TextRange};
use chidori_core::sdk::interactive_chidori_wrapper::CellHolder;
use chidori_core::uuid::Uuid;
use egui::Ui;
use egui_extras::syntax_highlighting::CodeTheme;
use crate::code::cell_rendering::{render_code_cell, render_code_gen_cell, render_prompt_cell, render_template_cell};

pub fn render_new_cell_interface(
    mut ui: &mut Ui,
    mut chidori_state: &mut ChidoriState,
    mut state: &mut MutexGuard<CellState>,
    code_theme: &mut CodeTheme
) {
    if state.cell.is_some() {
        if ui.button("Cancel").clicked() {
            state.cell = None;
            state.is_new_cell_open = false;
        }
    }

    if state.cell.is_none() {
        ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
        if ui.button("Add Code Cell").clicked() {
            let op_id = Uuid::now_v7();
            state.cell = Some(CellHolder {
                cell: CellTypes::Code(CodeCell {
                    backing_file_reference: None,
                    name: None,
                    language: SupportedLanguage::PyO3,
                    source_code: "".to_string(),
                    function_invocation: None,
                }, TextRange::default()),
                op_id,
                is_dirty_editor: false,
            });
        }
        if ui.button("Add Prompt Cell").clicked() {
            let op_id = Uuid::now_v7();
            state.cell = Some(CellHolder {
                cell: CellTypes::Prompt(LLMPromptCell::Chat {
                    backing_file_reference: None,
                    is_function_invocation: false,
                    configuration: Default::default(),
                    name: None,
                    provider: SupportedModelProviders::OpenAI,
                    complete_body: "".to_string(),
                    req: "".to_string(),
                }, TextRange::default()),
                op_id,
                is_dirty_editor: false,
            });
        }
        if ui.button("Add Template Cell").clicked() {
            let op_id = Uuid::now_v7();
            state.cell = Some(CellHolder {
                cell: CellTypes::Template(TemplateCell {
                    backing_file_reference: None,
                    name: None,
                    body: "".to_string(),
                }, TextRange::default()),
                op_id,
                is_dirty_editor: false,
            });
        }
        if ui.button("Add Code Generation Cell").clicked() {
            let op_id = Uuid::now_v7();
            state.cell = Some((CellHolder {
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
                is_dirty_editor: false,
            }));
        }
    }

    let exists_in_current_tree = false;
    let mut temp_cell_created = false;
    if let Some(mut temp_cell) = state.cell.take() {
        let op_id = temp_cell.op_id.clone();
        match &mut temp_cell.cell {
            CellTypes::Code(_, ..) => {
                render_code_cell(
                    &mut chidori_state,
                    code_theme,
                    &op_id,
                    ui,
                    state
                );
            }
            CellTypes::CodeGen(..) => {
                render_code_gen_cell(&mut chidori_state, &op_id, ui, state);
            }
            CellTypes::Prompt(LLMPromptCell::Completion { .. }, _) => {}
            CellTypes::Prompt(LLMPromptCell::Chat { .. }, _) => {
                render_prompt_cell(&mut chidori_state, &op_id, ui, state);
            }
            CellTypes::Template(..) => {
                render_template_cell(&mut chidori_state, &op_id, ui, state);
            }
            CellTypes::PlainText(..) => {
            }
        }

        if ui.button("Save and Push To Graph").clicked() {
            chidori_state.update_cell(temp_cell.clone());
            temp_cell_created = true;
        }
        state.cell = Some(temp_cell);
    }
    if temp_cell_created {
        state.cell = None;
        state.is_new_cell_open = false;
    }
} 