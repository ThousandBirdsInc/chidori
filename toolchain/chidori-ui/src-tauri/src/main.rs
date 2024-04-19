// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dagre;

use std::collections::HashSet;
use std::ops::{Deref, DerefMut};
use rusqlite::{Connection, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::sync::mpsc::TryRecvError;
use std::{panic, process, thread};
use std::cell::RefCell;
use std::time::Duration;
use petgraph::graph::DiGraph;
use petgraph::prelude::{DiGraphMap, NodeIndex};
use serde_json::json;
use tauri::{AppHandle, Manager};
use chidori_core::sdk::entry::{Chidori, EventsFromRuntime, InstancedEnvironment, SharedState, UserInteractionMessage, CellHolder};
use chidori_core::cells::CellTypes;
use crate::dagre::DagreLayout;
use crate::dagre::{Data, Edge, Node, Rect};
use notify_debouncer_full::{notify::{Watcher, RecommendedWatcher, RecursiveMode}, new_debouncer, DebounceEventResult, Debouncer, FileIdCache, FileIdMap};
use serde::Serializer;
use tauri::async_runtime::JoinHandle;
use ts_rs::TS;
use chidori_core::execution::execution::execution_graph::{ExecutionNodeId, MergedStateHistory, Serialize};
use chidori_core::execution::primitives::identifiers::{DependencyReference, OperationId};
use chidori_core::utils::telemetry::{trace_event_to_string, TraceEvents};

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
}


