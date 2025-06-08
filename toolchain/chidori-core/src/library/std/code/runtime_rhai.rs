use std::io::{self, Write};
use std::pin::Pin;
use std::sync::mpsc::{self, Sender};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use anyhow::Error;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicUsize, Ordering};
use dashmap::DashMap;
use tracing::{debug, Id, Span};
use uuid::Uuid;
use rhai::{Engine, Scope, Dynamic, EvalAltResult, FnPtr};
use rhai::plugin::*;
use rhai::serde::to_dynamic;

use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};
use crate::cells::{CellTypes, CodeCell, LLMPromptCell};
use crate::execution::execution::ExecutionState;
use crate::execution::execution::execution_state::{EnclosedState, ExecutionStateErrors};

static SOURCE_CODE_RUN_COUNTER: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
static CURRENT_RHAI_EXECUTION_ID: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));

static RHAI_OUTPUT_MAP: Lazy<Arc<DashMap<usize, DashMap<String, RkyvSerializedValue>>>> = Lazy::new(|| Arc::new(DashMap::new()));
static RHAI_LOGGING_BUFFER_STDOUT: Lazy<Arc<DashMap<usize, Vec<String>>>> = Lazy::new(|| Arc::new(DashMap::new()));
static RHAI_LOGGING_BUFFER_STDERR: Lazy<Arc<DashMap<usize, Vec<String>>>> = Lazy::new(|| Arc::new(DashMap::new()));

struct LoggingToChannel {
    exec_id: usize,
    sender: Sender<(usize, String)>,
    output_buffer_set: Arc<DashMap<usize, Vec<String>>>,
    buffered_write: Vec<(usize, String)>,
}

impl LoggingToChannel {
    fn new(sender: Sender<(usize, String)>, buffer_set: Arc<DashMap<usize, Vec<String>>>, exec_id: usize) -> Self {
        LoggingToChannel {
            exec_id,
            sender,
            output_buffer_set: buffer_set,
            buffered_write: vec![]
        }
    }

    fn write(&mut self, data: &str) {
        let exec_id = self.exec_id;
        self.buffered_write.push((exec_id, data.to_string()));
        let _ = self.sender.send((exec_id, data.to_string()));
    }

    fn flush(&mut self) {
        for (exec_id, data) in self.buffered_write.drain(..) {
            let mut output = self.output_buffer_set.entry(exec_id).or_insert(vec![]);
            output.push(data.clone());
        }
    }
}

fn set_current_rhai_execution_id(id: usize) {
    CURRENT_RHAI_EXECUTION_ID.store(id, Ordering::SeqCst);
}

fn get_current_rhai_execution_id() -> usize {
    CURRENT_RHAI_EXECUTION_ID.load(Ordering::SeqCst)
}

