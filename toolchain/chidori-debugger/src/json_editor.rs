use std::str::FromStr;
// Common functions for examples
use bevy::{prelude::*, window::PrimaryWindow};
use egui;
use egui::{Color32, FontFamily, FontId, Frame, Id, Margin, RichText, TextEdit, Ui, vec2};
use egui::text::CCursor;
use egui::text_selection::CCursorRange;
use egui_extras::syntax_highlighting;
use egui_extras::syntax_highlighting::CodeTheme;
use serde_json::Value;
use egui_json_tree::{
    delimiters::ExpandableDelimiter,
    pointer::JsonPointerSegment,
    render::{
        DefaultRender, RenderBaseValueContext, RenderContext, RenderExpandableDelimiterContext,
        RenderPropertyContext,
    },
    DefaultExpand, JsonTree,
};
use chidori_core::cells::{CellTypes, CodeCell, LLMCodeGenCell, LLMEmbeddingCell, LLMPromptCell, MemoryCell, SupportedLanguage, TemplateCell, WebserviceCell};
use chidori_core::execution::primitives::serialized_value::RkyvSerializedValue;

pub struct JsonEditorExample<'a> {
    value: &'a mut Value,
    id: Id,
}


#[derive(Clone, Default, Debug)]
struct EditorState {
    edit_object_key: Option<EditObjectKeyState>,
    edit_value: Option<EditValueState>,
}

#[derive(Clone, Debug)]
struct EditObjectKeyState {
    key: String,
    object_pointer: String,
    new_key_input: String,
    is_new_key: bool,
}

#[derive(Clone, Debug)]
struct EditValueState {
    pointer: String,
    new_value_input: String,
}

impl<'a> JsonEditorExample<'a> {
    pub fn new(value: &'a mut Value, id: Id) -> Self {
        Self { value, id }
    }

    fn show_editor(&mut self, ui: &mut Ui, context: RenderContext<'_, '_, Value>) {
        let mut mutable_state = ui.data_mut(|d| d.get_temp::<EditorState>(self.id).unwrap_or_default());

        match &mut mutable_state {
            EditorState { edit_object_key: Some(state), .. } => {
                Self::show_edit_object_key(ui, self.value, context, state, self.id);
                if ui.data(|d| d.get_temp::<EditorState>(self.id)).is_some() {
                    mutable_state.edit_object_key = Some(state.clone());
                    ui.data_mut(|d| d.insert_temp(self.id, mutable_state));
                }
            }
            EditorState { edit_value: Some(state), .. } => {
                Self::show_edit_value(ui, self.value, context, state, self.id);
                if ui.data(|d| d.get_temp::<EditorState>(self.id)).is_some() {
                    mutable_state.edit_value = Some(state.clone());
                    ui.data_mut(|d| d.insert_temp(self.id, mutable_state));
                }
            }
            _ => {
                self.show_with_context_menus(ui, context);
            }
        }

    }

    fn show_edit_object_key(
        ui: &mut Ui,
        document: &mut Value,
        context: RenderContext<Value>,
        state: &mut EditObjectKeyState,
        id: Id
    ) {
        if let RenderContext::Property(context) = &context {
            if let JsonPointerSegment::Key(key) = context.property {
                if key == state.key
                    && context
                    .pointer
                    .parent()
                    .map(|parent| parent.to_json_pointer_string())
                    .is_some_and(|object_pointer| object_pointer == state.object_pointer)
                {
                    Self::show_text_edit_with_focus(ui, &mut state.new_key_input);

                    ui.add_space(5.0);

                    let valid_key = state.key == state.new_key_input
                        || document
                        .pointer(&state.object_pointer)
                        .and_then(|v| v.as_object())
                        .is_some_and(|obj| !obj.contains_key(&state.new_key_input));

                    ui.add_enabled_ui(valid_key, |ui| {
                        if ui.small_button("✅").clicked() {
                            if let Some(obj) = document.pointer_mut(&state.object_pointer).and_then(|v| v.as_object_mut()) {
                                if let Some(value) = obj.remove(&state.key) {
                                    obj.insert(state.new_key_input.clone(), value);
                                }
                            }
                            ui.data_mut(|d| d.remove::<EditorState>(id));
                        }
                    });

                    ui.add_space(5.0);

                    if ui.small_button("❌").clicked() {
                        ui.data_mut(|d| d.remove::<EditorState>(id));
                    }
                    return;
                }
            }
        }
        context.render_default(ui);
    }

