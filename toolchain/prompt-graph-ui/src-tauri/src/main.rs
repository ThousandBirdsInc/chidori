// Prevents additional console window on Windows in release
#![cfg_attr( all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows" )]


use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::Arc;
use tauri::Manager;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing::info;
use tracing_subscriber;
use prompt_graph_core::proto2::{ChangeValue, ChangeValueWithCounter, Empty, ListRegisteredGraphsResponse, NodeWillExecuteOnBranch, Path, RequestOnlyId, SerializedValue};
use prompt_graph_core::proto2::execution_runtime_client::ExecutionRuntimeClient;
use prompt_graph_core::utils::serialized_value_to_string;
use prost::Message;

async fn get_client(url: String) -> Result<ExecutionRuntimeClient<tonic::transport::Channel>, tonic::transport::Error> {
    ExecutionRuntimeClient::connect(url.clone()).await
}


/// Serialized state is a representation of the entire application
/// On initial load we send this to the frontend
/// Subsequently we send only changes to this object
///
/// This is a key value map of counters to values generally
#[derive(serde::Serialize, Clone)]
struct SerializedState {
    change_events: HashMap<u64, Vec<u8>>,
    node_will_execute_events: HashMap<u64, Vec<u8>>,
}

struct AppState {
    changes: Arc<Mutex<Vec<ChangeValueWithCounter>>>,
    node_will_execute_events: Arc<Mutex<Vec<NodeWillExecuteOnBranch>>>,
    async_proc_input_tx: Mutex<mpsc::Sender<String>>,
}

fn main() {
    tracing_subscriber::fmt::init();

    let (async_proc_input_tx, async_proc_input_rx) = mpsc::channel(1);
    let (async_proc_output_tx, mut async_proc_output_rx) = mpsc::channel(1);

    let mut changes = Arc::new(Mutex::new(vec![
        ChangeValueWithCounter {
            filled_values: vec![],
            parent_monotonic_counters: vec![],
            monotonic_counter: 0,
            branch: 0,
            source_node: "".to_string(),
        }
    ]));
    let mut node_will_execute_events = Arc::new(Mutex::new(vec![
        NodeWillExecuteOnBranch {
            branch: 0,
            counter: 0,
            custom_node_type_name: None,
            node: None,
        }
    ]));

    tauri::Builder::default()
        .manage(AppState {
            changes: changes.clone(),
            node_will_execute_events: node_will_execute_events.clone(),
            async_proc_input_tx: Mutex::new(async_proc_input_tx),
        })
        .invoke_handler(tauri::generate_handler![
            // js2rs,
            get_initial_state,
            list_files
        ])
        .setup(|app| {
            tauri::async_runtime::spawn(async move {
                async_process_model(
                    async_proc_input_rx,
                    async_proc_output_tx,
                ).await
            });

            let app_handle = app.handle();
            let mut seen_changes = Arc::new(Mutex::new(HashSet::new()));
            let mut seen_nwe_events = Arc::new(Mutex::new(HashSet::new()));

            tauri::async_runtime::spawn(async move {
                loop {
                    // This makes a new connection to the client each time we've exhausted the stream
                    if let Ok(mut client) = get_client("http://localhost:9800".to_string()).await {
                        if let Ok(resp) = client.list_change_events(RequestOnlyId {
                            id: "0".to_string(),
                            branch: 0,
                        }).await {
                            let mut stream = resp.into_inner();
                            while let Some(x) = stream.next().await {
                                if let Ok(x) = x {
                                    let counter = x.monotonic_counter;
                                    if seen_changes.lock().await.contains(&counter) {
                                        continue;
                                    }
                                    seen_changes.lock().await.insert(counter);
                                    changes.lock().await.push(x);
                                }
                            };
                        }
                    } else {
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        continue;
                    }
                }
            });


            tauri::async_runtime::spawn(async move {
                loop {
                    // This makes a new connection to the client each time we've exhausted the stream
                    if let Ok(mut client) = get_client("http://localhost:9800".to_string()).await {
                        if let Ok(resp) = client.list_node_will_execute_events(RequestOnlyId {
                            id: "0".to_string(),
                            branch: 0,
                        }).await {
                            let mut stream = resp.into_inner();
                            while let Some(x) = stream.next().await {
                                if let Ok(x) = x {
                                    let counter = x.counter;
                                    if seen_nwe_events.lock().await.contains(&counter) {
                                        continue;
                                    }
                                    seen_nwe_events.lock().await.insert(counter);
                                    node_will_execute_events.lock().await.push(x);
                                }
                            };
                        }
                    } else {
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        continue;
                    }
                }
            });

            // tauri::async_runtime::spawn(async move {
            //     loop {
            //         if let Some(output) = async_proc_output_rx.recv().await {
            //             rs2js(output, &app_handle);
            //         }
            //     }
            // });

            Ok(())
        })
        .plugin(tauri_plugin_app::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_window::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// #[tauri::command]
// async fn js2rs(
//     message: String,
//     state: tauri::State<'_, AppState>,
// ) -> Result<(), String> {
//     info!(?message, "js2rs");
//     let async_proc_input_tx = state.async_proc_input_tx.lock().await;
//     async_proc_input_tx
//         .send(message)
//         .await
//         .map_err(|e| e.to_string())
// }


#[tauri::command]
async fn list_files(
    state: tauri::State<'_, AppState>,
) -> Result<ListRegisteredGraphsResponse, String> {
    info!("list_files");
    if let Ok(mut client) = get_client("http://localhost:9800".to_string()).await {
        let graphs = client.list_registered_graphs(Empty{}).await.map_err(|e| e.to_string())?;
        return Ok(graphs.into_inner().clone());
    }
    Err("Failed to connect to runtime".to_string())
}


#[tauri::command]
async fn get_initial_state(
    state: tauri::State<'_, AppState>,
) -> Result<SerializedState, String> {
    // info!(?message, "get_current_changes");

    let changes = state.changes.lock().await;
    let events = state.node_will_execute_events.lock().await;

    Ok(SerializedState {
        change_events: changes.deref().iter().map(|x| (x.monotonic_counter, x.encode_to_vec())).collect(),
        node_will_execute_events: events.deref().iter().map(|x| (x.counter, x.encode_to_vec())).collect(),
    })
}

async fn async_process_model(
    mut input_rx: mpsc::Receiver<String>,
    output_tx: mpsc::Sender<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    while let Some(input) = input_rx.recv().await {
        let output = input;
        output_tx.send(output).await?;
    }

    Ok(())
}
