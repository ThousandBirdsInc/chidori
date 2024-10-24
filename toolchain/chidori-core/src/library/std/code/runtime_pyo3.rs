use std::io::{self, Write};

use std::pin::Pin;
use chidori_static_analysis::language::python::parse::{
    build_report, extract_dependencies_python,
};

use futures_util::FutureExt;
use pyo3::prelude::*;
use pyo3::types::{IntoPyDict, PyCFunction, PyDict, PyList, PySet, PyTuple};
use std::sync::mpsc::{self, Sender};

use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::{env, mem};
use std::sync::{Arc, Mutex};
use anyhow::Error;
use once_cell::sync::OnceCell;
use pyo3_asyncio::generic;
use tokio::runtime::Runtime;
use chidori_static_analysis::language::Report;
use crate::cells::{CellTypes, CodeCell, LLMPromptCell};
use crate::execution::execution::ExecutionState;

use std::path::{Path, PathBuf};
use std::process::Command;
use anyhow::{anyhow};

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicUsize, Ordering};
use dashmap::DashMap;
use log::info;
use regex::Regex;

use sha1::{Sha1, Digest};
use tracing::{Id, Span};
use uuid::Uuid;
use crate::execution::execution::execution_state::ExecutionStateErrors;

static SOURCE_CODE_RUN_COUNTER: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
static CURRENT_PYTHON_EXECUTION_ID: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));

fn install_dependencies_from_requirements(requirements_dir: &str, venv_path: &str) -> anyhow::Result<()> {
    let requirements_path = Path::new(requirements_dir).join("requirements.txt");

    if !requirements_path.exists() {
        return Err(anyhow!("requirements.txt not found in the specified directory"));
    }

    let uv_path = which::which("uv").map_err(|_| anyhow!("uv not found in PATH"))?;

    // Use 'uv pip sync' to install dependencies
    let status = Command::new(uv_path)
        .arg("pip")
        .arg("sync")
        .arg(requirements_path)
        .arg("--virtualenv")
        .arg(venv_path)
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("Failed to install dependencies from requirements.txt"))
    }
}

fn get_or_create_default_venv() -> anyhow::Result<PathBuf> {
    let home_dir = env::var("HOME").or_else(|_| env::var("USERPROFILE"))?;
    let default_venv_dir = PathBuf::from(home_dir).join(".chidori_venvs");

    if !default_venv_dir.exists() {
        std::fs::create_dir_all(&default_venv_dir)?;
    }

    let venv_name = format!("chidori_venv_{}", Uuid::new_v4());
    let venv_path = default_venv_dir.join(venv_name);

    let uv_path = which::which("uv").map_err(|_| anyhow::anyhow!("uv not found in PATH"))?;
    let status = Command::new(uv_path)
        .arg("venv")
        .arg(&venv_path)
        .status()?;

    if !status.success() {
        return Err(anyhow::anyhow!("Failed to create default virtualenv"));
    }

    Ok(venv_path)
}


fn pyany_to_rkyv_serialized_value(p: &PyAny) -> RkyvSerializedValue {
    match p.get_type().name() {
        Ok(s) => match s {
            "int" => {
                let val = p.extract::<i32>().unwrap();
                RkyvSerializedValue::Number(val)
            }
            "float" => {
                let val = p.extract::<f32>().unwrap();
                RkyvSerializedValue::Float(val)
            }
            "str" => {
                let val = p.extract::<String>().unwrap();
                RkyvSerializedValue::String(val)
            }
            "bool" => {
                let val = p.extract::<bool>().unwrap();
                RkyvSerializedValue::Boolean(val)
            }
            "list" => {
                let list = p.downcast::<PyList>().unwrap();
                let arr = list
                    .iter()
                    .map(|item| pyany_to_rkyv_serialized_value(item))
                    .collect();
                RkyvSerializedValue::Array(arr)
            }
            "tuple" => {
                let list = p.downcast::<PyTuple>().unwrap();
                let arr = list
                    .iter()
                    .map(|item| pyany_to_rkyv_serialized_value(item))
                    .collect();
                RkyvSerializedValue::Array(arr)
            }
            "dict" => {
                let dict = p.downcast::<PyDict>().unwrap();
                let mut map = HashMap::new();
                for (key, value) in dict {
                    let key_string = key.extract::<String>().unwrap();
                    map.insert(key_string, pyany_to_rkyv_serialized_value(value));
                }
                RkyvSerializedValue::Object(map)
            }
            "set" => {
                let pyset = p.downcast::<PySet>().unwrap();
                let mut set = HashSet::new();
                for value in pyset {
                    set.insert(pyany_to_rkyv_serialized_value(value));
                }
                RkyvSerializedValue::Set(set)
            }
            "NoneType" => {
                RkyvSerializedValue::Null
            },
            "Future" => {
                RkyvSerializedValue::Null
            },
            x @ _  => {
                panic!("Py03 marshalling unsupported type: {}", x);
                RkyvSerializedValue::Null
            },
        },
        Err(_) => RkyvSerializedValue::Null,
    }
}

fn rkyv_serialized_value_to_pyany(py: Python, value: &RkyvSerializedValue) -> PyObject {
    match value {
        RkyvSerializedValue::Number(n) => n.into_py(py),
        RkyvSerializedValue::Float(f) => f.into_py(py),
        RkyvSerializedValue::String(s) => s.into_py(py),
        RkyvSerializedValue::Boolean(b) => b.into_py(py),
        RkyvSerializedValue::Array(a) => {
            let py_list = PyList::empty(py);
            for item in a {
                let py_item = rkyv_serialized_value_to_pyany(py, item);
                py_list.append(py_item).unwrap();
            }
            py_list.into_py(py)
        }
        RkyvSerializedValue::Object(o) => {
            let py_dict = PyDict::new(py);
            for (key, value) in o {
                let py_value = rkyv_serialized_value_to_pyany(py, value);
                py_dict.set_item(key, py_value).unwrap();
            }
            py_dict.into_py(py)
        }
        RkyvSerializedValue::Null => py.None(),
        // TODO: Handle other types
        _ => py.None(),
    }
}