    fn show_edit_value(
        ui: &mut Ui,
        document: &mut Value,
        context: RenderContext<Value>,
        state: &mut EditValueState,
        id: Id
    ) {
        if let RenderContext::BaseValue(context) = &context {
            if state.pointer == context.pointer.to_json_pointer_string() {
                Self::show_text_edit_with_focus(ui, &mut state.new_value_input);

                ui.add_space(5.0);

                if ui.small_button("✅").clicked() {
                    let new_value = serde_json::Value::String(state.new_value_input.to_string());
                    if let Some(value) = document.pointer_mut(&state.pointer) {
                        *value = new_value;
                    }
                    ui.data_mut(|d| d.remove::<EditorState>(id));
                }

                ui.add_space(5.0);

                if ui.small_button("❌").clicked() {
                    ui.data_mut(|d| d.remove::<EditorState>(id));
                }
                return;
            }
        }
        context.render_default(ui);
    }

    fn show_with_context_menus(&mut self, ui: &mut Ui, context: RenderContext<Value>) {
        match context {
            RenderContext::Property(context) => {
                self.show_property_context_menu(ui, context);
            }
            RenderContext::BaseValue(context) => {
                self.show_value_context_menu(ui, context);
            }
            RenderContext::ExpandableDelimiter(context) => {
                self.show_expandable_delimiter_context_menu(ui, context);
            }
        };
    }

    fn show_property_context_menu(
        &mut self,
        ui: &mut Ui,
        context: RenderPropertyContext<'_, '_, Value>,
    ) {
        context
            .render_default(ui)
            .on_hover_cursor(egui::CursorIcon::ContextMenu)
            .context_menu(|ui| {
                if context.value.is_object() && ui.button("Add to object").clicked() {
                    self.add_to_object(ui, &context.pointer.to_json_pointer_string());
                    ui.close_menu();
                }

                if context.value.is_array() && ui.button("Add to array").clicked() {
                    self.add_to_array(&context.pointer.to_json_pointer_string());
                    ui.close_menu();
                }

                if let Some(parent) = context.pointer.parent() {
                    if let JsonPointerSegment::Key(key) = &context.property {
                        if ui.button("Edit key").clicked() {
                            let state = EditObjectKeyState {
                                key: key.to_string(),
                                object_pointer: parent.to_json_pointer_string(),
                                new_key_input: key.to_string(),
                                is_new_key: false,
                            };
                            ui.data_mut(|d| d.insert_temp(self.id, EditorState { edit_object_key: Some(state), edit_value: None }));
                            ui.close_menu()
                        }
                    }

                    if ui.button("Delete").clicked() {
                        match context.property {
                            JsonPointerSegment::Key(key) => {
                                self.delete_from_object(&parent.to_json_pointer_string(), key);
                            }
                            JsonPointerSegment::Index(idx) => {
                                self.delete_from_array(&parent.to_json_pointer_string(), idx);
                            }
                        }
                        ui.close_menu();
                    }
                }
            });
    }

    fn show_value_context_menu(
        &mut self,
        ui: &mut Ui,
        context: RenderBaseValueContext<'_, '_, Value>,
    ) {
        context
            .render_default(ui)
            .on_hover_cursor(egui::CursorIcon::ContextMenu)
            .context_menu(|ui| {
                if ui.button("Edit value").clicked() {
                    let state = if let serde_json::Value::String(s) = context.value {
                        EditValueState {
                            pointer: context.pointer.to_json_pointer_string(),
                            new_value_input: s.clone(),
                        }
                    } else {
                        EditValueState {
                            pointer: context.pointer.to_json_pointer_string(),
                            new_value_input: context.value.to_string(),
                        }
                    };
                    ui.data_mut(|d| d.insert_temp(self.id, EditorState { edit_value: Some(state), edit_object_key: None }));
                    ui.close_menu();
                }

                match (context.pointer.parent(), context.pointer.last()) {
                    (Some(parent), Some(JsonPointerSegment::Key(key))) => {
                        if ui.button("Delete").clicked() {
                            self.delete_from_object(&parent.to_json_pointer_string(), key);
                            ui.close_menu();
                        }
                    }
                    (Some(parent), Some(JsonPointerSegment::Index(idx))) => {
                        if ui.button("Delete").clicked() {
                            self.delete_from_array(&parent.to_json_pointer_string(), *idx);
                            ui.close_menu();
                        }
                    }
                    _ => {}
                };
            });
    }

