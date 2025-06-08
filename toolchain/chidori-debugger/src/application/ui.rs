use bevy::prelude::{EventReader, Local, Res, ResMut};
use egui::{Color32, FontFamily, Frame, Id, Margin, Response, Vec2b};
use egui::panel::TopBottomSide;

use crate::{CurrentTheme, MenuAction};
use crate::vendored::bevy_egui::EguiContexts;
use crate::accidental::tokio_tasks;

use super::types::{ChidoriState, EguiTree, TreeBehavior};

// Example content constants
const EXAMPLES_CORE1: &str = include_str!("../../examples/core1_simple_math/core.md");
const EXAMPLES_CORE2: &str = include_str!("../../examples/core2_marshalling/core.md");
const EXAMPLES_CORE3: &str = include_str!("../../examples/core3_function_invocations/core.md");
const EXAMPLES_CORE4: &str = include_str!("../../examples/core4_async_function_invocations/core.md");
const EXAMPLES_CORE5: &str = include_str!("../../examples/core5_prompts_invoked_as_functions/core.md");
const EXAMPLES_CORE6: &str = include_str!("../../examples/core6_prompts_leveraging_function_calling/core.md");
const EXAMPLES_CORE7: &str = include_str!("../../examples/core7_rag_stateful_memory_cells/core.md");
const EXAMPLES_CORE8: &str = include_str!("../../examples/core8_prompt_code_generation_and_execution/core.md");
const EXAMPLES_CORE9: &str = include_str!("../../examples/core9_multi_agent_simulation/core.md");
const EXAMPLES_CORE10: &str = include_str!("../../examples/core10_concurrency/core.md");
const EXAMPLES_CORE11: &str = include_str!("../../examples/core11_hono/core.md");
const EXAMPLES_CORE12: &str = include_str!("../../examples/core12_dependency_management/core.md");

fn with_cursor(res: Response) -> Response {
    if res.hovered() {
        res.ctx.output_mut(|p| {
            p.cursor_icon = egui::CursorIcon::PointingHand;
        });
    }
    res
}

pub fn initial_save_notebook_dialog(
    mut contexts: EguiContexts,
    mut egui_tree: ResMut<EguiTree>,
    runtime: ResMut<tokio_tasks::TokioTasksRuntime>,
    mut internal_state: ResMut<ChidoriState>,
    mut theme: Res<CurrentTheme>,
    mut notebook_name_state: Local<Option<String>>,
) {
    if !internal_state.application_state_is_displaying_save_dialog {
        return;
    }
    let mut contexts1 = &mut contexts;
    let mut internal_state1 = &mut internal_state;

    // Initialize the Local state if it's None
    if notebook_name_state.is_none() {
        *notebook_name_state = Some(String::new());
    }

    let mut saving_notebook_name = Some(String::new());
    egui::CentralPanel::default()
        .frame(
            Frame::default()
                .fill(theme.theme.card)
                .stroke(theme.theme.card_border)
                .inner_margin(16.0)
                .outer_margin(200.0)
                .rounding(theme.theme.radius as f32),
        )
        .show(contexts1.ctx_mut(), |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("Save Notebook");
                ui.add_space(8.0);

                // Get a mutable reference to the String inside the Option
                {
                    let mut notebook_name = notebook_name_state.as_mut().unwrap();
                    let response = ui.add(
                        egui::TextEdit::singleline(notebook_name)
                            .hint_text("Enter notebook name...")
                            .desired_width(300.0)
                    );
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if !notebook_name.trim().is_empty() {
                            internal_state1.save_notebook();
                        }
                    }
                }

                ui.add_space(16.0);

                // Buttons row
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        saving_notebook_name = Some(String::new());
                        internal_state1.application_state_is_displaying_save_dialog = false;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let notebook_name = notebook_name_state.as_mut().unwrap();
                        let save_button = ui.add_enabled(
                            !notebook_name.trim().is_empty(),
                            egui::Button::new("Save")
                        );

                        if save_button.clicked() {
                            // internal_state1.save_notebook(notebook_name, &runtime);
                            saving_notebook_name = Some(String::new());
                            internal_state1.application_state_is_displaying_save_dialog = false;
                        }
                    });
                });
            });
        });
}