// Helper function for String conversion if needed
impl RkyvSerializedValue {
    fn as_string(&self) -> Option<&String> {
        if let RkyvSerializedValue::String(s) = self {
            Some(s)
        } else {
            None
        }
    }
}

#[pyfunction]
fn identity_function(py: Python, arg: PyObject) -> PyResult<PyObject> {
    Ok(arg)
}

#[pyfunction]
fn on_event(py: Python, arg: PyObject) -> PyResult<PyObject> {
    let identity_func = wrap_pyfunction!(identity_function, py)?;
    // Convert &PyCFunction to PyObject
    let obj: PyObject = identity_func.into_py(py);
    Ok(obj)
}

/// When called this suspends execution with a long running rust function
/// we hand back the GIL for other python execution. Invoke is used to execute another
/// cell's provided function, or a cell as a function.
// TODO: test and demonstrate this
#[pyfunction]
fn invoke(py: Python, arg: PyObject) -> PyResult<PyObject> {
    py.allow_threads(|| 0);
    let identity_func = wrap_pyfunction!(identity_function, py)?;
    let obj: PyObject = identity_func.into_py(py);
    Ok(obj)
}


fn set_current_python_execution_id(id: usize) {
    CURRENT_PYTHON_EXECUTION_ID.store(id, Ordering::SeqCst);
}

fn get_current_python_execution_id() -> usize {
    CURRENT_PYTHON_EXECUTION_ID.load(Ordering::SeqCst)
}

fn increment_source_code_run_counter() -> usize {
    SOURCE_CODE_RUN_COUNTER.fetch_add(1, Ordering::SeqCst)
}

static PYTHON_OUTPUT_MAP: Lazy<Arc<DashMap<usize, DashMap<String, RkyvSerializedValue>>>> = Lazy::new(|| Arc::new(DashMap::new()));
static PYTHON_LOGGING_BUFFER_STDOUT: Lazy<Arc<DashMap<usize, Vec<String>>>> = Lazy::new(|| Arc::new(DashMap::new()));
static PYTHON_LOGGING_BUFFER_STDERR: Lazy<Arc<DashMap<usize, Vec<String>>>> = Lazy::new(|| Arc::new(DashMap::new()));

#[pyclass]
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
}

#[pymethods]
impl LoggingToChannel {
    fn set_exec_id(&mut self, exec_id: usize) {
        self.exec_id = exec_id;
    }

