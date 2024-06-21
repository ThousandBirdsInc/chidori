// Common functions for examples
use bevy::{prelude::*, window::PrimaryWindow};
use bevy_cosmic_edit::*;
use egui;
use egui::{Color32, FontFamily, Margin, RichText, Ui};
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
    let mut theme = egui_extras::syntax_highlighting::CodeTheme::dark();
    match cell {
        CellTypes::Code(CodeCell { name, source_code, language, .. }, _) => {
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
            let mut frame = egui::Frame::default().fill(Color32::from_hex("#222222").unwrap()).outer_margin(16.0).inner_margin(16.0).rounding(6.0).begin(ui);
            {
                let mut ui = &mut frame.content_ui;
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
                });
            }
            frame.end(ui);
        }
        CellTypes::CodeGen(LLMCodeGenCell { name, req, .. }, _) => {
            let mut s = req.clone();
            let mut frame = egui::Frame::default().fill(Color32::from_hex("#222222").unwrap()).outer_margin(16.0).inner_margin(16.0).rounding(6.0).begin(ui);
            {
                let mut ui = &mut frame.content_ui;
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
                });
            }
            frame.end(ui);
        }
        CellTypes::Prompt(LLMPromptCell::Completion { .. }, _) => {}
        CellTypes::Prompt(LLMPromptCell::Chat { name, configuration, req, .. }, _) => {
            let mut layouter = |ui: &egui::Ui, text_string: &str, wrap_width: f32| {
                let mut layout_job =
                    egui_extras::syntax_highlighting::highlight(ui.ctx(), &theme, text_string, "md");
                layout_job.wrap.max_width = wrap_width;

                // Fix font size
                for mut section in &mut layout_job.sections {
                    section.format.font_id = egui::FontId::new(14.0, FontFamily::Monospace);
                }

                ui.fonts(|f| f.layout_job(layout_job))
            };
            let mut s = req.clone();
            let mut frame = egui::Frame::default().fill(Color32::from_hex("#222222").unwrap()).outer_margin(16.0).inner_margin(16.0).begin(ui);
            {
                let mut ui = &mut frame.content_ui;
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
                            .layouter(&mut layouter),
                    );
                    ui.add_space(10.0);
                });
            }
            frame.end(ui);
        }
        CellTypes::Embedding(LLMEmbeddingCell { .. }, _) => {}
        CellTypes::Web(WebserviceCell { name, configuration, .. }, _) => {
            let mut s = configuration.clone();
            let mut frame = egui::Frame::default().fill(Color32::from_hex("#222222").unwrap()).outer_margin(16.0).inner_margin(16.0).begin(ui);
            {
                let mut ui = &mut frame.content_ui;
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
                });
            }
            frame.end(ui);
        }
        CellTypes::Template(TemplateCell { name, body }, _) => {
            let mut s = body.clone();
            let mut frame = egui::Frame::default().fill(Color32::from_hex("#222222").unwrap()).outer_margin(16.0).inner_margin(16.0).begin(ui);
            {
                let mut ui = &mut frame.content_ui;
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
                });
            }
            frame.end(ui);
        }
        CellTypes::Memory(MemoryCell { .. }, _) => {}
    }
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
                    CellTypes::Web(c, _) => { JsonTreeValue::Base(&c.configuration, BaseValueType::String)}
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