fn main() {
    // let config: DiskMap<String, String> = DiskMap::open_new("/tmp/chidori.db").unwrap();

    tauri::Builder::default()
        .setup(|app| {
            let (trace_event_sender, trace_event_receiver) = std::sync::mpsc::channel();
            let (runtime_event_sender, runtime_event_receiver) = std::sync::mpsc::channel();
            app.manage(InternalState {
                chidori: Arc::new(Mutex::new(Chidori::new_with_events(trace_event_sender, runtime_event_sender))),
                watched_path: Mutex::new(None),
                background_thread: Mutex::new(None),
                file_watch: Mutex::new(None),
            });

            // take_hook() returns the default hook in case when a custom one is not set
            let orig_hook = panic::take_hook();
            panic::set_hook(Box::new(move |panic_info| {
                // invoke the default handler and exit the process
                orig_hook(panic_info);
                process::exit(1);
            }));

            let handle = app.handle();
            let internal_state = handle.state::<InternalState>();
            let mut background_thread_guard = internal_state.background_thread
                .lock()
                .expect("Failed to lock background_thread");
            let chidori = internal_state.chidori.clone();
            *background_thread_guard = Some(tauri::async_runtime::spawn(async move {
                let mut instance = {
                    let mut chidori_guard = chidori.lock().unwrap();
                    let mut instance = chidori_guard.get_instance().unwrap();
                    // Drop the lock on chidori to avoid deadlock
                    drop(chidori_guard);
                    instance
                };
                instance.run().await;
            }));


            let handle = app.handle();
            tauri::async_runtime::spawn(async move {
                loop {
                    match runtime_event_receiver.try_recv() {
                        Ok(msg) => {
                            // println!("Forwarding message to client: {:?}", &msg);
                            match msg {
                                EventsFromRuntime::ExecutionGraphUpdated(state) => {
                                    handle.emit_all("sync:executionGraphState", Some(maintain_execution_graph(&state))).expect("Failed to emit");
                                }
                                EventsFromRuntime::ExecutionStateChange(state) => {
                                    handle.emit_all("sync:observeState", Some(state)).expect("Failed to emit");
                                }
                                EventsFromRuntime::DefinitionGraphUpdated(state) => {
                                    handle.emit_all("sync:definitionGraphState", Some(maintain_definition_graph(&state))).expect("Failed to emit");
                                }
                                EventsFromRuntime::CellsUpdated(state) => {
                                    handle.emit_all("sync:cellsState", Some(state)).expect("Failed to emit");
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

            // app.unlisten(id);

            let handle = app.handle();
            let env = handle.state::<InternalState>();
            let env_clone = env.chidori.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    match trace_event_receiver.recv() {
                        Ok(msg) => {
                            // println!("Received: {:?}", &msg);
                            handle.emit_all("execution:events", Some(trace_event_to_string(msg)));
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
fn move_state_view_to_id(id: ExecutionNodeId, app_handle: AppHandle) -> Result<(), String> {
    let env = app_handle.state::<InternalState>();
    let env = env.chidori.lock().unwrap();
    env.handle_user_action(UserInteractionMessage::RevertToState(Some(id))).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn play(app_handle: AppHandle) -> Result<(), String>  {
    let env = app_handle.state::<InternalState>();
    let env = env.chidori.lock().unwrap();
    env.handle_user_action(UserInteractionMessage::Play).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn pause(app_handle: AppHandle) -> Result<(), String>  {
    let env = app_handle.state::<InternalState>();
    let env = env.chidori.lock().unwrap();
    env.handle_user_action(UserInteractionMessage::Pause).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn set_execution_id(app_handle: AppHandle, id: (usize, usize)) -> Result<(), String>  {
    let env = app_handle.state::<InternalState>();
    let env = env.chidori.lock().unwrap();
    env.handle_user_action(UserInteractionMessage::RevertToState(Some(id))).map_err(|e| e.to_string())?;
    Ok(())
}


#[tauri::command]
fn load_and_watch_directory(path: String, app_handle: AppHandle) -> Result<(), String>  {
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
        chidori_guard.load_md_directory(Path::new(&path)).map_err(|e| e.to_string())?;
    }
    Ok(())
}


fn maintain_execution_graph(elements: &Vec<(ExecutionNodeId, ExecutionNodeId)>) -> String {
    let mut nodes = HashSet::new();
    let mut edges = HashSet::new();

    for (a, b) in elements {
        nodes.insert(Node {
            id: format!("{:?}-{:?}", a.0, a.1),
            node_type: "default".to_string(),
            data: Data {
                label: format!("{:?}-{:?}", a.0, a.1),
            },
            position: Rect { x: 0.0, y: 0.0, layer: 0, width: 5.0, height: 5.0 },
        });
        nodes.insert(Node {
            id: format!("{:?}-{:?}", b.0, b.1),
            node_type: "default".to_string(),
            data: Data {
                label: format!("{:?}-{:?}", b.0, b.1),
            },
            position: Rect { x: 0.0, y: 0.0, layer: 0, width: 5.0, height: 5.0 },
        });
    }

    for edge in elements {
        edges.insert(Edge {
            id: format!("{:?}-{:?}", edge.0, edge.1),
            edge_type: "default".to_string(),
            source: format!("{:?}-{:?}", edge.0.0, edge.0.1),
            target: format!("{:?}-{:?}", edge.1.0, edge.1.1),
            label: format!("{:?}-{:?}", edge.0.0, edge.0.1),
        });
    }
    let mut nodes: Vec<Node> = nodes.into_iter().collect();
    let mut edges: Vec<Edge> = edges.into_iter().collect();

    let mut graph = DiGraph::new();

    // Add nodes to the graph and keep track of their indices
    let mut index_map = std::collections::HashMap::new();
    for mut node in &mut nodes {
        let id = node.id.clone();
        let index = graph.add_node(RefCell::new(node));
        index_map.insert(id, index);
    }

    // Add edges to the graph using the indices
    for mut edge in &mut edges {
        let source: &NodeIndex = index_map.get(&edge.source.clone()).unwrap();
        let target: &NodeIndex = index_map.get(&edge.target.clone()).unwrap();
        graph.add_edge(source.clone(), target.clone(), RefCell::new(edge));
    }

    let mut dagre_layout = DagreLayout::new(&mut graph, crate::dagre::Direction::TopToBottom, 20.0, 10.0, 50.0);
    dagre_layout.layout();

    json!({ "nodes": nodes, "edges": edges }).to_string()
}


fn maintain_definition_graph(graph: &Vec<(OperationId, OperationId, Vec<DependencyReference>)>) -> String {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for (node_a, node_b, weight) in graph.iter() {
        nodes.push(Node {
            id: node_a.to_string(),
            node_type: "default".to_string(),
            data: Data {
                label: node_a.to_string(),
            },
            position: Rect { x: 0.0, y: 0.0, layer: 0, width: 200.0, height: 80.0 },
        });
        nodes.push(Node {
            id: node_b.to_string(),
            node_type: "default".to_string(),
            data: Data {
                label: node_b.to_string(),
            },
            position: Rect { x: 0.0, y: 0.0, layer: 0, width: 200.0, height: 80.0 },
        });
    }

    for (node_a, node_b, weight) in graph.iter() {
        edges.push(Edge {
            id: format!("{}-{}", node_a, node_b),
            edge_type: "default".to_string(),
            source: node_a.to_string(),
            target: node_b.to_string(),
            label: format!("{:?}", weight)
        });
    }

    let mut graph = DiGraph::new();

    // Add nodes to the graph and keep track of their indices
    let mut index_map = std::collections::HashMap::new();
    for node in &mut nodes {
        let id = node.id.clone();
        let index = graph.add_node(RefCell::new(node));
        index_map.insert(id, index);
    }

    // Add edges to the graph using the indices
    for edge in &mut edges {
        let start_index: &NodeIndex = index_map.get(&edge.source.clone()).unwrap();
        let end_index: &NodeIndex = index_map.get(&edge.target.clone()).unwrap();
        graph.add_edge(end_index.clone(), start_index.clone(), RefCell::new(edge));
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
    }
}