// Common functions for examples
use bevy::{prelude::*, window::PrimaryWindow};
use bevy_cosmic_edit::*;
use egui;
use egui::{Color32, FontFamily, FontId, Frame, Margin, RichText, Ui};
use egui_extras::syntax_highlighting;
use egui_extras::syntax_highlighting::CodeTheme;
use crate::egui_json_tree::value::{BaseValueType, ExpandableType, JsonTreeValue, ToJsonTreeValue};
use chidori_core::cells::{CellTypes, CodeCell, LLMCodeGenCell, LLMEmbeddingCell, LLMPromptCell, MemoryCell, SupportedLanguage, TemplateCell, WebserviceCell};
use chidori_core::execution::primitives::serialized_value::RkyvSerializedValue;

pub fn deselect_editor_on_esc(
    i: Res<ButtonInput<KeyCode>>,
    mut focus: ResMut<FocusedWidget>
) {
    if i.just_pressed(KeyCode::Escape) {
        focus.0 = None;
    }
}

pub fn change_active_editor_sprite(
    mut commands: Commands,
    windows: Query<&Window, With<PrimaryWindow>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut cosmic_edit_query: Query<
        (&mut Sprite, &GlobalTransform, &Visibility, Entity),
        (With<CosmicBuffer>, Without<ReadOnly>),
    >,
    camera_q: Query<(&Camera, &GlobalTransform)>,
) {
    let window = windows.single();
    let (camera, camera_transform) = camera_q.single();
    if buttons.just_pressed(MouseButton::Left) {
        for (sprite, node_transform, visibility, entity) in &mut cosmic_edit_query.iter_mut() {
            if visibility == Visibility::Hidden {
                continue;
            }
            let size = sprite.custom_size.unwrap_or(Vec2::ONE);
            let x_min = node_transform.affine().translation.x - size.x / 2.;
            let y_min = node_transform.affine().translation.y - size.y / 2.;
            let x_max = node_transform.affine().translation.x + size.x / 2.;
            let y_max = node_transform.affine().translation.y + size.y / 2.;
            if let Some(pos) = window.cursor_position() {
                if let Some(pos) = camera.viewport_to_world_2d(camera_transform, pos) {
                    if x_min < pos.x && pos.x < x_max && y_min < pos.y && pos.y < y_max {
                        commands.insert_resource(FocusedWidget(Some(entity)))
                    };
                }
            };
        }
    }
}

pub fn change_active_editor_ui(
    mut commands: Commands,
    mut interaction_query: Query<
        (&Interaction, &CosmicSource),
        (Changed<Interaction>, Without<ReadOnly>),
    >,
) {
    for (interaction, source) in interaction_query.iter_mut() {
        if let Interaction::Pressed = interaction {
            commands.insert_resource(FocusedWidget(Some(source.0)));
        }
    }
}

pub fn print_editor_text(
    text_inputs_q: Query<&CosmicEditor>,
    mut previous_value: Local<Vec<String>>,
) {
    for text_input in text_inputs_q.iter() {
        let current_text: Vec<String> = text_input.with_buffer(|buf| {
            buf.lines
                .iter()
                .map(|bl| bl.text().to_string())
                .collect::<Vec<_>>()
        });
        if current_text == *previous_value {
            return;
        }
        *previous_value = current_text.clone();
    }
}

pub fn bevy_color_to_cosmic(color: bevy::prelude::Color) -> CosmicColor {
    CosmicColor::rgba(
        (color.r() * 255.) as u8,
        (color.g() * 255.) as u8,
        (color.b() * 255.) as u8,
        (color.a() * 255.) as u8,
    )
}

pub fn despawn_screen<T: Component>(to_despawn: Query<Entity, With<T>>, mut commands: Commands) {
    for entity in &to_despawn {
        commands.entity(entity).despawn_recursive();
    }
}