    fn show_expandable_delimiter_context_menu(
        &mut self,
        ui: &mut Ui,
        context: RenderExpandableDelimiterContext<'_, '_, Value>,
    ) {
        match context.delimiter {
            ExpandableDelimiter::OpeningArray => {
                context
                    .render_default(ui)
                    .on_hover_cursor(egui::CursorIcon::ContextMenu)
                    .context_menu(|ui| {
                        if ui.button("Add to array").clicked() {
                            self.add_to_array(&context.pointer.to_json_pointer_string());
                            ui.close_menu();
                        }
                    });
            }
            ExpandableDelimiter::OpeningObject => {
                context
                    .render_default(ui)
                    .on_hover_cursor(egui::CursorIcon::ContextMenu)
                    .context_menu(|ui| {
                        if ui.button("Add to object").clicked() {
                            self.add_to_object(ui, &context.pointer.to_json_pointer_string());
                            ui.close_menu();
                        }
                    });
            }
            _ => {
                context.render_default(ui);
            }
        };
    }

    fn show_text_edit_with_focus(ui: &mut Ui, input: &mut String) {
        let text_edit_output = TextEdit::singleline(input)
            .code_editor()
            .margin(Margin::symmetric(2.0, 0.0))
            .clip_text(false)
            .desired_width(0.0)
            .min_size(vec2(10.0, 2.0))
            .show(ui);

        // maintain focus until confirmation
        // let text_edit_id = text_edit_output.response.id;
        // if ui.data_mut(|d| { d.remove_temp::<bool>(text_edit_id).unwrap_or(true) }) {
        //     if let Some(mut text_edit_state) = TextEdit::load_state(ui.ctx(), text_edit_id) {
        //         text_edit_state
        //             .cursor
        //             .set_char_range(Some(CCursorRange::two(
        //                 CCursor::new(0),
        //                 CCursor::new(input.len()),
        //             )));
        //         text_edit_state.store(ui.ctx(), text_edit_id);
        //         ui.ctx().memory_mut(|mem| mem.request_focus(text_edit_id));
        //     }
        //     ui.data_mut(|d| d.insert_temp(text_edit_id, false));
        // }
    }

    fn delete_from_array(&mut self, array_pointer: &str, idx: usize) {
        if let Some(arr) = self.value.pointer_mut(array_pointer).and_then(|value| value.as_array_mut()) {
            arr.remove(idx);
        }
    }

    fn delete_from_object(&mut self, object_pointer: &str, key: &str) {
        if let Some(obj) = self.value.pointer_mut(object_pointer).and_then(|value| value.as_object_mut()) {
            obj.remove(key);
        }
    }

    fn add_to_object(&mut self, ui: &mut Ui, pointer: &str) {
        if let Some(obj) = self.value.pointer_mut(pointer).and_then(|value| value.as_object_mut()) {
            let mut counter = 0;
            let mut new_key = "new_key".to_string();

            while obj.contains_key(&new_key) {
                counter += 1;
                new_key = format!("new_key_{counter}");
            }

            obj.insert(new_key.clone(), Value::Null);

            let state = EditObjectKeyState {
                key: new_key.clone(),
                object_pointer: pointer.to_string(),
                new_key_input: new_key,
                is_new_key: true,
            };
            ui.data_mut(|d| d.insert_temp(self.id, EditorState { edit_object_key: Some(state), edit_value: None }));
        }
    }

    fn add_to_array(&mut self, pointer: &str) {
        if let Some(arr) = self.value.pointer_mut(pointer).and_then(|value| value.as_array_mut()) {
            arr.push(Value::Null);
        }
    }
}

pub trait Show {
    fn title(&self) -> &'static str;
    fn show(&mut self, ui: &mut Ui);
}

impl<'a> Show for JsonEditorExample<'a> {
    fn title(&self) -> &'static str {
        "JSON Editor Example"
    }

    fn show(&mut self, ui: &mut Ui) {
        JsonTree::new(self.title(), &self.value.clone())
            .abbreviate_root(true)
            .default_expand(DefaultExpand::All)
            .on_render(|ui, context| self.show_editor(ui, context))
            .show(ui);
    }
}
