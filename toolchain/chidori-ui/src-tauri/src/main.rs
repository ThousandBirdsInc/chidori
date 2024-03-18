// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dagre;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use petgraph::graph::DiGraph;
use petgraph::prelude::NodeIndex;
use serde_json::json;
use tauri::{AppHandle, Manager};
use chidori_core::sdk::entry::Chidori;
use crate::dagre::DagreLayout;
use crate::dagre::{Data, Edge, Node, Rect};

// the payload type must implement `Serialize` and `Clone`.
#[derive(Clone, serde::Serialize)]
struct Payload {
    message: String,
}

struct InteralState(Arc<Mutex<Chidori>>);

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let (sender, receiver) = std::sync::mpsc::channel();
            app.manage(InteralState(Arc::new(Mutex::new(Chidori::new_with_events(sender)))));

            let handle = app.handle();
            let env = handle.state::<InteralState>();
            let env_clone = env.0.clone();
            let id = app.listen_global("execution:run", move |event| {
                let mut env = env_clone.lock().unwrap();
                if let Ok(mut instance) = env.get_instance() {
                    tauri::async_runtime::spawn(async move {
                        instance.run();
                    });
                }
            });

            // unlisten to the event using the `id` returned on the `listen_global` function
            // a `once_global` API is also exposed on the `App` struct
            // app.unlisten(id);

            // emit the `event-name` event to all webview windows on the frontend
            tauri::async_runtime::spawn(async move {
                // TODO: on receiving events
                    loop {
                        match receiver.recv() {
                            Ok(msg) => {
                                println!("Received: {}", msg);
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
            greet,
            run_script_on_directory,
            get_graph_state
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}!", name)
}

#[tauri::command]
fn run_script_on_directory(path: String, app_handle: AppHandle) {
    let env = app_handle.state::<InteralState>();
    let mut env = env.0.lock().unwrap();
    env.load_md_directory(Path::new(path.as_str()));
}

#[tauri::command]
fn get_list_of_cells(app_handle: AppHandle) -> String {
    let env = app_handle.state::<InteralState>();
    let mut ee = env.0.lock().unwrap();
    let i = ee.get_instance().unwrap();

    "".to_string()
}

#[tauri::command]
fn get_graph_state(app_handle: AppHandle) -> String {
    let env = app_handle.state::<InteralState>();
    let mut ee = env.0.lock().unwrap();
    let env = ee.get_instance();
    if env.is_err() {
        return "".to_string();
    }
    let env = env.unwrap();

    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    let graph = &env.state.get_dependency_graph();
    for node in graph.nodes() {
        let op = env.state.operation_by_id.get(&node);
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