pub fn egui_logs(ui: &mut Ui, value: &Vec<String>) {
    if !value.is_empty() {
        let max_rect = ui.max_rect();
        let clip_rect = egui::Rect::from_min_max(
            max_rect.min,
            max_rect.min + egui::vec2(max_rect.width(), 10000.0), // 50.0 is the height of the clipping area
        );
        ui.set_clip_rect(clip_rect);
        ui.vertical(|ui| {
            ui.label("Logs");
            ui.separator();
            for (key, value) in value.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("{:?}", key));
                    ui.separator();
                    ui.label(value);
                });
            }
        });
    }
}

pub fn egui_rkyv(ui: &mut Ui, value: &RkyvSerializedValue, with_clip: bool) {
    if with_clip {
        let max_rect = ui.max_rect();
        let clip_rect = egui::Rect::from_min_max(
            max_rect.min,
            max_rect.min + egui::vec2(max_rect.width(), 10000.0), // 50.0 is the height of the clipping area
        );
        ui.set_clip_rect(clip_rect);
    }
    match value {
        RkyvSerializedValue::StreamPointer(_) => {}
        RkyvSerializedValue::FunctionPointer(_, _) => {}
        RkyvSerializedValue::Cell(_) => {}
        RkyvSerializedValue::Set(_) => {}
        RkyvSerializedValue::Float(a) => {
            ui.label(format!("{:?}", a));
        }
        RkyvSerializedValue::Number(a) => {
            ui.label(format!("{:?}", a));
        }
        RkyvSerializedValue::String(a) => {
            ui.label(format!("{:?}", a));
        }
        RkyvSerializedValue::Boolean(a) => {
            ui.label(format!("{:?}", a));
        }
        RkyvSerializedValue::Null => {}
        RkyvSerializedValue::Array(a) => {
            ui.vertical(|ui| {
                ui.label("Array");
                ui.separator();
                for (key, value) in a.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{:?}", key));
                        ui.separator();
                        egui_rkyv(ui, value, with_clip);
                    });
                }
            });
        }
        RkyvSerializedValue::Object(o) => {
            ui.vertical(|ui| {
                ui.label("Object");
                ui.separator();
                for (key, value) in o.iter() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{:?}", key));
                        ui.separator();
                        egui_rkyv(ui, value, with_clip);
                    });
                }
            });
        }
    }
}

pub fn egui_label(ui: &mut Ui, text: &str) {
    let frame = egui::Frame::none()
        .inner_margin(Margin::symmetric(1.0, 8.0))
        .fill(egui::Color32::TRANSPARENT) // Optional: to make the frame transparent
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(Color32::from_hex("#8C8C8C").unwrap()));
        });

    // frame.response.interact_rect.expand(egui::Vec2::new(ui.available_width(), 0.0));
}


// pub fn egui_centered_label(ui: &mut Ui, value: &RkyvSerializedValue) {
//     let label_size = ui.fonts().layout_single_line(egui::TextStyle::Body, label_text.to_string()).size;
//
//     let label_pos = egui::Pos2::new(
//         (window_width - label_size.x) / 2.0,
//         (window_height - label_size.y) / 2.0,
//     );
//
//     ui.allocate_ui_at_rect(egui::Rect::from_min_size(label_pos, label_size), |ui| {
//         ui.label(label_text);
//     });
// }


pub fn egui_render_cell_read(ui: &mut Ui, cell: &CellTypes) {
    let theme = CodeTheme::dark();

    match cell {
        CellTypes::Code(CodeCell { name, source_code, language, .. }, _) => {
            render_code_cell(ui, name, source_code, language, &theme);
        }
        CellTypes::CodeGen(LLMCodeGenCell { name, req, .. }, _) => {
            render_text_cell(ui, name, req, "Code Gen Prompt", "md", &theme);
        }
        CellTypes::Prompt(LLMPromptCell::Chat { name, req, .. }, _) => {
            render_text_cell(ui, name, req, "Prompt", "md", &theme);
        }
        CellTypes::Template(TemplateCell { name, body }, _) => {
            render_text_cell(ui, name, body, "Prompt", "", &theme);
        }
        CellTypes::Prompt(LLMPromptCell::Completion { .. }, _) |
        CellTypes::Embedding(LLMEmbeddingCell { .. }, _) |
        CellTypes::Memory(MemoryCell { .. }, _) => {}
    }
}