pub fn handle_menu_actions(
    mut menu_events: EventReader<MenuAction>,
    mut contexts: EguiContexts,
    mut egui_tree: ResMut<EguiTree>,
    runtime: ResMut<tokio_tasks::TokioTasksRuntime>,
    mut internal_state: ResMut<ChidoriState>,
    mut theme: Res<CurrentTheme>,
    mut displayed_example_desc: Local<Option<(String, String, String)>>
) {
    use bevy_utils::tracing::{error, info};
    
    for event in menu_events.read() {
        match event {
            MenuAction::NewProject => {}
            MenuAction::OpenProject => {
                internal_state.application_state_is_displaying_example_modal = false;
                // let sender = self.text_channel.0.clone();
                runtime.spawn_background_task(|mut ctx| async move {
                    let task = rfd::AsyncFileDialog::new().pick_folder();
                    let folder = task.await;
                    if let Some(folder) = folder {
                        let path = folder.path().to_string_lossy().to_string();
                        ctx.run_on_main_thread(move |ctx| {
                            if let Some(mut internal_state) =
                                ctx.world.get_resource_mut::<ChidoriState>()
                            {
                                match internal_state.load_and_watch_directory(path) {
                                    Ok(()) => {
                                        // Directory loaded and watched successfully
                                        println!("Directory loaded and being watched successfully");
                                    },
                                    Err(e) => {
                                        // Handle the error
                                        eprintln!("Error loading and watching directory: {}", e);
                                    }
                                }
                            }
                        })
                            .await;
                    }
                });
            }
            MenuAction::Save => {
                internal_state.save_notebook();
            }
            _ => {}
        }
    }
}