    fn write(&mut self, data: &str) {
        let exec_id = self.exec_id;;
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


#[derive(Debug)]
pub struct AnyhowErrWrapper(anyhow::Error);


impl std::convert::From<AnyhowErrWrapper> for PyErr {
    fn from(err: AnyhowErrWrapper) -> PyErr {
        pyo3::exceptions::PyOSError::new_err(err.0.to_string())
    }
}





#[tracing::instrument]
pub async fn source_code_run_python(
    execution_state: &ExecutionState,
    source_code: &String,
    payload: &RkyvSerializedValue,
    function_invocation: &Option<String>,
    virtualenv_path: &Option<String>,
    requirements_dir: &Option<String>,
) -> anyhow::Result<(Result<RkyvSerializedValue, ExecutionStateErrors>, Vec<String>, Vec<String>)> {

    // Capture the current span's ID
    let current_span_id = Span::current().id();

    println!("Invoking source_code_run_python");

    let exec_id = increment_source_code_run_counter();

    // Ensure virtualenv exists or create it
    let venv_path = if let Some(venv_path) = &virtualenv_path {
        PathBuf::from(venv_path)
    } else {
        let default_venv = get_or_create_default_venv()?;
        default_venv
    };

    if !venv_path.exists() {
        let uv_path = which::which("uv").map_err(|_| anyhow::anyhow!("uv not found in PATH"))?;
        let status = Command::new(uv_path)
            .arg("venv")
            .arg(&venv_path)
            .status()?;
        if !status.success() {
            return Err(anyhow::anyhow!("Failed to create virtualenv"));
        }
    }

    // Install dependencies from requirements.txt if specified
    if let Some(req_dir) = &requirements_dir {
        install_dependencies_from_requirements(req_dir, venv_path.to_str().unwrap())?;
    }

    pyo3::prepare_freethreaded_python();
    let (sender_stdout, receiver_stdout) = mpsc::channel();
    let (sender_stderr, receiver_stderr) = mpsc::channel();

    let dependencies = extract_dependencies_python(&source_code)?;
    let report = build_report(&dependencies);

    let result =  Python::with_gil(|py| {
        // TODO: this was causing a deadlock
        let current_event_loop = pyo3_asyncio::tokio::get_current_loop(py);
        // Initialize our event loop if one is not already established
        let event_loop = if current_event_loop.is_err() {
            let asyncio = py.import("asyncio")?;
            let event_loop = asyncio.call_method0("new_event_loop")?;
            asyncio.call_method1("set_event_loop", (event_loop,))?;
            event_loop
        } else {
            current_event_loop.unwrap()
        };

        // Configure locals and globals passed to evaluation
        let globals = PyDict::new(py);
        let execution_state = Arc::new(Mutex::new(execution_state.clone()));
        create_external_function_shims(&execution_state, &report, py, globals, current_span_id.clone())?;
        create_internal_proxy_shims(&execution_state, &report, py, globals, current_span_id)?;


        let sys = py.import("sys")?;

        // Add virtualenv path to sys.path
        let site_packages_path = venv_path
            .join("lib")
            .join("python3.12")  // Adjust this version as needed
            .join("site-packages");

        if site_packages_path.exists() {
            let current_path: Vec<String> = sys.getattr("path")?.extract()?;
            let mut new_path = vec![site_packages_path.to_str().unwrap().to_string()];
            new_path.extend(current_path);
            sys.setattr("path", new_path)?;
        } else {
            return Err(anyhow::anyhow!("Virtualenv site-packages not found: {:?}", site_packages_path));
        }

        // Create Chidori module if it doesn't already exist
        let py_modules = sys.getattr("modules")?;
        if py_modules.get_item("chidori").is_err() {
            // We assume this will only happen once for the Python GIL instance (per this Rust process)
            // so this is treated as an initialization handler.
            let chidori_module = PyModule::new(py, "chidori")?;
            chidori_module.add_function(wrap_pyfunction!(on_event, chidori_module)?)?;
            chidori_module.add_function(wrap_pyfunction!(identity_function, chidori_module)?)?;
            let chidori_set_value = PyCFunction::new_closure(
                py,
                None,
                None,
                move |args: &PyTuple, kwargs: Option<&PyDict>| {
                    if args.len() == 3 {
                        let id: usize = args.get_item(0).unwrap().extract::<usize>().unwrap();
                        let name: String = args.get_item(1).unwrap().extract::<String>().unwrap();
                        let output_c = PYTHON_OUTPUT_MAP.clone();
                        let output = output_c.entry(id).or_insert(DashMap::new());
                        let value = args.get_item(2).unwrap(); // Keep as PyAny
                        output.insert(name, pyany_to_rkyv_serialized_value(value));
                    }
                },
            )?;
            chidori_module.add("set_value", chidori_set_value)?;
            py_modules.set_item("chidori", chidori_module)?;
        }

        // Set up capture of stdout from python process and storing it into a Vec
        let stdout_capture = LoggingToChannel::new(sender_stdout, PYTHON_LOGGING_BUFFER_STDOUT.clone(), exec_id);
        let stdout_capture_py = stdout_capture.into_py(py);
        let stderr_capture = LoggingToChannel::new(sender_stderr, PYTHON_LOGGING_BUFFER_STDERR.clone(), exec_id);
        let stderr_capture_py = stderr_capture.into_py(py);

        sys.setattr("stdout", stdout_capture_py)?;
        sys.setattr("stderr", stderr_capture_py)?;

        if let RkyvSerializedValue::Object(ref payload_map) = payload {
            if let Some(RkyvSerializedValue::Object(globals_map)) = payload_map.get("globals") {
                for (key, value) in globals_map {
                    println!("Setting globals {}: {:?}", key, value);
                    let py_value = rkyv_serialized_value_to_pyany(py, value); // Implement this function to convert RkyvSerializedValue to PyObject
                    globals.set_item(key, py_value)?;
                }
            }
        }

        // Add recording of specific values to the source code since we're going to wrap it
        let mut initial_source_code = format!(r#"
import sys
sys.stdout.set_exec_id({exec_id})
sys.stderr.set_exec_id({exec_id})
        "#, exec_id=exec_id);
        initial_source_code.push_str("\n");
        initial_source_code.push_str(&source_code.clone());

        // If any instances of these lines are located, skip wrapping anything because the code will initialize its own async runtime.
        let does_contain_async_runtime = initial_source_code
            .lines()
            .any(|line| line.contains("asyncio.run") || line.contains("unittest.IsolatedAsyncioTestCase") || line.contains("loadTestsFromTestCase"));

        let mut complete_code = if does_contain_async_runtime {
            println!("Executing python with 'does_contain_async_runtime' ");
            // If we have an async function, we don't need to wrap it in an async function
            format!(r#"
import chidori

{}
sys.stdout.flush()
sys.stderr.flush()
        "#, initial_source_code)
        } else {
            println!("Executing python with 'no async runtime' ");

            for (name, report_item) in &report.cell_exposed_values {
                initial_source_code.push_str("\n");
                initial_source_code.push_str(&format!(
                    r#"chidori.set_value({exec_id}, "{name}", {name})"#,
                    exec_id = exec_id,
                    name = name
                ));
            }
            // Necessary to expose defined functions to the global scope from the inside of the __wrapper function
            for (name, report_item) in &report.triggerable_functions {

                // Re-assign functions to their HashA instance - these are where we hold
                // on to the initial definitions of the functions for inbound function calls.
                initial_source_code.push_str("\n");
                initial_source_code.push_str(&format!(
                    r#"{hashed_name} = {name}"#,
                    name = name,
                    hashed_name = hash_to_python_method_name(name)
                ));

                // The twice hashed (HashB) instance is assigned to the original function name,
                // internal references to the function invoke this.
                initial_source_code.push_str("\n");
                initial_source_code.push_str(&format!(
                    r#"{name} = {twice_hashed_name}"#,
                    name = name,
                    twice_hashed_name = hash_to_python_method_name(&hash_to_python_method_name(name))
                ));

                // Declare in our output what functions were defined by this run.
                initial_source_code.push_str("\n");
                initial_source_code.push_str(&format!(
                    r#"chidori.set_value({exec_id}, "{name}", "function")"#,
                    exec_id = exec_id,
                    name = name
                ));

                // Assign a mapping of functions declared within the module to a global map
                initial_source_code.push_str("\n");
                initial_source_code.push_str(&format!(
                    r#"globals()["{name}"] = {name}"#,
                    name = hash_to_python_method_name(name)
                ));
            }
            let indent_all_source_code = initial_source_code.lines().map(|line| format!("    {}", line)).collect::<Vec<_>>().join("\n");
            // Wrap all of our code in a top level async wrapper
            format!(r#"
import asyncio
import chidori
async def __wrapper():
{}
    sys.stdout.flush()
    sys.stderr.flush()
asyncio.run(__wrapper())
        "#, indent_all_source_code)
        };

        // Important: this is the point of initial execution of the source code
        py.run(&complete_code, Some(globals), None)?;

        // With the source environment established, we can now invoke specific methods provided by this node
        return match function_invocation {
            None => {
                py.allow_threads(move || {
                    Ok(Box::pin(async move {
                        let output_c = PYTHON_OUTPUT_MAP.clone();
                        Ok(if let Some((k, output_c)) = output_c.remove(&exec_id) {
                            RkyvSerializedValue::Object(output_c.into_iter().collect())
                        } else {
                            RkyvSerializedValue::Object(HashMap::new())
                        })
                    }) as Pin<Box<dyn Future<Output = Result<RkyvSerializedValue, ExecutionStateErrors>> + Send>>)
                })
            }
            Some(name) => {
                // Function invocations refer to the function hashed _once_ this is the reference
                // to the original definition of the function.
                let name = hash_to_python_method_name(&name);

                // This is calling to the not proxied version, so it is the Hash A instance of the function
                // otherwise we're in a loop of external dispatches
                let local = globals.get_item(name)?;
                if let Some(py_func) = local {
                    // Call the function
                    let mut args: Vec<Py<PyAny>> = vec![];
                    let mut kwargs = vec![];
                    if let RkyvSerializedValue::Object(ref payload_map) = payload {
                        if let Some(RkyvSerializedValue::Object(args_map)) = payload_map.get("args")
                        {
                            let mut args_vec: Vec<_> = args_map
                                .iter()
                                .map(|(k, v)| (k.parse::<i32>().unwrap(), v))
                                .collect();

                            args_vec.sort_by_key(|k| k.0);
                            args.extend(
                                args_vec
                                    .into_iter()
                                    .map(|(_, v)| rkyv_serialized_value_to_pyany(py, v)),
                            );
                        }

                        if let Some(RkyvSerializedValue::Object(kwargs_map)) =
                            payload_map.get("kwargs")
                        {
                            for (k, v) in kwargs_map.iter() {
                                kwargs.push((k, rkyv_serialized_value_to_pyany(py, v)));
                            }
                        }
                    }

                    let args = PyTuple::new(py, &args);
                    let kwargs = kwargs.into_iter().into_py_dict(py);

                    println!("calling the function");
                    let result = py_func.call(args, Some(kwargs)).map_err(|e| {
                        dbg!(&e);
                        e
                    })?;
                    println!("after calling the function");
                    if result.get_type().name().unwrap() == "coroutine" {
                        // If the function is a coroutine, we need to await it
                        println!("before into future for python coroutine");
                        let is_running = event_loop.call_method0("is_running")?.extract::<bool>()?;
                        let (fut, result, needs_await) = if !is_running {
                            // If not running, run the event loop
                            let py_any = event_loop.call_method1("run_until_complete", (result,))?;
                            (None, Some(py_any.into_py(py)), false)
                        } else {
                            // If already running, prepare to use pyo3_asyncio
                            println!("Event loop is already running, using pyo3_asyncio");
                            let future = pyo3_asyncio::tokio::into_future(result)?;
                            (Some(future), None, true)
                        };


                        // let f = pyo3_asyncio::tokio::into_future(result)?;
                        Ok(Box::pin(async move {
                            println!("waiting the python coroutine");
                            let final_result = if let Some(fut) = fut {
                                fut.await.map_err(|e| ExecutionStateErrors::Unknown(e.to_string()))?
                            } else {
                                result.unwrap()
                            };

                            Ok(Python::with_gil(|py| {
                                let py_any: &PyAny = final_result.as_ref(py);
                                pyany_to_rkyv_serialized_value(py_any)
                            }))
                        }) as Pin<Box<dyn Future<Output = Result<RkyvSerializedValue, ExecutionStateErrors>> + Send>>)
                    } else {
                        let result: PyObject = result.into_py(py);
                        Ok(Box::pin(async move {
                            Ok(Python::with_gil(|py| {
                                let py_any: &PyAny = result.as_ref(py);
                                pyany_to_rkyv_serialized_value(py_any)
                            }))
                        }) as Pin<Box<dyn Future<Output=Result<RkyvSerializedValue, ExecutionStateErrors>> + Send>>)
                    }
                } else {
                    Err(anyhow::anyhow!("Function not found"))
                }
            }
        }
    });
    match result {
        Ok(result) => {
            println!("about to await result");
            let awaited_result = result.await;
            println!("after awaited result");
            let (_, output_stdout) = PYTHON_LOGGING_BUFFER_STDOUT.remove(&exec_id).unwrap_or((0, vec![]));
            let (_, output_stderr) = PYTHON_LOGGING_BUFFER_STDERR.remove(&exec_id).unwrap_or((0, vec![]));
            Ok((awaited_result, output_stdout, output_stderr))
        }
        Err(e) => {
            return Err(anyhow::anyhow!(e.to_string()));
        }
    }
}


fn create_internal_proxy_shims(execution_state_handle: &Arc<Mutex<ExecutionState>>, report: &Report, py: Python, globals: &PyDict, parent_span_id: Option<tracing::Id>) -> Result<(), Error> {
    // Create shims for the functions declared within this file,
    // when a hashed reference to a function is invoked, we invoke the actual function internally
    // but through our dispatch system, producing execution states.

    // map_of_renamed_fns_to_original: HashMap<String, String>
    for (function_name, triggerable_function) in &report.triggerable_functions {
        let clone_function_name = function_name.clone();
        let execution_state_handle = execution_state_handle.clone();
        let parent_span_id = parent_span_id.clone();
        let closure_callable = create_python_dispatch_closure(py, clone_function_name, execution_state_handle, parent_span_id)?;

        // Internal proxy shims are assigned to the function name hashed _twice_
        let renamed_function = hash_to_python_method_name(&hash_to_python_method_name(&function_name));
        globals.set_item(renamed_function, closure_callable);
    }
    Ok(())
}

fn create_python_dispatch_closure(py: Python, clone_function_name: String, execution_state_handle: Arc<Mutex<ExecutionState>>, parent_span_id: Option<Id>) -> Result<&PyCFunction, Error> {
    let closure_callable = PyCFunction::new_closure(
        py,
        None, // name
        None, // doc
        move |args: &PyTuple, kwargs: Option<&PyDict>| -> PyResult<PyObject> {
            let total_arg_payload = python_args_to_rkyv(args, kwargs)?;
            let clone_function_name = clone_function_name.clone();
            let parent_span_id = parent_span_id.clone();
            let py = args.py();
            // TODO: this should be after the dispatch, substituting it
            // let mut new_exec_state = {
            //     let mut exec_state = execution_state_handle.lock().unwrap();
            //     let mut new_exec_state = exec_state.clone();
            //     std::mem::swap(&mut *exec_state, &mut new_exec_state);
            //     new_exec_state
            // };

            let mut new_exec_state = execution_state_handle.lock().unwrap().clone();
            // All function calls across cells are forced to be async
            pyo3_asyncio::tokio::future_into_py(py, async move {
                let (result, execution_state) = new_exec_state.dispatch(&clone_function_name, total_arg_payload, parent_span_id.clone()).await.map_err(|e| AnyhowErrWrapper(e))?;
                // TODO: assign this execution state to the current so that as we continue to execute this is now where we're progressing from
                // TODO: this should not be an unwrap and should instead propagate the error
                match result {
                    Ok(result) => {
                        PyResult::Ok(Python::with_gil(|py| rkyv_serialized_value_to_pyany(py, &result)))
                    }
                    Err(e) => {
                        Err(PyErr::new::<pyo3::exceptions::PyTypeError, _>(format!("{:}", e)))
                    }
                }
            }).map(|x| x.into())
        },
    )?;
    Ok(closure_callable)
}

fn create_external_function_shims(execution_state_handle: &Arc<Mutex<ExecutionState>>, report: &Report, py: Python, globals: &PyDict, parent_span_id: Option<tracing::Id>) -> Result<(), Error> {
    // Create shims for functions that are referred to, we look at what functions are being provided
    // and create shims for matches between the function name provided and the identifiers referred to.

    let function_names = {
        let execution_state_handle = execution_state_handle.clone();
        let mut exec_state = execution_state_handle.lock().unwrap();
        exec_state.function_name_to_metadata.keys().cloned().collect::<Vec<_>>()
    };
    for function_name in function_names {
        if report
            .cell_depended_values
            .contains_key(&function_name)
        {
            let clone_function_name = function_name.clone();
            let execution_state_handle = execution_state_handle.clone();

            let parent_span_id = parent_span_id.clone();
            let closure_callable = create_python_dispatch_closure(py, clone_function_name, execution_state_handle, parent_span_id)?;
            // TODO: this should get something that identifies some kind of signal
            //       for how to fetch the execution state
            globals.set_item(function_name.clone(), closure_callable);
        }
    }
    Ok(())
}

fn python_args_to_rkyv(args: &PyTuple, kwargs: Option<&PyDict>) -> Result<RkyvSerializedValue, PyErr> {
    let total_arg_payload = RkyvObjectBuilder::new();
    let total_arg_payload =
        total_arg_payload.insert_value("args", {
            let mut m = HashMap::new();
            for (i, a) in args.iter().enumerate() {
                m.insert(
                    format!("{}", i),
                    pyany_to_rkyv_serialized_value(a),
                );
            }
            RkyvSerializedValue::Object(m)
        });

    let total_arg_payload = if let Some(kwargs) = kwargs {
        total_arg_payload.insert_value("kwargs", {
            let mut m = HashMap::new();
            for (i, a) in kwargs.iter() {
                let k: String = i.extract()?;
                m.insert(k, pyany_to_rkyv_serialized_value(a));
            }
            RkyvSerializedValue::Object(m)
        })
    } else {
        total_arg_payload
    }.build();
    Ok(total_arg_payload)
}

/// This allows for interaction with Rye and Uv which we use for managing
/// dependencies and virtualenvs for our python code execution.
fn python_dependency_management() {
    // TODO: https://github.com/PyO3/pyo3/discussions/3726
}

fn replace_identifier(code: &str, old_identifier: &str, new_identifier: &str) -> String {
    let pattern = format!(r"(?<![a-zA-Z0-9_]){}(?![a-zA-Z0-9_])", regex::escape(old_identifier));
    let re = fancy_regex::Regex::new(&pattern).unwrap();
    re.replace_all(code, new_identifier).to_string()
}

fn hash_to_python_method_name(input: &str) -> String {
    // Create a SHA1 object
    let mut hasher = Sha1::new();

    // Write input string to it
    hasher.update(input.as_bytes());

    // Read hash digest and consume hasher
    let result = hasher.finalize();

    // Convert hash to hex string
    let hex_result = result
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect::<String>();

    // Ensure it starts with a valid character (underscore if necessary)
    let mut valid_method_name = if hex_result.chars().next().unwrap().is_numeric() {
        format!("_{}", hex_result)
    } else {
        hex_result
    };

    // Replace any invalid characters with an underscore
    valid_method_name = valid_method_name.replace(|c: char| !c.is_alphanumeric(), "_");

    // Truncate to a reasonable length for a method name (e.g., 30 characters)
    valid_method_name.truncate(30);

    valid_method_name
}

fn rename_triggerable_functions(report: &Report, code: &str) -> (String, HashMap<String, String>) {
    let mut hash_map = HashMap::new();
    let mut new_code = code.to_string();
    for (func_name, _) in &report.triggerable_functions {
        let hashed_name = hash_to_python_method_name(func_name);
        hash_map.insert(hashed_name.clone(), func_name.clone());
        new_code = replace_identifier(&new_code, func_name, &hashed_name);
        // Update the code in your Report or related structures if needed
        // For example, updating a map of code snippets:
        // report.triggerable_functions.get_mut(func_name).unwrap().code = new_code;
    }
    (new_code, hash_map)
}



#[cfg(test)]
mod tests {
    use std::time::Duration;
    use chumsky::prelude::any;
    use super::*;
    use crate::cells::{SupportedLanguage, TextRange};
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;
    use tokio::sync::Notify;
    use chidori_static_analysis::language::{InternalCallGraph, ReportTriggerableFunctions};
    use crate::execution::execution::execution_graph::ExecutionGraphSendPayload;
    use crate::execution::execution::execution_state::ExecutionStateEvaluation;
    use crate::execution::primitives::operation::OperationFnOutput;
    use crate::execution::primitives::serialized_value::ArchivedRkyvSerializedValue::Number;
    //     #[tokio::test]
    //     async fn test_source_code_run_py_success() {
    //         let source_code = String::from(
    //             r#"
    // from chidori import suspend
    //
    // def fun():
    //     return 42 + suspend()
    //         "#,
    //         );
    //         let result = source_code_run_python(source_code);
    //         // TODO: this should deserialize to a function pointer
    //         // assert_eq!(
    //         //     result.unwrap(),
    //         //     HashMap::from_iter(vec![("fun".to_string(), RkyvSerializedValue::Number(42),),])
    //     }

    #[tokio::test]
    async fn test_py_source_without_entrypoint() {
        println!("running A");
        let source_code = String::from(
            r#"
y = 42
x = 12 + y
li = [x, y]
        "#,
        );
        let result = source_code_run_python(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None, &None, &None).await;
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
                vec![]
            )
        );
    }

    #[tokio::test]
    async fn test_py_source_without_entrypoint_with_stdout() {
        println!("running B");
        let source_code = String::from(
            r#"
print("testing")
        "#,
        );
        let result = source_code_run_python(&ExecutionState::new_with_random_id(), &source_code, &RkyvSerializedValue::Null, &None, &None, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvSerializedValue::Object(HashMap::from_iter(vec![]))),
                vec![String::from("testing"), String::from("\n")],
                vec![]
            )
        );
    }

    #[tokio::test]
    async fn test_execution_of_internal_function() {
        let source_code = String::from(
            r#"
import chidori as ch

@ch.on_event("ex")
def example():
    a = 20
    return a
        "#,
        );
        let result = source_code_run_python(&ExecutionState::new_with_random_id(),
                                            &source_code,
                                            &RkyvSerializedValue::Null,
                                            &Some("example".to_string()),
                                            &None,
                                            &None,
        ).await;
        assert_eq!(result.unwrap(), (Ok(RkyvSerializedValue::Number(20)), vec![], vec![]));
    }

    #[tokio::test]
    async fn test_execution_of_internal_function_with_arguments() {
        let source_code = String::from(
            r#"
import chidori as ch

def example(x):
    a = 20 + x
    return a
        "#,
        );
        let result = source_code_run_python(&ExecutionState::new_with_random_id(),
                                            &source_code,
                                            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
                                            &Some("example".to_string()),
                                            &None,
                                            &None,
        ).await;
        assert_eq!(result.unwrap(), (Ok(RkyvSerializedValue::Number(25)), vec![], vec![]));
    }

    #[tokio::test]
    async fn test_execution_of_python_with_function_provided_via_cell() -> anyhow::Result<()> {
        let source_code = String::from(
            r#"
a = 20 + await demo()
        "#,
        );
        let mut state = ExecutionState::new_with_random_id();
        let id_a = Uuid::new_v4();
        let (state, _) = state.update_operation(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        def demo():
                            return 100
                        "#}),
            function_invocation: None,
        }, TextRange::default()), id_a)?;
        let result = source_code_run_python(&state,
                                            &source_code,
                                            &RkyvObjectBuilder::new()
                                                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                                                .build(),
                                            &None,
                                            &None,
                                            &None,
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new().insert_number("a", 120).build()),
                vec![],
                vec![]
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_running_async_function_dependency() -> anyhow::Result<()> {
        let mut state = ExecutionState::new_with_random_id();
        let id_a = Uuid::new_v4();
        let (state, _) = state.update_operation(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        import asyncio
                        async def demo():
                            await asyncio.sleep(1)
                            return 100
                        "#}),
            function_invocation: None,
        }, TextRange::default()), id_a)?;
        let result = source_code_run_python(&state,
                                            &String::from( r#"data = await demo()"#, ),
                                            &RkyvObjectBuilder::new()
                                                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))

                                                .build(),
                                            &None,
                                            &None,
                                            &None,
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new().insert_number("data", 100).build()),
                vec![],
                vec![]
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_chain_of_multiple_dependent_python_functions() -> anyhow::Result<()> {
        // TODO: this should validate that we can invoke a function that depends on another function
        let source_code = String::from(
            r#"
data = await demo()
        "#,
        );
        let mut state = ExecutionState::new_with_random_id();
        let id_a = Uuid::new_v4();
        let (mut state, _) = state.update_operation(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        import asyncio
                        async def demo():
                            await asyncio.sleep(1)
                            return 100 + await demo_second_function_call()
                        "#}),
            function_invocation: None,
        }, TextRange::default()), id_a)?;
        let id_b = Uuid::new_v4();
        let (state, _) = state.update_operation(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        import asyncio
                        async def demo_second_function_call():
                            await asyncio.sleep(1)
                            return 100
                        "#}),
            function_invocation: None,
        }, TextRange::default()), id_b)?;
        let result = source_code_run_python(&state,
                                            &source_code,
                                            &RkyvObjectBuilder::new()
                                                .build(),
                                            &None,
                                            &None,
                                            &None,
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new().insert_number("data", 200).build()),
                vec![],
                vec![]
            )
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_internal_function_invocation_chain_with_observability() -> anyhow::Result<()> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<ExecutionGraphSendPayload>(1028);
        let mut state_a = ExecutionState::new_with_graph_sender(Uuid::nil(), Arc::new(sender.clone()));
        let id_a = Uuid::new_v4();
        let (state, _) = state_a.update_operation(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        import asyncio
                        async def function_a():
                            await asyncio.sleep(1)
                            return 100

                        async def function_b():
                            await asyncio.sleep(1)
                            return 100 + await function_a()

                        async def function_c():
                            await asyncio.sleep(1)
                            return 100 + await function_b()
                        "#}),
            function_invocation: None,
        }, TextRange::default()), id_a)?;
        let source_code = String::from(
            r#"data = await function_c()"#,
        );
        let received_events = Arc::new(Mutex::new(Vec::new()));
        let received_events_clone = Arc::clone(&received_events);


        let cancellation_notify = Arc::new(Notify::new());
        let cancellation_notify_clone = cancellation_notify.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {
                        match receiver.try_recv() {
                            Ok((resulting_execution_state, oneshot)) => {
                                println!("Received event");
                                received_events_clone.lock().unwrap().push(resulting_execution_state.clone());
                                if let Some(oneshot) = oneshot {
                                    oneshot.send(()).unwrap();
                                }
                            },
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                                // No messages available
                            },
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                println!("===== Was DC'd");
                                // Handle the case where the sender has disconnected and no more messages will be received
                                break; // or handle it according to your application logic
                            }
                        }
                    }
                    _ = cancellation_notify_clone.notified() => {
                        println!("Task is notified to stop");
                        return;
                    }
                }

            }
        });
        let result = source_code_run_python(
            &state,
            &source_code,
            &RkyvObjectBuilder::new()
                .build(),
            &None,
            &None,
            &None,
        ).await;
        cancellation_notify.notify_one();
        assert_eq!(
            result.unwrap(),
            (
                Ok(RkyvObjectBuilder::new().insert_number("data", 300).build()),
                vec![],
                vec![]
            )
        );

        // Assertions for the received events
        let events = received_events.lock().unwrap();
        assert!(!events.is_empty(), "Should have received execution events for each function call");

        // Check for specific events (adjust based on your expected execution flow)
        assert!(events.iter().any(|e| matches!(e, ExecutionStateEvaluation::Executing(..))),
                "Should have received at least one Executing event");

        // Helper function to check OperationFnOutput
        fn check_operation_output(output: &Arc<OperationFnOutput>, expected_value: i64) -> bool {
            match output.as_ref() {
                OperationFnOutput { has_error: false, execution_state: None, output: output_value, stdout, stderr } => {
                    matches!(output_value, Ok(RkyvSerializedValue::Number(n)) if *n == expected_value as i32)
                        && stdout.is_empty()
                        && stderr.is_empty()
                },
                _ => false
            }
        }

        // Check for Complete events
        assert!(
            matches!(events[3], ExecutionStateEvaluation::Complete(ref state) if {
        state.state.values().next().map_or(false, |output| check_operation_output(output, 100))
    }),
            "First Complete event should have output 100"
        );

        assert!(
            matches!(events[4], ExecutionStateEvaluation::Complete(ref state) if {
        state.state.values().next().map_or(false, |output| check_operation_output(output, 200))
    }),
            "Second Complete event should have output 200"
        );

        assert!(
            matches!(events[5], ExecutionStateEvaluation::Complete(ref state) if {
        state.state.values().next().map_or(false, |output| check_operation_output(output, 300))
    }),
            "Third Complete event should have output 300"
        );

        Ok(())
    }


    #[ignore]
    #[tokio::test]
    async fn test_running_sync_unit_test() {
        let source_code = String::from(
            r#"
import unittest

def addTwo(x):
    return x + 2

class TestMarshalledValues(unittest.TestCase):
    def test_addTwo(self):
        self.assertEqual(addTwo(2), 4)

unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))
        "#,
        );
        let result = source_code_run_python(&ExecutionState::new_with_random_id(),
                                            &source_code,
                                            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
                                            &None,
                                            &None,
                                            &None,
        ).await;
        let (result, _, stderr) = result.unwrap();
        dbg!(&stderr);
        assert_eq!(stderr.iter().filter(|x| x.contains("Ran 1 test")).count(), 1);
        assert_eq!(stderr.iter().filter(|x| x.contains("OK")).count(), 1);
    }

    #[ignore]
    #[tokio::test]
    async fn test_running_async_unit_test() {
        let source_code = String::from(
            r#"
import unittest
import asyncio

async def add(a, b):
    await asyncio.sleep(1)
    return a + b

class TestMarshalledValues(unittest.IsolatedAsyncioTestCase):
    async def test_run_prompt(self):
        self.assertEqual(await add(2, 2), 4)

unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))
        "#,
        );
        let result = source_code_run_python(&ExecutionState::new_with_random_id(),
                                            &source_code,
                                            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
                                            &None,
                                            &None,
                                            &None,
        ).await;
        let (result, _, stderr) = result.unwrap();
        dbg!(&stderr);
        assert_eq!(stderr.iter().filter(|x| x.contains("Ran 1 test")).count(), 1);
        assert_eq!(stderr.iter().filter(|x| x.contains("OK")).count(), 1);
    }

    // TODO: the expected behavior is that as we execute the function again and again from another location, the state mutates
    #[ignore]
    #[tokio::test]
    async fn test_execution_of_internal_function_mutating_internal_state() {
        let source_code = String::from(
            r#"
a = 0
def example(x):
    global a
    a += 1
    return a
        "#,
        );
        let result = source_code_run_python(&ExecutionState::new_with_random_id(),
                                            &source_code,
                                            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
                                            &Some("example".to_string()),
                                            &None,
                                            &None,
        ).await;
        assert_eq!(result.unwrap(), (Ok(RkyvSerializedValue::Number(1)), vec![], vec![]));
        let result = source_code_run_python(&ExecutionState::new_with_random_id(),
                                            &source_code,
                                            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
                                            &Some("example".to_string()),
                                            &None,
                                            &None,
        ).await;
        assert_eq!(result.unwrap(), (Ok(RkyvSerializedValue::Number(2)), vec![], vec![]));
    }


    #[tokio::test]
    async fn test_error_handling_failure_to_parse() {
        let source_code = String::from(
            r#"
fn example():
    return 42
        "#,
        );
        let result = source_code_run_python(
            &ExecutionState::new_with_random_id(),
            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &Some("example".to_string()),
            &None,
            &None,
        ).await;
        match result {
            Ok(_) => {panic!("Must return error.")}
            Err(e) => {
                assert_eq!(e.to_string(), String::from("Parse error at offset 4 in <embedded>: invalid syntax. Got unexpected token 'example'. Source: \nfn example():\n    return 42\n        "));
            }
        }
    }


    #[tokio::test]
    async fn test_error_handling_exception_thrown() {
        let source_code = String::from(
            r#"
raise ValueError("Raising a python error")
        "#,
        );
        let result = source_code_run_python(
            &ExecutionState::new_with_random_id(),
            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &Some("example".to_string()),
            &None,
            &None,
        ).await;
        match result {
            Ok(_) => {panic!("Must return error.")}
            Err(e) => {
                assert_eq!(e.to_string(), String::from("ValueError: Raising a python error"));
            }
        }
    }

    #[tokio::test]
    async fn test_identifier_replacement() {
        let test_cases = vec![
            ("x", "y", "def func(x): return x", "def func(y): return y"),
            ("func", "new_func", "def func(): pass\nfunc()", "def new_func(): pass\nnew_func()"),
            ("var", "variable", "var = 5\nvar_name = 10", "variable = 5\nvar_name = 10"),
            ("_private", "_hidden", "_private = 1\nnot_private = 2", "_hidden = 1\nnot_private = 2"),
            ("MAX_VALUE", "MAXIMUM", "MAX_VALUE = 100\nMAX_VALUE_LIMIT = 200", "MAXIMUM = 100\nMAX_VALUE_LIMIT = 200"),
            ("i", "index", "for i in range(10): print(i)", "for index in range(10): print(index)"),
            ("data", "info", "data = [1, 2, 3]\nmore_data = [4, 5, 6]", "info = [1, 2, 3]\nmore_data = [4, 5, 6]"),
            ("calculate", "compute", "def calculate(x): return calculate(x-1)", "def compute(x): return compute(x-1)"),
            ("temp", "temporary", "temp = 98.6\ntemperature = 100", "temporary = 98.6\ntemperature = 100"),
            ("log", "logger", "import log\nlog.info('message')", "import logger\nlogger.info('message')"),
            ("str", "string", "str_value = str(42)", "str_value = string(42)"),
            ("dict", "dictionary", "my_dict = dict()\ndict_obj = {}", "my_dict = dictionary()\ndict_obj = {}"),
            ("print", "display", "print('Hello')\nprinter = None", "display('Hello')\nprinter = None"),
            ("sum", "total", "sum([1, 2, 3])\nsum_up = lambda x: x", "total([1, 2, 3])\nsum_up = lambda x: x"),
            ("Exception", "Error", "raise Exception('error')\nExceptionHandler", "raise Error('error')\nExceptionHandler"),
        ];

        for (old, new, input, expected) in test_cases {
            let result = replace_identifier(input, old, new);
            assert_eq!(result, expected, "Failed to replace '{}' with '{}'", old, new);
        }

    }

    #[test]
    fn test_hash_to_python_method_name() {
        let python_method_name = "example_method_name";
        let hashed_name_a = hash_to_python_method_name(python_method_name);
        let hashed_name_b = hash_to_python_method_name(python_method_name);

        // Verify that the hashed name is deterministic and meets requirements
        assert_eq!(hashed_name_a, hashed_name_b);
    }

    #[test]
    fn test_hash_to_python_method_name_numeric_start() {
        let python_method_name = "123example_method_name";
        let hashed_name = hash_to_python_method_name(python_method_name);

        // Verify that the hashed name starts with an underscore if the hash starts with a digit
        assert!(hashed_name.starts_with('_'));
    }

    #[test]
    fn test_hash_to_python_method_name_length() {
        let python_method_name = "a_very_long_method_name_that_exceeds_thirty_characters";
        let hashed_name = hash_to_python_method_name(python_method_name);

        // Verify that the hashed name is truncated to 30 characters
        assert!(hashed_name.len() <= 30);
    }

    use super::*;

    #[test]
    fn test_rename_triggerable_functions() {
        // Prepare a dummy Report with some triggerable functions
        let mut report = Report {
            internal_call_graph: InternalCallGraph::default(),
            cell_exposed_values: HashMap::new(),
            cell_depended_values: HashMap::new(),
            triggerable_functions: {
                let mut map = HashMap::new();
                map.insert("example_function".to_string(), ReportTriggerableFunctions::default());
                map.insert("another_function".to_string(), ReportTriggerableFunctions::default());
                map
            },
        };

        let code = r#"
def example_function():
    pass

def another_function():
    pass
"#;

        let (new_code, hash_map ) = rename_triggerable_functions(&report, code);
        let example_function_renamed = hash_to_python_method_name("example_function");
        let another_function_renamed = hash_to_python_method_name("another_function");
        assert_eq!(new_code,
                   format!(r#"
def {}():
    pass

def {}():
    pass
"#, example_function_renamed, another_function_renamed));

        assert_eq!(hash_map.get(&hash_to_python_method_name("example_function")).unwrap(), "example_function");
        assert_eq!(hash_map.get(&hash_to_python_method_name("another_function")).unwrap(), "another_function");
    }

}