fn render_code_cell(ui: &mut Ui, name: &Option<String>, source_code: &str, language: &SupportedLanguage, theme: &CodeTheme) {
    let (language_string, syntax_language) = match language {
        SupportedLanguage::PyO3 => ("python", "py"),
        SupportedLanguage::Starlark => ("starlark", "py"),
        SupportedLanguage::Deno => ("javascript/typescript", "js"),
    };

    render_frame(ui, "Code", Some(language_string), name, source_code, theme, syntax_language);
}

fn render_text_cell(ui: &mut Ui, name: &Option<String>, text: &str, label: &str, syntax: &str, theme: &CodeTheme) {
    render_frame(ui, label, None, name, text, theme, syntax);
}

fn render_frame(ui: &mut Ui, label: &str, extra_label: Option<&str>, name: &Option<String>, text: &str, theme: &CodeTheme, syntax: &str) {
    let mut frame = Frame::default()
        .fill(Color32::from_hex("#222222").unwrap())
        .outer_margin(16.0)
        .inner_margin(16.0)
        .rounding(6.0)
        .begin(ui);

    let content_ui = &mut frame.content_ui;
    content_ui.horizontal(|ui| {
        egui_label(ui, label);
        if let Some(extra) = extra_label {
            egui_label(ui, extra);
        }
        if let Some(name) = name {
            egui_label(ui, name);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if ui.button("Open").clicked() {
                println!("Should open file");
            }
        });
    });

    let mut s = text.to_string();
    content_ui.vertical(|ui| {
        let mut layouter = |ui: &egui::Ui, text_string: &str, wrap_width: f32| {
            let mut layout_job =
                egui_extras::syntax_highlighting::highlight(ui.ctx(), &theme, text_string, syntax);
            layout_job.wrap.max_width = wrap_width;

            // Fix font size
            for mut section in &mut layout_job.sections {
                section.format.font_id = egui::FontId::new(14.0, FontFamily::Monospace);
            }

            ui.fonts(|f| f.layout_job(layout_job))
        };

        ui.add(
            egui::TextEdit::multiline(&mut s)
                .font(FontId::new(14.0, FontFamily::Monospace))
                .code_editor()
                .lock_focus(true)
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(8.0, 8.0))
                .layouter(&mut layouter),
        );
        ui.add_space(10.0);
    });

    frame.end(ui);
}