fn increment_source_code_run_counter() -> usize {
    SOURCE_CODE_RUN_COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn dynamic_to_rkyv_serialized_value(d: &Dynamic) -> RkyvSerializedValue {
    match d.type_name() {
        "i64" => RkyvSerializedValue::Number(d.as_int().unwrap() as i32),
        "f64" => RkyvSerializedValue::Float(d.as_float().unwrap() as f32),
        "string" => RkyvSerializedValue::String(d.to_string()),
        "bool" => RkyvSerializedValue::Boolean(d.as_bool().unwrap()),
        "array" => {
            let arr = d.clone().into_array().unwrap();
            RkyvSerializedValue::Array(
                arr.into_iter()
                    .map(|item| dynamic_to_rkyv_serialized_value(&item))
                    .collect()
            )
        }
        "map" => {
            let map = d.clone().try_cast::<rhai::Map>().unwrap();
            let mut hash_map = HashMap::new();
            for (key, value) in map {
                hash_map.insert(key.to_string(), dynamic_to_rkyv_serialized_value(&value));
            }
            RkyvSerializedValue::Object(hash_map)
        }
        _ => RkyvSerializedValue::Null,
    }
}

fn rkyv_serialized_value_to_dynamic(value: &RkyvSerializedValue) -> Dynamic {
    match value {
        RkyvSerializedValue::Number(n) => Dynamic::from(*n as i64),
        RkyvSerializedValue::Float(f) => Dynamic::from(*f as f64),
        RkyvSerializedValue::String(s) => Dynamic::from(s.clone()),
        RkyvSerializedValue::Boolean(b) => Dynamic::from(*b),
        RkyvSerializedValue::Array(a) => {
            Dynamic::from_array(
                a.iter()
                    .map(|item| rkyv_serialized_value_to_dynamic(item))
                    .collect()
            )
        }
        RkyvSerializedValue::Object(o) => {
            let mut map = rhai::Map::new();
            for (key, value) in o {
                map.insert(key.clone().into(), rkyv_serialized_value_to_dynamic(value));
            }
            Dynamic::from_map(map)
        }
        RkyvSerializedValue::Null => Dynamic::UNIT,
        _ => Dynamic::UNIT,
    }
}

#[export_module]
pub mod chidori_module {
    #[rhai_fn(global)]
    pub fn set_value(id: i64, name: &str, value: Dynamic) {
        let output_c = RHAI_OUTPUT_MAP.clone();
        let output = output_c.entry(id as usize).or_insert(DashMap::new());
        output.insert(name.to_string(), dynamic_to_rkyv_serialized_value(&value));
    }

    #[rhai_fn(global)]
    pub fn on_event(event_name: &str) -> Dynamic {
        // Similar to Python implementation, return a function that can be used as a decorator
        Dynamic::UNIT
    }
}

pub async fn source_code_run_rhai(
    execution_state: &ExecutionState,
    source_code: &String,
    payload: &RkyvSerializedValue,
    function_invocation: &Option<String>,
) -> anyhow::Result<(Result<RkyvSerializedValue, ExecutionStateErrors>, Vec<String>, Vec<String>, ExecutionState)> {
    let current_span_id = Span::current().id();
    debug!("Invoking source_code_run_rhai");

    let exec_id = increment_source_code_run_counter();
    set_current_rhai_execution_id(exec_id);

    let (sender_stdout, receiver_stdout) = mpsc::channel();
    let (sender_stderr, receiver_stderr) = mpsc::channel();

    let execution_state = Arc::new(Mutex::new(execution_state.clone()));

    // Create Rhai engine
    let mut engine = Engine::new();
    
    // Register the chidori module
    engine.register_global_module(exported_module!(chidori_module).into());

    // Set up stdout/stderr capture
    let mut stdout_capture = LoggingToChannel::new(sender_stdout, RHAI_LOGGING_BUFFER_STDOUT.clone(), exec_id);
    let mut stderr_capture = LoggingToChannel::new(sender_stderr, RHAI_LOGGING_BUFFER_STDERR.clone(), exec_id);

    // Create scope for variables
    let mut scope = Scope::new();

    // Add payload to scope if provided
    if let RkyvSerializedValue::Object(ref payload_map) = payload {
        if let Some(RkyvSerializedValue::Object(globals_map)) = payload_map.get("globals") {
            for (key, value) in globals_map {
                scope.push(key.clone(), rkyv_serialized_value_to_dynamic(value));
            }
        }
    }

    // Execute the code
    let result = engine.eval_with_scope::<Dynamic>(&mut scope, source_code);

    match result {
        Ok(_) => {
            // Handle function invocation if specified
            if let Some(name) = function_invocation {
                if let Some(func) = scope.get_value::<FnPtr>(name) {
                    let mut args = vec![];
                    if let RkyvSerializedValue::Object(ref payload_map) = payload {
                        if let Some(RkyvSerializedValue::Object(args_map)) = payload_map.get("args") {
                            let mut args_vec: Vec<_> = args_map
                                .iter()
                                .map(|(k, v)| (k.parse::<i32>().unwrap(), v))
                                .collect();
                            args_vec.sort_by_key(|k| k.0);
                            args = args_vec
                                .into_iter()
                                .map(|(_, v)| rkyv_serialized_value_to_dynamic(v))
                                .collect();
                        }
                    }

                    let mut call_args = String::new();
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            call_args.push_str(", ");
                        }
                        call_args.push_str(&format!("{}", arg));
                    }
                    let call_expr = format!("{}({})", name, call_args);
                    let result = engine.eval_with_scope::<Dynamic>(&mut scope, &call_expr);
                    match result {
                        Ok(value) => {
                            let output_c = RHAI_OUTPUT_MAP.clone();
                            let output = output_c.entry(exec_id).or_insert(DashMap::new());
                            output.insert(name.clone(), dynamic_to_rkyv_serialized_value(&value));
                        }
                        Err(e) => {
                            return Err(anyhow::anyhow!("Function execution error: {}", e));
                        }
                    }
                } else {
                    return Err(anyhow::anyhow!("Function not found: {}", name));
                }
            }

            // Get outputs
            let output_c = RHAI_OUTPUT_MAP.clone();
            let output = if let Some((_, output)) = output_c.remove(&exec_id) {
                RkyvSerializedValue::Object(output.into_iter().collect())
            } else {
                RkyvSerializedValue::Object(HashMap::new())
            };

            let execution_state = execution_state.lock().unwrap().clone();
            let (_, output_stdout) = RHAI_LOGGING_BUFFER_STDOUT.remove(&exec_id).unwrap_or((0, vec![]));
            let (_, output_stderr) = RHAI_LOGGING_BUFFER_STDERR.remove(&exec_id).unwrap_or((0, vec![]));

            Ok((Ok(output), output_stdout, output_stderr, execution_state.clone()))
        }
        Err(e) => {
            Err(anyhow::anyhow!("Rhai execution error: {}", e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[tokio::test]
    async fn test_rhai_source_without_entrypoint() {
        let source_code = String::from(
            r#"
let y = 42;
let x = 12 + y;
let li = [x, y];
        "#,
        );
        let result = source_code_run_rhai(
            &ExecutionState::new_with_random_id(),
            &source_code,
            &RkyvSerializedValue::Null,
            &None,
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvSerializedValue::Object(HashMap::from_iter(vec![
                    ("y".to_string(), RkyvSerializedValue::Number(42),),
                    ("x".to_string(), RkyvSerializedValue::Number(54),),
                    (
                        "li".to_string(),
                        RkyvSerializedValue::Array(vec![
                            RkyvSerializedValue::Number(54),
                            RkyvSerializedValue::Number(42)
                        ]),
                    )
                ]))),
                vec![],
                vec![],
                ExecutionState::new_with_random_id()
            )
        );
    }

    #[tokio::test]
    async fn test_rhai_source_without_entrypoint_with_stdout() {
        let source_code = String::from(
            r#"
print("testing");
        "#,
        );
        let result = source_code_run_rhai(
            &ExecutionState::new_with_random_id(),
            &source_code,
            &RkyvSerializedValue::Null,
            &None,
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvSerializedValue::Object(HashMap::new())),
                vec![String::from("testing"), String::from("\n")],
                vec![],
                ExecutionState::new_with_random_id()
            )
        );
    }

    #[tokio::test]
    async fn test_execution_of_internal_function() {
        let source_code = String::from(
            r#"
fn example() {
    let a = 20;
    return a;
}
        "#,
        );
        let result = source_code_run_rhai(
            &ExecutionState::new_with_random_id(),
            &source_code,
            &RkyvSerializedValue::Null,
            &Some("example".to_string()),
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvSerializedValue::Object(HashMap::from_iter(vec![
                    ("example".to_string(), RkyvSerializedValue::Number(20),),
                ]))),
                vec![],
                vec![],
                ExecutionState::new_with_random_id()
            )
        );
    }

    #[tokio::test]
    async fn test_execution_of_internal_function_with_arguments() {
        let source_code = String::from(
            r#"
fn example(x) {
    let a = 20 + x;
    return a;
}
        "#,
        );
        let result = source_code_run_rhai(
            &ExecutionState::new_with_random_id(),
            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &Some("example".to_string()),
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvSerializedValue::Object(HashMap::from_iter(vec![
                    ("example".to_string(), RkyvSerializedValue::Number(25),),
                ]))),
                vec![],
                vec![],
                ExecutionState::new_with_random_id()
            )
        );
    }
} 