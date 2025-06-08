use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use bevy::prelude::{Commands, ResMut};
use bevy_utils::tracing::debug;

use chidori_core::sdk::interactive_chidori_wrapper::{EventsFromRuntime, InteractiveChidoriWrapper};
use chidori_core::sdk::chidori_runtime_instance::PlaybackState;
use crate::accidental::tokio_tasks;

use super::types::ChidoriState;

const RECV_RUNTIME_EVENT_TIMEOUT_MS: u64 = 100;

pub fn setup(mut commands: Commands, runtime: ResMut<tokio_tasks::TokioTasksRuntime>) {
    let (trace_event_sender, trace_event_receiver) = std::sync::mpsc::channel();
    let (runtime_event_sender, runtime_event_receiver) = std::sync::mpsc::channel();
    let mut internal_state = ChidoriState {
        debug_mode: false,
        chidori: std::sync::Arc::new(std::sync::Mutex::new(InteractiveChidoriWrapper::new_with_events(
            trace_event_sender,
            runtime_event_sender,
        ))),
        watched_path: std::sync::Mutex::new(None),
        background_thread: std::sync::Mutex::new(None),
        file_watch: std::sync::Mutex::new(None),
        application_state_is_displaying_example_modal: true,
        application_state_is_displaying_save_dialog: false,
        current_playback_state: PlaybackState::Paused,
        execution_id_to_evaluation: std::sync::Arc::new(Default::default()),
        local_cell_state: Default::default(),
        log_messages: vec![],
        definition_graph: vec![],
        execution_graph: vec![],
        grouped_nodes: Default::default(),
        current_execution_head: Default::default(),
        trace_events: vec![],
    };

    // Clone a reference to the shared execution_id_to_evaluation
    {
        let mut chidori = internal_state
            .chidori
            .lock()
            .expect("Failed to lock background_thread");
        let shared_state = chidori.shared_state.lock().unwrap();
        internal_state.execution_id_to_evaluation = shared_state.execution_id_to_evaluation.clone();
    }

    {
        let mut background_thread_guard = internal_state
            .background_thread
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

                let _ = instance.wait_until_ready().await;
                let result = instance.run(PlaybackState::Paused).await;
                match result {
                    Ok(_) => {
                        panic!("Instance completed execution and closed successfully.");
                    }
                    Err(e) => {
                        println!("Error occurred: {}, retrying...", e);
                    }
                }
            }
        }));
    }

    runtime.spawn_background_task(|mut ctx| async move {
        loop {
            match runtime_event_receiver.recv_timeout(Duration::from_millis(RECV_RUNTIME_EVENT_TIMEOUT_MS)) {
                Ok(msg) => {
                    debug!("Received message from runtime: {:?}", &msg);
                    let msg_to_logs = msg.clone();
                    ctx.run_on_main_thread(move |ctx| {
                        if let Some(mut s) =
                            ctx.world.get_resource_mut::<ChidoriState>()
                        {
                            s.log_messages.push(format!("Received from runtime: {:?}", &msg_to_logs));
                        }
                    })
                        .await;
                    match msg {
                        EventsFromRuntime::ExecutionGraphUpdated(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriState>()
                                {
                                    s.execution_graph = state;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::UpdateExecutionHead(head) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut s) =
                                    ctx.world.get_resource_mut::<ChidoriState>()
                                {
                                    s.current_execution_head = head;
                                }
                            })
                            .await;
                        }
                        EventsFromRuntime::ReceivedChatMessage(_) => {}
                        EventsFromRuntime::PlaybackState(state) => {
                            ctx.run_on_main_thread(move |ctx| {
                                if let Some(mut internal_state) = ctx.world.get_resource_mut::<ChidoriState>() {
                                    internal_state.current_playback_state = state;
                                }
                            })
                                .await;
                        }
                    }
                }
                Err(e) => match e {
                    RecvTimeoutError::Timeout => {}
                    RecvTimeoutError::Disconnected => {
                        println!("Runtime channel disconnected");
                        break;
                    }
                },
            }
        }
        println!("Runtime event loop ended");
    });

    runtime.spawn_background_task(|mut ctx| async move {
        loop {
            match trace_event_receiver.recv() {
                Ok(msg) => {
                    // println!("Received: {:?}", &msg);
                    // handle.emit_all("execution:events", Some(trace_event_to_string(msg)));
                    ctx.run_on_main_thread(move |ctx| {
                        if let Some(mut s) = ctx.world.get_resource_mut::<ChidoriState>() {
                            s.trace_events.push(msg);
                        }
                    })
                    .await;
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