impl ToJsonTreeValue for RkyvSerializedValue {
    fn to_json_tree_value(&self) -> JsonTreeValue {
        match self {
            RkyvSerializedValue::StreamPointer(_) => JsonTreeValue::Base(&"null", BaseValueType::Null),
            RkyvSerializedValue::FunctionPointer(_, _) => JsonTreeValue::Base(&"null", BaseValueType::Null),
            RkyvSerializedValue::Cell(c) => {
                match c {
                    CellTypes::Code(c, _) => { JsonTreeValue::Base(&c.source_code, BaseValueType::String)}
                    CellTypes::CodeGen(c, _) => { JsonTreeValue::Base(&c.req, BaseValueType::String)}
                    CellTypes::Prompt(c, _) => {
                        match c {
                            LLMPromptCell::Chat { req, .. } => { JsonTreeValue::Base(req, BaseValueType::String)}
                            LLMPromptCell::Completion { req, .. } => { JsonTreeValue::Base(req, BaseValueType::String)}
                        }
                    }
                    CellTypes::Embedding(c, _) => { JsonTreeValue::Base(&c.req, BaseValueType::String)}
                    CellTypes::Template(c, _) => { JsonTreeValue::Base(&c.body, BaseValueType::String)}
                    CellTypes::Memory(c, _) => { JsonTreeValue::Base(&c.embedding_function, BaseValueType::String)}
                }
            },
            RkyvSerializedValue::Set(set) => JsonTreeValue::Expandable(
                set.iter()
                    .enumerate()
                    .map(|(idx, elem)| (idx.to_string(), elem as &dyn ToJsonTreeValue))
                    .collect(),
                ExpandableType::Array,
            ),
            RkyvSerializedValue::Float(n) => JsonTreeValue::Base(n, BaseValueType::Number),
            RkyvSerializedValue::Boolean(b) => JsonTreeValue::Base(b, BaseValueType::Bool),
            RkyvSerializedValue::Number(n) => JsonTreeValue::Base(n, BaseValueType::Number),
            RkyvSerializedValue::String(s) => JsonTreeValue::Base(s, BaseValueType::String),
            RkyvSerializedValue::Null => JsonTreeValue::Base(&"null", BaseValueType::Null),
            RkyvSerializedValue::Array(arr) => JsonTreeValue::Expandable(
                arr.iter()
                    .enumerate()
                    .map(|(idx, elem)| (idx.to_string(), elem as &dyn ToJsonTreeValue))
                    .collect(),
                ExpandableType::Array,
            ),
            RkyvSerializedValue::Object(obj) => JsonTreeValue::Expandable(
                obj.iter()
                    .map(|(key, val)| (key.to_owned(), val as &dyn ToJsonTreeValue))
                    .collect(),
                ExpandableType::Object,
            ),
        }
    }

    fn is_expandable(&self) -> bool {
        matches!(
            self,
            RkyvSerializedValue::Object(_) | RkyvSerializedValue::Set(_) | RkyvSerializedValue::Array(_)
        )
    }
}



use regex::Regex;

fn traverse_rkyv_serialized_value(
    value: &RkyvSerializedValue,
    pattern: &Regex,
    path: Vec<String>,
    results: &mut Vec<(String, Vec<String>)>,
) {
    match value {
        RkyvSerializedValue::String(s) => {
            if pattern.is_match(s) {
                results.push((s.clone(), path));
            }
        }
        RkyvSerializedValue::Array(arr) => {
            for (index, item) in arr.iter().enumerate() {
                let mut new_path = path.clone();
                new_path.push(index.to_string());
                traverse_rkyv_serialized_value(item, pattern, new_path, results);
            }
        }
        RkyvSerializedValue::Object(obj) => {
            for (key, val) in obj.iter() {
                let mut new_path = path.clone();
                new_path.push(key.clone());
                traverse_rkyv_serialized_value(val, pattern, new_path, results);
            }
        }
        RkyvSerializedValue::Set(set) => {
            for (index, item) in set.iter().enumerate() {
                let mut new_path = path.clone();
                new_path.push(format!("Set[{}]", index));
                traverse_rkyv_serialized_value(item, pattern, new_path, results);
            }
        }
        _ => {} // Other variants don't contain nested RkyvSerializedValues or Strings
    }
}

