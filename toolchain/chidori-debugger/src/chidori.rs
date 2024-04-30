use std::collections::HashSet;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::sync::mpsc::TryRecvError;
use std::{panic, process, thread};
use std::cell::RefCell;
use std::time::Duration;
use bevy::app::{App, Startup, Update};
use bevy::prelude::{Commands, default, in_state, IntoSystemConfigs, OnEnter, OnExit, ResMut, Resource};
use bevy_cosmic_edit::{CosmicEditPlugin, CosmicFontConfig};
use petgraph::graph::DiGraph;
use petgraph::prelude::{DiGraphMap, NodeIndex};
use serde_json::json;
use chidori_core::sdk::entry::{Chidori, EventsFromRuntime, InstancedEnvironment, SharedState, UserInteractionMessage, CellHolder};
use chidori_core::cells::CellTypes;
use notify_debouncer_full::{notify::{Watcher, RecommendedWatcher, RecursiveMode}, new_debouncer, DebounceEventResult, Debouncer, FileIdCache, FileIdMap};
use serde::Serializer;
use chidori_core::execution::execution::execution_graph::{ExecutionNodeId, MergedStateHistory, Serialize};
use chidori_core::execution::primitives::identifiers::{DependencyReference, OperationId};
use chidori_core::utils::telemetry::{trace_event_to_string, TraceEvents};
use crate::{GameState, tokio_tasks};
use crate::util::{change_active_editor_ui, deselect_editor_on_esc, despawn_screen, print_editor_text};

use bevy_egui::{EguiPlugin, egui, EguiContexts};
use std::collections::HashMap;
use bevy_egui::egui::{Button, Id, Widget};
use bevy_egui::egui::panel::Side;
use chidori_core::tokio::task::JoinHandle;

const EXAMPLES_CORE1: &str = include_str!("../../chidori-core/examples/core1_simple_math/core.md");
const EXAMPLES_CORE2: &str = include_str!("../../chidori-core/examples/core2_marshalling/core.md");
const EXAMPLES_CORE3: &str = include_str!("../../chidori-core/examples/core3_function_invocations/core.md");
const EXAMPLES_CORE4: &str = include_str!("../../chidori-core/examples/core4_async_function_invocations/core.md");
const EXAMPLES_CORE5: &str = include_str!("../../chidori-core/examples/core5_prompts_invoked_as_functions/core.md");
const EXAMPLES_CORE6: &str = include_str!("../../chidori-core/examples/core6_prompts_leveraging_function_calling/core.md");
const EXAMPLES_CORE7: &str = include_str!("../../chidori-core/examples/core7_rag_stateful_memory_cells/core.md");
const EXAMPLES_CORE8: &str = include_str!("../../chidori-core/examples/core8_prompt_code_generation_and_execution/core.md");
const EXAMPLES_CORE9: &str = include_str!("../../chidori-core/examples/core9_multi_agent_simulation/core.md");


#[derive(Resource)]
pub struct ChidoriTraceEvents {
    pub inner: Vec<TraceEvents>
}

#[derive(Resource)]
pub struct ChidoriExecutionGraph {
    pub inner: Vec<(ExecutionNodeId, ExecutionNodeId)>
}

#[derive(Resource)]
pub struct ChidoriDefinitionGraph {
    pub inner: Vec<(OperationId, OperationId, Vec<DependencyReference>)>
}

#[derive(Resource)]
pub struct ChidoriExecutionState {
    pub inner: Option<MergedStateHistory>
}

#[derive(Resource)]
pub struct ChidoriCells {
    pub inner: Vec<CellHolder>
}

#[derive(Resource)]
struct InternalState {
    watched_path: Mutex<Option<String>>,
    file_watch: Mutex<Option<Debouncer<RecommendedWatcher, FileIdMap>>>,
    background_thread: Mutex<Option<JoinHandle<()>>>,
    chidori: Arc<Mutex<Chidori>>,
}

impl InternalState {
    pub fn get_loaded_path(&self) -> String {
        let env = self.chidori.lock().unwrap();
        if env.loaded_path.is_none() {
            return "".to_string();
        }
        env.loaded_path.as_ref().unwrap().to_string()
    }

