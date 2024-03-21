// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dagre;

use diskmap::DiskMap;
use std::ops::{Deref, DerefMut};
use rusqlite::{Connection, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use petgraph::graph::DiGraph;
use petgraph::prelude::NodeIndex;
use serde_json::json;
use tauri::{AppHandle, Manager};
use chidori_core::sdk::entry::{Chidori, EventsFromRuntime, InstancedEnvironment, SharedState, UserInteractionMessage};
use chidori_core::cells::CellTypes;
use crate::dagre::DagreLayout;
use crate::dagre::{Data, Edge, Node, Rect};
use notify_debouncer_full::{notify::{Watcher, RecommendedWatcher, RecursiveMode}, new_debouncer, DebounceEventResult, Debouncer, FileIdCache, FileIdMap};
use tauri::async_runtime::JoinHandle;
use ts_rs::TS;
use chidori_core::execution::execution::execution_graph::ExecutionNodeId;

// the payload type must implement `Serialize` and `Clone`.
#[derive(Clone, serde::Serialize)]
struct Payload {
    message: String,
}

/// Whenever a change to this object is made it's synced with the frontend
/// This has typescript types generated for it automatically in ../src/lib/types
#[derive( TS, serde::Serialize, serde::Deserialize, Debug, PartialEq, Clone, )]
#[ts(export, export_to = "../src/lib/types/")]
struct ObservedState {
    counter: usize,
}

impl ObservedState {
    fn new() -> Self {
        Self {
            counter: 0,
        }
    }
}

struct InternalState {
    watched_path: Mutex<Option<String>>,
    file_watch: Mutex<Option<Debouncer<RecommendedWatcher, FileIdMap>>>,
    background_thread: Mutex<Option<JoinHandle<()>>>,
    chidori: Arc<Mutex<Chidori>>,
    observed_state: Arc<Mutex<ObservedState>>,
}

impl InternalState {
    fn update_observed_state(&mut self, state: ObservedState) {
        let mut observed_state = self.observed_state.lock().unwrap();
        *observed_state = state;
    }
}


fn main() {
    let config: DiskMap<String, String> = DiskMap::open_new("/tmp/chidori.db").unwrap();

    tauri::Builder::default()
        .setup(|app| {
            let (trace_event_sender, trace_event_receiver) = std::sync::mpsc::channel();
            let (runtime_event_sender, runtime_event_receiver) = std::sync::mpsc::channel();
            app.manage(InternalState {
                chidori: Arc::new(Mutex::new(Chidori::new_with_events(trace_event_sender, runtime_event_sender))),
                watched_path: Mutex::new(None),
                background_thread: Mutex::new(None),
                file_watch: Mutex::new(None),
                observed_state: Arc::new(Mutex::new(ObservedState::new())),
            });

            let handle = app.handle();
            let internal_state = handle.state::<InternalState>();
            let mut background_thread_guard = internal_state.background_thread.lock().expect("Failed to lock background_thread");
            let chidori = internal_state.chidori.clone();
            *background_thread_guard = Some(tauri::async_runtime::spawn(async move {
                let mut chidori_guard = chidori.lock().unwrap();
                let mut instance = chidori_guard.get_instance().unwrap();
                // Drop the lock on chidori
                drop(chidori_guard);
                instance.run();
            }));


            let handle = app.handle();
            tauri::async_runtime::spawn(async move {
                loop {
                    match runtime_event_receiver.recv() {
                        Ok(msg) => {
                            println!("Runtime event: {:?}", &msg);
                            match msg {
                                EventsFromRuntime::ExecutionGraphUpdated(state) => {
                                    handle.emit_all("sync:executionGraphState", Some(maintain_execution_graph(state)));
                                }
                                EventsFromRuntime::ExecutionStateChange(state) => {
                                    handle.emit_all("sync:observeState", Some(state));
                                }
                                EventsFromRuntime::CellsUpdated(state) => {
                                    handle.emit_all("sync:cellsState", Some(state));
                                }
                                _ => {}
                            }
                        }
                        Err(_) => {
                            println!("Channel closed");
                            break;
                        }
                    }
                }
            });

            // let id = app.listen_global("execution:run", move |event| {
            //     let mut env = env_clone.lock().unwrap();
            //     if let Ok(mut instance) = env.get_instance() {
            //         tauri::async_runtime::spawn(async move {
            //             instance.run();
            //         });
            //     }
            // });

            // unlisten to the event using the `id` returned on the `listen_global` function
            // a `once_global` API is also exposed on the `App` struct
            // app.unlisten(id);

            let handle = app.handle();
            let env = handle.state::<InternalState>();
            let env_clone = env.chidori.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    match trace_event_receiver.recv() {
                        Ok(msg) => {
                            // println!("Received: {:?}", &msg);
                            handle.emit_all("execution:events", Some(msg));
                        }
                        Err(_) => {
                            println!("Channel closed");
                            break;
                        }
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            play,
            pause,
            load_and_watch_directory,
            move_state_view_to_id,
            get_graph_state,
            get_loaded_path,
            set_execution_id
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
fn get_loaded_path(app_handle: AppHandle) -> String {
    let env = app_handle.state::<InternalState>();
    let env = env.chidori.lock().unwrap();
    if env.loaded_path.is_none() {
        return "".to_string();
    }
    env.loaded_path.as_ref().unwrap().to_string()
}

#[tauri::command]
fn move_state_view_to_id(id: ExecutionNodeId, app_handle: AppHandle) {
    let env = app_handle.state::<InternalState>();
    let env = env.chidori.lock().unwrap();
    env.handle_user_action(UserInteractionMessage::RevertToState(Some(id)));
}

#[tauri::command]
fn play(app_handle: AppHandle) {
    let env = app_handle.state::<InternalState>();
    let env = env.chidori.lock().unwrap();
    env.handle_user_action(UserInteractionMessage::Play);
}

#[tauri::command]
fn pause(app_handle: AppHandle) {
    let env = app_handle.state::<InternalState>();
    let env = env.chidori.lock().unwrap();
    env.handle_user_action(UserInteractionMessage::Pause);
}

#[tauri::command]
fn set_execution_id(app_handle: AppHandle, id: (usize, usize)) {
    let env = app_handle.state::<InternalState>();
    let env = env.chidori.lock().unwrap();
    env.handle_user_action(UserInteractionMessage::RevertToState(Some(id)));
}


#[tauri::command]
fn load_and_watch_directory(path: String, app_handle: AppHandle) {
    let internal_state = app_handle.state::<InternalState>();
    let chidori = internal_state.chidori.clone();
    let mut file_watch_guard = internal_state.file_watch.lock().expect("Failed to lock file_watch");

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
        chidori_guard.load_md_directory(Path::new(&path));
    }
}

fn maintain_execution_graph(elements: Vec<(ExecutionNodeId, ExecutionNodeId)>) -> String {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    for (a, b) in &elements {
        nodes.push(Node {
            id: format!("{:?}-{:?}", a.0, a.1),
            node_type: "default".to_string(),
            data: Data {
                label: format!("{:?}-{:?}", a.0, a.1),
            },
            position: Rect { x: 0.0, y: 0.0, layer: 0, width: 5.0, height: 5.0 },
        });
        nodes.push(Node {
            id: format!("{:?}-{:?}", b.0, b.1),
            node_type: "default".to_string(),
            data: Data {
                label: format!("{:?}-{:?}", b.0, b.1),
            },
            position: Rect { x: 0.0, y: 0.0, layer: 0, width: 5.0, height: 5.0 },
        });
    }

    for edge in &elements {
        edges.push(Edge {
            id: format!("{:?}-{:?}", edge.0, edge.1),
            edge_type: "default".to_string(),
            source: format!("{:?}-{:?}", edge.0.0, edge.0.1),
            target: format!("{:?}-{:?}", edge.1.0, edge.1.1),
            label: format!("{:?}-{:?}", edge.0.0, edge.0.1),
        });
    }

    let mut graph = DiGraph::new();

    // Add nodes to the graph and keep track of their indices
    let mut index_map = std::collections::HashMap::new();
    for node in &mut nodes {
        let id = node.id.clone();
        let index = graph.add_node(node);
        index_map.insert(id, index);
    }

    // Add edges to the graph using the indices
    for edge in &mut edges {
        let start_index: &NodeIndex = index_map.get(&edge.source.clone()).unwrap();
        let end_index: &NodeIndex = index_map.get(&edge.target.clone()).unwrap();
        graph.add_edge(end_index.clone(), start_index.clone(), edge);
    }

    let mut dagre_layout = DagreLayout::new(&mut graph, crate::dagre::Direction::TopToBottom, 20.0, 10.0, 50.0);
    dagre_layout.layout();

    json!({ "nodes": nodes, "edges": edges }).to_string()
}

#[tauri::command]
fn get_graph_state(app_handle: AppHandle) -> String {
    let env = app_handle.state::<InternalState>();
    let mut ee = env.chidori.lock().unwrap();
    let env = ee.get_instance();
    if env.is_err() {
        return "".to_string();
    }
    let env = env.unwrap();

    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    let graph = &env.execution_head_latest_state.get_dependency_graph();
    for node in graph.nodes() {
        let op = env.execution_head_latest_state.operation_by_id.get(&node);
        nodes.push(Node {
            id: node.to_string(),
            node_type: "default".to_string(),
            data: Data {
                label: node.to_string(),
            },
            position: Rect { x: 0.0, y: 0.0, layer: 0, width: 200.0, height: 80.0 },
        });
    }

    for edge in graph.all_edges() {
        edges.push(Edge {
            id: format!("{}-{}", edge.0, edge.1),
            edge_type: "default".to_string(),
            source: edge.0.to_string(),
            target: edge.1.to_string(),
            label: edge.0.to_string(),
        });
    }

    let mut graph = DiGraph::new();

    // Add nodes to the graph and keep track of their indices
    let mut index_map = std::collections::HashMap::new();
    for node in &mut nodes {
        let id = node.id.clone();
        let index = graph.add_node(node);
        index_map.insert(id, index);
    }

    // Add edges to the graph using the indices
    for edge in &mut edges {
        let start_index: &NodeIndex = index_map.get(&edge.source.clone()).unwrap();
        let end_index: &NodeIndex = index_map.get(&edge.target.clone()).unwrap();
        graph.add_edge(end_index.clone(), start_index.clone(), edge);
    }

    let mut dagre_layout = DagreLayout::new(&mut graph, crate::dagre::Direction::TopToBottom, 20.0, 10.0, 50.0);
    dagre_layout.layout();

    json!({ "nodes": nodes, "edges": edges }).to_string()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_observed_state() {
        let state = ObservedState::new();
        assert_eq!(state.counter, 0);
    }
}