// Helper function to initiate the traversal
pub fn find_matching_strings(
    value: &RkyvSerializedValue,
    pattern: &str,
) -> Vec<(String, Vec<String>)> {
    let regex = Regex::new(pattern).expect("Invalid regex pattern");
    let mut results = Vec::new();
    traverse_rkyv_serialized_value(value, &regex, Vec::new(), &mut results);
    results
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn test_simple_string_match() {
        let value = RkyvSerializedValue::String("Hello, world!".to_string());
        let results = find_matching_strings(&value, r"^Hello");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "Hello, world!");
        assert_eq!(results[0].1, Vec::<String>::new());
    }

    #[test]
    fn test_array_match() {
        let value = RkyvSerializedValue::Array(vec![
            RkyvSerializedValue::String("foo".to_string()),
            RkyvSerializedValue::String("bar".to_string()),
            RkyvSerializedValue::String("baz".to_string()),
        ]);
        let results = find_matching_strings(&value, r"^ba");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "bar");
        assert_eq!(results[0].1, vec!["1"]);
        assert_eq!(results[1].0, "baz");
        assert_eq!(results[1].1, vec!["2"]);
    }

    #[test]
    fn test_nested_object_match() {
        let mut inner_obj = HashMap::new();
        inner_obj.insert("key".to_string(), RkyvSerializedValue::String("value".to_string()));

        let mut outer_obj = HashMap::new();
        outer_obj.insert("nested".to_string(), RkyvSerializedValue::Object(inner_obj));
        outer_obj.insert("sibling".to_string(), RkyvSerializedValue::String("hello".to_string()));

        let value = RkyvSerializedValue::Object(outer_obj);
        let results = find_matching_strings(&value, r"value|hello");
        assert_eq!(results.len(), 2);
        assert!(results.contains(&("value".to_string(), vec!["nested".to_string(), "key".to_string()])));
        assert!(results.contains(&("hello".to_string(), vec!["sibling".to_string()])));
    }

    #[test]
    fn test_set_match() {
        let mut set = HashSet::new();
        set.insert(RkyvSerializedValue::String("apple".to_string()));
        set.insert(RkyvSerializedValue::String("banana".to_string()));
        set.insert(RkyvSerializedValue::String("cherry".to_string()));

        let value = RkyvSerializedValue::Set(set);
        let results = find_matching_strings(&value, r"^[ab]");
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|(s, p)| s == "apple" && p[0].starts_with("Set[") && p[0].ends_with("]")));
        assert!(results.iter().any(|(s, p)| s == "banana" && p[0].starts_with("Set[") && p[0].ends_with("]")));
    }

    #[test]
    fn test_no_match() {
        let value = RkyvSerializedValue::Object(HashMap::from([
            ("key1".to_string(), RkyvSerializedValue::Number(42)),
            ("key2".to_string(), RkyvSerializedValue::Boolean(true)),
        ]));
        let results = find_matching_strings(&value, r"nonexistent");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_complex_nested_structure() {
        let mut inner_set = HashSet::new();
        inner_set.insert(RkyvSerializedValue::String("set_item1".to_string()));
        inner_set.insert(RkyvSerializedValue::String("set_item2".to_string()));

        let mut inner_obj = HashMap::new();
        inner_obj.insert("inner_key".to_string(), RkyvSerializedValue::String("inner_value".to_string()));

        let value = RkyvSerializedValue::Object(HashMap::from([
            ("array".to_string(), RkyvSerializedValue::Array(vec![
                RkyvSerializedValue::String("array_item1".to_string()),
                RkyvSerializedValue::String("array_item2".to_string()),
            ])),
            ("set".to_string(), RkyvSerializedValue::Set(inner_set)),
            ("object".to_string(), RkyvSerializedValue::Object(inner_obj)),
        ]));

        let results = find_matching_strings(&value, r"item|inner");
        assert_eq!(results.len(), 5);
        assert!(results.contains(&("array_item1".to_string(), vec!["array".to_string(), "0".to_string()])));
        assert!(results.contains(&("array_item2".to_string(), vec!["array".to_string(), "1".to_string()])));
        assert!(results.iter().any(|(s, p)| s == "set_item1" && p[0] == "set" && p[1].starts_with("Set[") && p[1].ends_with("]")));
        assert!(results.iter().any(|(s, p)| s == "set_item2" && p[0] == "set" && p[1].starts_with("Set[") && p[1].ends_with("]")));
        assert!(results.contains(&("inner_value".to_string(), vec!["object".to_string(), "inner_key".to_string()])));
    }
}