    pub fn move_state_view_to_id(&self, id: ExecutionNodeId) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::RevertToState(Some(id))).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn play(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::Play).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn pause(&self) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::Pause).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn set_execution_id(&self, id: (usize, usize)) -> anyhow::Result<(), String> {
        let env = self.chidori.lock().unwrap();
        env.handle_user_action(UserInteractionMessage::RevertToState(Some(id))).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_string(&self, file: &str) -> anyhow::Result<(), String> {
        let chidori = self.chidori.clone();
        {
            let mut chidori_guard = chidori.lock().expect("Failed to lock chidori");
            chidori_guard.load_md_string(file);
        }
        Ok(())
    }


    pub fn load_and_watch_directory(&self, path: String) -> anyhow::Result<(), String> {
        let chidori = self.chidori.clone();
        let mut file_watch_guard = self.file_watch.lock().expect("Failed to lock file_watch");

        // Initialize the watcher and set up the event handler within a single block to avoid cloning `path` multiple times.
        let watcher_chidori = chidori.clone();
        let watcher_path = path.clone();
        let mut debouncer = new_debouncer(Duration::from_millis(200), None, move |result: DebounceEventResult| {
            match result {
                Ok(events) => events.iter().for_each(|event| {}),
                Err(errors) => errors.iter().for_each(|error| {}),
            }
            let path_buf = PathBuf::from(&watcher_path);
            let mut chidori_guard = watcher_chidori.lock().expect("Failed to lock chidori");
            chidori_guard.load_md_directory(&path_buf);
        }).unwrap();

        // Watch the directory for changes. Since `path` has not been moved, we can reuse it here.
        debouncer.watcher().watch(Path::new(&path), RecursiveMode::Recursive).expect("Failed to watch directory");
        debouncer.cache().add_root(Path::new(&path), RecursiveMode::Recursive);

        // Replace the old watcher with the new one.
        *file_watch_guard = Some(debouncer);

        {
            let mut chidori_guard = chidori.lock().expect("Failed to lock chidori");
            dbg!("Loading directory");
            chidori_guard.load_md_directory(Path::new(&path)).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn setup(
    mut commands: Commands,
    runtime: ResMut<tokio_tasks::TokioTasksRuntime>
) {
    commands.insert_resource(ChidoriTraceEvents {
        inner: vec![],
    });
    commands.insert_resource(ChidoriExecutionGraph {
        inner: vec![],
    });
    commands.insert_resource(ChidoriDefinitionGraph {
        inner: vec![],
    });
    commands.insert_resource(ChidoriExecutionState {
        inner: None,
    });
    commands.insert_resource(ChidoriCells {
        inner: vec![],
    });


    let (trace_event_sender, trace_event_receiver) = std::sync::mpsc::channel();
    let (runtime_event_sender, runtime_event_receiver) = std::sync::mpsc::channel();
    let mut internal_state = InternalState {
        chidori: Arc::new(Mutex::new(Chidori::new_with_events(trace_event_sender, runtime_event_sender))),
        watched_path: Mutex::new(None),
        background_thread: Mutex::new(None),
        file_watch: Mutex::new(None),
    };

    {
        let mut background_thread_guard = internal_state.background_thread
            .lock()
            .expect("Failed to lock background_thread");
        let chidori = internal_state.chidori.clone();
        *background_thread_guard = Some(runtime.spawn_background_task(|mut ctx| async move {
            loop {
                // Create an instance within the loop
                let mut instance = {
                    let mut chidori_guard = chidori.lock().unwrap();
                    let instance = chidori_guard.get_instance().unwrap();
                    drop(chidori_guard); // Drop the lock on chidori to avoid deadlock
                    instance
                };

                // tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                // TODO: this indicates that its definitely something to do with between each crate
                // Try running the instance
                let result = instance.run().await;

                match result {
                    Ok(_) => {
                        // If the instance runs successfully, break out of the loop
                        println!("Instance ran successfully.");
                        break;
                    },
                    Err(e) => {
                        // Log the error and prepare to retry
                        println!("Error occurred: {}, retrying...", e);
                        // The loop will continue, creating and running a new instance
                    }
                }
            }
        }));

    }


    runtime.spawn_background_task(|mut ctx| async move {
        loop {
            match runtime_event_receiver.try_recv() {
                Ok(msg) => {
                    match msg {
                        EventsFromRuntime::ExecutionGraphUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriExecutionGraph>() {
                                    s.inner = state;
                                }
                            }).await;
                        }
                        EventsFromRuntime::ExecutionStateChange(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriExecutionState>() {
                                    s.inner = Some(state);
                                }
                            }).await;
                        }
                        EventsFromRuntime::DefinitionGraphUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriDefinitionGraph>() {
                                    s.inner = state;
                                }
                            }).await;
                        }
                        EventsFromRuntime::CellsUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriCells>() {
                                    s.inner = state;
                                }
                            }).await;
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    match e {
                        TryRecvError::Empty => {}
                        TryRecvError::Disconnected => {
                            break;
                        }
                    }
                }
            }
        }
    });

    runtime.spawn_background_task(|mut ctx| async move {
        loop {
            match trace_event_receiver.recv() {
                Ok(msg) => {
                    println!("Received: {:?}", &msg);
                    // handle.emit_all("execution:events", Some(trace_event_to_string(msg)));
                    ctx.run_on_main_thread(move |ctx| {
                        if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriTraceEvents>() {
                            s.inner.push(msg);
                        }
                    }).await;
                }
                Err(_) => {
                    println!("Channel closed");
                    break;
                }
            }
        }
    });

    commands.insert_resource(internal_state);
}