pub fn root_gui(
    mut contexts: EguiContexts,
    mut egui_tree: ResMut<EguiTree>,
    runtime: ResMut<tokio_tasks::TokioTasksRuntime>,
    mut internal_state: ResMut<ChidoriState>,
    mut theme: Res<CurrentTheme>,
    mut displayed_example_desc: Local<Option<(String, String, String)>>
) {
    use bevy_utils::tracing::{error, info};
    use chidori_core::sdk::chidori_runtime_instance::PlaybackState;
    
    if internal_state.application_state_is_displaying_example_modal {
        let mut contexts1 = &mut contexts;
        let mut internal_state1 = &mut internal_state;
        egui::CentralPanel::default()
            .frame(
                Frame::default()
                    .fill(theme.theme.card)
                    .stroke(theme.theme.card_border)
                    .inner_margin(16.0)
                    .outer_margin(100.0)
                    .rounding(theme.theme.radius as f32),
            )
            .show(contexts1.ctx_mut(), |ui| {
                ui.add_space(12.0);
                let mut frame = egui::Frame::default().inner_margin(16.0).begin(ui);
                {
                    let mut ui = &mut frame.content_ui;
                    ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 12.0);
                    // Add widgets inside the frame
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label("New Notebook:");
                            let res = with_cursor(ui.button("Create New Notebook"));
                            if res.clicked() {
                                internal_state1.application_state_is_displaying_example_modal = false;
                                internal_state1.application_state_is_displaying_save_dialog = true;
                            }
                            ui.add_space(16.0);
                            ui.label("Load Existing Project");
                            let res = with_cursor(ui.button("Load From Folder"));
                            if res.clicked() {
                                internal_state1.application_state_is_displaying_example_modal = false;
                                runtime.spawn_background_task(|mut ctx| async move {
                                    let task = rfd::AsyncFileDialog::new().pick_folder();
                                    let folder = task.await;
                                    if let Some(folder) = folder {
                                        let path = folder.path().to_string_lossy().to_string();
                                        ctx.run_on_main_thread(move |ctx| {
                                            if let Some(mut internal_state) =
                                                ctx.world.get_resource_mut::<ChidoriState>()
                                            {
                                                let mut watched_path = internal_state.watched_path.get_mut().unwrap();
                                                *watched_path = Some(path.clone());
                                                match internal_state.load_and_watch_directory(path) {
                                                    Ok(()) => {
                                                        // Directory loaded and watched successfully
                                                        info!("Directory loaded and being watched successfully");
                                                    },
                                                    Err(e) => {
                                                        // Handle the error
                                                        error!("Error loading and watching directory: {}", e);
                                                    }
                                                }
                                            }
                                        })
                                            .await;
                                    }
                                });
                            }
                            ui.add_space(16.0);
                            ui.label("Load Example:");
                            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
                            let buttons_text_load = vec![
                                ("Core 1: Simple Math", EXAMPLES_CORE1, "Demonstrates simple arithmetic between cells, and that values can be passed between Python and JavaScript runtimes."),
                                ("Core 2: Marshalling Values", EXAMPLES_CORE2, "All of the types that we can successfully pass between runtimes and that are preserved by our execution engine."),
                                ("Core 3: Invoking Functions", EXAMPLES_CORE3, "Demonstrates what function execution looks like when using Chidori. Explore how states are preserved and the ability to revert between them with re-execution."),
                                ("Core 4: Invoking Async Functions", EXAMPLES_CORE4, "Function invocations default to being asynchronous."),
                                ("Core 5: Invoking Prompts as Functions", EXAMPLES_CORE5, "We treat prompts as first class resources, this demonstrates how prompts are invokable as functions."),
                                (
                                    "Core 6: Using Function Calling in Prompts",
                                    EXAMPLES_CORE6, "Prompts may import functions and invoke those in order to accomplish their instructions."

                                ),
                                ("Core 7: Chat With PDF Clone", EXAMPLES_CORE7, "Cells preserve their internal state, we provide a specialized API for embeddings which demonstrates this behavior, exposing functions for interacting with that state."),
                                (
                                    "Core 8: Anthropic Artifacts Clone",
                                    EXAMPLES_CORE8, "Chidori is designed for L4-L5 agents, new behaviors can be generated on the fly via code generation."
                                ),
                                ("Core 9: Multi-Agent Social Experiment", EXAMPLES_CORE9, "desc"),
                                ("Core 10: Demonstrating Our Execution Concurrency", EXAMPLES_CORE10, "desc"),
                                ("Core 11: Hono Web Service", EXAMPLES_CORE11, "desc"),
                                ("Core 12: Dependency Management", EXAMPLES_CORE12, "desc"),
                            ];

                            let available_height = ui.available_height();
                            egui::ScrollArea::vertical()
                                .auto_shrink(Vec2b::new(true, false))
                                .min_scrolled_height(400.0).show(ui, |ui| {
                                let mut frame = egui::Frame::default().outer_margin(Margin {
                                    left: 0.0,
                                    right: 40.0,
                                    top: 0.0,
                                    bottom: 0.0,
                                }).rounding(6.0).begin(ui);
                                {
                                    ui.set_height(available_height);
                                    let mut ui = &mut frame.content_ui;
                                    let mut is_a_button_hovered = false;
                                    for button in buttons_text_load {
                                        let res = with_cursor(ui.button(button.0));
                                        if res.hovered() {
                                            is_a_button_hovered = true;
                                            *displayed_example_desc = Some((button.0.to_string(), button.1.to_string(), button.2.to_string()));
                                        }
                                        if res.clicked() {
                                            internal_state1.load_string(button.1);
                                        }
                                    }
                                    if is_a_button_hovered == false {
                                        *displayed_example_desc = None;
                                    }
                                }
                                frame.end(ui);
                            });
                        });

                        if let Some((title, code, desc)) = &*displayed_example_desc {
                            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                                let mut frame = egui::Frame::default().outer_margin(Margin::symmetric(64.0, 0.0)).inner_margin(64.0).rounding(6.0).begin(ui);
                                {
                                    let mut ui = &mut frame.content_ui;
                                    let mut code_mut = code.to_string();
                                    ui.set_max_width(800.0);
                                    ui.label(title);
                                    ui.add_space(16.0);
                                    ui.label(desc);
                                    ui.add_space(16.0);
                                    egui::ScrollArea::new([false, true]) // Horizontal: false, Vertical: true
                                        .max_width(800.0)
                                        .max_height(600.0)
                                        .show(ui, |ui| {
                                            ui.add(
                                                egui::TextEdit::multiline(&mut code_mut)
                                                    .font(egui::FontId::new(14.0, FontFamily::Monospace))
                                                    .code_editor()
                                                    .lock_focus(true)
                                                    .desired_width(f32::INFINITY)
                                                    .margin(Margin::symmetric(8.0, 8.0))

                                            );
                                        });
                                }
                                frame.end(ui);
                            });
                        }

                        // ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        //     ui.add_space(ui.available_height() / 2.0 - 256.0); // Center vertically
                        //     egui::Image::new(egui::include_image!("../assets/images/tblogo-white.png"))
                        //         .fit_to_exact_size(vec2(512.0, 512.0))
                        //         .rounding(5.0)
                        //         .ui(ui);
                        // });
                    });
                }
                frame.end(ui);
            });
    } else {
        egui::CentralPanel::default()
            .frame(egui::Frame::default()
                .fill(Color32::TRANSPARENT)
                .outer_margin(Margin {
                    left: 0.0,
                    right: 0.0,
                    top: 48.0,
                    bottom: 0.0,
                }))
            .show(contexts.ctx_mut(), |ui| {
                let mut behavior = TreeBehavior {
                    current_theme: &theme
                };
                egui_tree.tree.ui(&mut behavior, ui);
            });
    }

    egui::TopBottomPanel::new(TopBottomSide::Top, Id::new("top_panel")).show(
        contexts.ctx_mut(),
        |ui| {
            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
            // ui.text_edit_multiline(&mut text);
            // a simple button opening the dialog
            let mut frame = egui::Frame::default()
                .inner_margin(Margin::symmetric(8.0, 8.0))
                .begin(ui);
            {
                let mut ui = &mut frame.content_ui;
                ui.horizontal(|ui| {
                    ui.style_mut().spacing.item_spacing = egui::vec2(32.0, 8.0);
                    // if with_cursor(ui.button("Save")).clicked() {
                    //     internal_state.reset();
                    // }
                    // if with_cursor(ui.button("Reset")).clicked() {
                    //     internal_state.reset();
                    // }
                    ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);

                    if !internal_state.application_state_is_displaying_example_modal {
                        match internal_state.current_playback_state {
                            PlaybackState::Paused => {
                                if with_cursor(ui.button("⏵")).clicked() {
                                    internal_state.play();
                                }
                                if with_cursor(ui.button("⏭")).clicked() {
                                    internal_state.step();
                                }
                            }
                            PlaybackState::Step => {
                                if with_cursor(ui.button("⏵️")).clicked() {
                                    internal_state.play();
                                }
                                if with_cursor(ui.button("⏸")).clicked() {
                                    internal_state.pause();
                                }
                            }
                            PlaybackState::Running => {
                                if with_cursor(ui.button("⏸")).clicked() {
                                    internal_state.pause();
                                }
                            }
                        }
                    }

                    ui.add_space(8.0);

                    // let mut my_f32 = 0.0;
                    // ui.add(egui::Slider::new(&mut my_f32, 0.0..=100.0).text("Rate Limit func/s"));

                    // ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    //     if with_cursor(ui.button("UI Debug Mode")).clicked() {
                    //         internal_state.debug_mode = !internal_state.debug_mode;
                    //     }
                    // });
                });

            }
            frame.end(ui);
        },
    );
} 