pub fn update_gui(
    mut commands: Commands,
    mut contexts: EguiContexts,
    runtime: ResMut<tokio_tasks::TokioTasksRuntime>,
    internal_state: ResMut<InternalState>
) {
    egui::SidePanel::new(Side::Right, Id::new("right_panel")).show(contexts.ctx_mut(), |ui| {
        ui.style_mut().spacing.item_spacing = bevy_egui::egui::vec2(16.0, 16.0);
        // ui.text_edit_multiline(&mut text);
        // a simple button opening the dialog
        let mut frame = egui::Frame::default().inner_margin(16.0).begin(ui);
        {
            let mut ui = &mut frame.content_ui;
            ui.horizontal(|ui| {
                if ui.button("Play").clicked() {
                    internal_state.play();
                }
                if ui.button("Pause").clicked() {
                    internal_state.pause();
                }
            });

            if ui.button("📂 Open project directory").clicked() {
                // let sender = self.text_channel.0.clone();
                runtime.spawn_background_task(|mut ctx| async move {
                    let task = rfd::AsyncFileDialog::new().pick_file();
                    let file = task.await;
                    if let Some(file) = file {
                        let text = file.read().await;
                        // let _ = sender.send(String::from_utf8_lossy(&text).to_string());
                        // ctx.request_repaint();
                    }
                });
            }

            let mut frame = egui::Frame::default().inner_margin(16.0).begin(ui);
            {
                let mut ui = &mut frame.content_ui;
                ui.label("Examples");
                // Add widgets inside the frame
                ui.vertical(|ui| {
                    if ui.button("Core 1: Simple Math").clicked() {
                        internal_state.load_string(EXAMPLES_CORE1);
                    }
                    if ui.button("Core 2: Marshalling").clicked() {
                        internal_state.load_string(EXAMPLES_CORE2);
                    }
                    if ui.button("Core 3: Function Invocations").clicked() {
                        internal_state.load_string(EXAMPLES_CORE3);
                    }
                    if ui.button("Core 4: Async Function Invocations").clicked() {
                        internal_state.load_string(EXAMPLES_CORE4);
                    }
                    if ui.button("Core 5: Prompts Invoked as Functions").clicked() {
                        internal_state.load_string(EXAMPLES_CORE5);
                    }
                    if ui.button("Core 6: Prompts Leveraging Function Calling").clicked() {
                        internal_state.load_string(EXAMPLES_CORE6);
                    }
                    if ui.button("Core 7: Rag Stateful Memory Cells").clicked() {
                        internal_state.load_string(EXAMPLES_CORE7);
                    }
                    if ui.button("Core 8: Prompt Code Generation and Execution").clicked() {
                        internal_state.load_string(EXAMPLES_CORE8);
                    }
                    if ui.button("Core 9: Multi-Agent Simulation").clicked() {
                        internal_state.load_string(EXAMPLES_CORE9);
                    }
                });
            }
            frame.end(ui);


            if ui.button("💾 Save text to file").clicked() {
                let task = rfd::AsyncFileDialog::new().save_file();
                // let contents = self.sample_text.clone();
                runtime.spawn_background_task(|mut ctx|async move {
                    let file = task.await;
                    if let Some(file) = file {
                        // _ = file.write(contents.as_bytes()).await;
                    }
                });
            }

        }
        frame.end(ui);
    });
}


pub fn chidori_plugin(app: &mut App) {
    app
        .add_systems(Update, update_gui)
        .add_systems(Startup, setup);
}
