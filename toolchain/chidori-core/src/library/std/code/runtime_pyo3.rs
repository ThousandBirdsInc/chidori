use std::io::{self, Write};

use std::pin::Pin;
use chidori_static_analysis::language::python::parse::{
    build_report, extract_dependencies_python,
};

use futures_util::FutureExt;
use pyo3::prelude::*;
use pyo3::types::{IntoPyDict, PyCFunction, PyDict, PyList, PySet, PyTuple};
use std::sync::mpsc::{self, Sender};

// use rustpython::vm::{pymodule, PyPayload, PyResult, VirtualMachine};
// use rustpython_vm as vm;
// use rustpython_vm::builtins::{PyBool, PyDict, PyInt, PyList, PyStr};

use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};
use std::collections::HashMap;
use std::future::Future;
use std::mem;
use std::sync::{Arc, Mutex};
use anyhow::Error;
use once_cell::sync::OnceCell;
use pyo3_asyncio::generic;
use tokio::runtime::Runtime;
use chidori_static_analysis::language::Report;
// use rustpython_vm::PyObjectRef;
use crate::cells::{CellTypes, CodeCell, LLMPromptCell};
use crate::execution::execution::ExecutionState;


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
            "NoneType" => {
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



use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicUsize, Ordering};
use dashmap::DashMap;

static SOURCE_CODE_RUN_COUNTER: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
static CURRENT_PYTHON_EXECUTION_ID: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));

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


pub async fn source_code_run_python(
    execution_state: &ExecutionState,
    source_code: &String,
    payload: &RkyvSerializedValue,
    function_invocation: &Option<String>,
) -> anyhow::Result<(RkyvSerializedValue, Vec<String>, Vec<String>)> {
    let exec_id = increment_source_code_run_counter();
    pyo3::prepare_freethreaded_python();
    let (sender_stdout, receiver_stdout) = mpsc::channel();
    let (sender_stderr, receiver_stderr) = mpsc::channel();

    let dependencies = extract_dependencies_python(&source_code);
    let report = build_report(&dependencies);

    let result =  Python::with_gil(|py| {
        // TODO: this was causing a deadlock
        // let current_event_loop = pyo3_asyncio::tokio::get_current_loop(py);

        // Initialize our event loop if one is not already established
        // let event_loop = if current_event_loop.is_err() {
        //     let asyncio = py.import("asyncio")?;
        //     let event_loop = asyncio.call_method0("new_event_loop")?;
        //     asyncio.call_method1("set_event_loop", (event_loop,))?;
        //     event_loop
        // } else {
        //     current_event_loop.unwrap()
        // };

        // Configure locals and globals passed to evaluation
        let globals = PyDict::new(py);
        let execution_state = Arc::new(Mutex::new(execution_state.clone()));
        create_function_shims(&execution_state, &report, py, globals)?;


        let sys = py.import("sys")?;

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
        let mut initial_source_code = format!("import sys\nsys.stdout.set_exec_id({exec_id})\nsys.stderr.set_exec_id({exec_id})", exec_id=exec_id);
        initial_source_code.push_str("\n");
        initial_source_code.push_str(&source_code.clone());

        // If any instances of these lines are located, skip wrapping anything because the code will initialize its own async runtime.
        let does_contain_async_runtime = initial_source_code
            .lines()
            .any(|line| line.contains("asyncio.run") || line.contains("unittest.IsolatedAsyncioTestCase") || line.contains("loadTestsFromTestCase"));

        let mut complete_code = if does_contain_async_runtime {
            // If we have an async function, we don't need to wrap it in an async function
            initial_source_code
        } else {
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
                initial_source_code.push_str("\n");
                initial_source_code.push_str(&format!(
                    r#"globals()["{name}"] = {name}"#,
                    name = name
                ));
            }
            let indent_all_source_code = initial_source_code.lines().map(|line| format!("    {}", line)).collect::<Vec<_>>().join("\n");
            // Wrap all of our code in a top level async wrapper
            format!(r#"
import asyncio
import chidori
async def __wrapper():
{}
asyncio.run(__wrapper())
        "#, indent_all_source_code)
        };
        complete_code.push_str("\n");
        complete_code.push_str("import sys\nsys.stdout.flush()\nsys.stderr.flush()");
        complete_code.push_str("\n");

        // Important: this is the point of initial execution of the source code
        py.run(&complete_code, Some(globals), None).unwrap();



        // With the source environment established, we can now invoke specific methods provided by this node
        return match function_invocation {
            None => {
                py.allow_threads(move || {
                    Ok(Box::pin(async move {
                        let output_c = PYTHON_OUTPUT_MAP.clone();
                        if let Some((k, output_c)) = output_c.remove(&exec_id) {
                            RkyvSerializedValue::Object(output_c.into_iter().collect())
                        } else {
                            RkyvSerializedValue::Object(HashMap::new())
                        }
                    }) as Pin<Box<dyn Future<Output = RkyvSerializedValue> + Send>>)
                })
            }
            Some(name) => {
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

                    let result = py_func.call(args, Some(kwargs))?;
                    if result.get_type().name().unwrap() == "coroutine" {
                        // If the function is a coroutine, we need to await it
                        let f = pyo3_asyncio::tokio::into_future(result)?;
                        Ok(Box::pin(async move {
                            // TODO: these should return Result so that we don't unwrap here
                            let py_any = &f.await.unwrap();
                            Python::with_gil(|py| {
                                let py_any: &PyAny = py_any.as_ref(py);
                                pyany_to_rkyv_serialized_value(py_any)
                            })
                        }) as Pin<Box<dyn Future<Output = RkyvSerializedValue> + Send>>)
                    } else {
                        let result: PyObject = result.into_py(py);
                        Ok(Box::pin(async move {
                            Python::with_gil(|py| {
                                let py_any: &PyAny = result.as_ref(py);
                                pyany_to_rkyv_serialized_value(py_any)
                            })
                        }) as Pin<Box<dyn Future<Output=RkyvSerializedValue> + Send>>)
                    }
                } else {
                    Err(anyhow::anyhow!("Function not found"))
                }
            }
        }
    });
    if let Ok(result) = result {
        let awaited_result = result.await;
        let (_, output_stdout) = PYTHON_LOGGING_BUFFER_STDOUT.remove(&exec_id).unwrap_or((0, vec![]));
        let (_, output_stderr) = PYTHON_LOGGING_BUFFER_STDERR.remove(&exec_id).unwrap_or((0, vec![]));
        Ok((awaited_result, output_stdout, output_stderr))
    } else {
        return Err(anyhow::anyhow!("No result"));
    }
}

fn create_function_shims(execution_state_handle: &Arc<Mutex<ExecutionState>>, report: &Report, py: Python, globals: &PyDict) -> Result<(), Error> {
    // Create shims for functions that are referred to, we look at what functions are being provided
    // and create shims for matches between the function name provided and the identifiers referred to.

    let function_names = {
        let execution_state_handle = execution_state_handle.clone();
        let mut exec_state = execution_state_handle.lock().unwrap();
        exec_state.function_name_to_operation_id.keys().cloned().collect::<Vec<_>>()
    };
    for function_name in function_names {
        if report
            .cell_depended_values
            .contains_key(&function_name)
        {
            let clone_function_name = function_name.clone();
            let execution_state_handle = execution_state_handle.clone();
            let closure_callable = PyCFunction::new_closure(
                py,
                None, // name
                None, // doc
                move |args: &PyTuple, kwargs: Option<&PyDict>| -> PyResult<PyObject> {
                    let total_arg_payload = python_args_to_rkyv(args, kwargs)?;
                    let clone_function_name = clone_function_name.clone();
                    let py = args.py();

                    // All function calls across cells are forced to be async
                    // TODO: clone the execution state, sending it to the execution graph and use the dispatch method to execute the code
                    // TODO: we want to fetch the execution state at the time this function is called
                    // TODO: this needs an Arc to something that holds our latest execution state

                    // TODO: replace the execution state here and notify the execution graph that we have a new execution state generated
                    //       HOW do we do this in a transactional way?
                    let mut new_exec_state = {
                        let mut exec_state = execution_state_handle.lock().unwrap();
                        let mut new_exec_state = exec_state.clone();
                        std::mem::swap(&mut *exec_state, &mut new_exec_state);
                        new_exec_state
                    };
                    // TODO: update the state with the args we're about to execute

                    pyo3_asyncio::tokio::future_into_py(py, async move {
                        // TODO: await here, before we execute the dispatch, pausing before running the next operation
                        let (result, execution_state) = new_exec_state.dispatch(&clone_function_name, total_arg_payload).await;
                        // TODO: await here, after we execute the dispatch, pausing before running the next operation
                        PyResult::Ok(Python::with_gil(|py| rkyv_serialized_value_to_pyany(py, &result)))
                    }).map(|x| x.into())
                },
            )?;
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::SupportedLanguage;
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;

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
        let result = source_code_run_python(&ExecutionState::new(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvSerializedValue::Object(HashMap::from_iter(vec![
                    ("y".to_string(), RkyvSerializedValue::Number(42),),
                    ("x".to_string(), RkyvSerializedValue::Number(54),),
                    (
                        "li".to_string(),
                        RkyvSerializedValue::Array(vec![
                            RkyvSerializedValue::Number(54),
                            RkyvSerializedValue::Number(42)
                        ]),
                    )
                ])),
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
        let result = source_code_run_python(&ExecutionState::new(), &source_code, &RkyvSerializedValue::Null, &None).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvSerializedValue::Object(HashMap::from_iter(vec![])),
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
        let result = source_code_run_python(&ExecutionState::new(),
                                            &source_code,
            &RkyvSerializedValue::Null,
            &Some("example".to_string()),
        ).await;
        assert_eq!(result.unwrap(), (RkyvSerializedValue::Number(20), vec![], vec![]));
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
        let result = source_code_run_python(&ExecutionState::new(),
                                            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &Some("example".to_string()),
        ).await;
        assert_eq!(result.unwrap(), (RkyvSerializedValue::Number(25), vec![], vec![]));
    }

    #[tokio::test]
    async fn test_execution_of_python_with_function_provided_via_cell() {
        let source_code = String::from(
            r#"
a = 20 + await demo()
        "#,
        );
        let mut state = ExecutionState::new();
        let (state, _) = state.update_op(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        def demo():
                            return 100
                        "#}),
            function_invocation: None,
        }), Some(0));
        let result = source_code_run_python(&state,
                                            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &None,
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvObjectBuilder::new().insert_number("a", 120).build(),
                vec![],
                vec![]
            )
        );
    }

    #[tokio::test]
    async fn test_running_async_function_dependency() {
        let source_code = String::from(
            r#"
data = await demo()
        "#,
        );
        let mut state = ExecutionState::new();
        let (state, _) = state.update_op(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        import asyncio
                        async def demo():
                            await asyncio.sleep(1)
                            return 100
                        "#}),
            function_invocation: None,
        }), Some(0));
        let result = source_code_run_python(&state,
                                            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))

                .build(),
            &None,
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvObjectBuilder::new().insert_number("data", 100).build(),
                vec![],
                vec![]
            )
        );
    }

    #[tokio::test]
    async fn test_chain_of_multiple_dependent_python_functions() {
        // TODO: this should validate that we can invoke a function that depends on another function
        let source_code = String::from(
            r#"
data = await demo()
        "#,
        );
        let mut state = ExecutionState::new();
        let (mut state, _) = state.update_op(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        import asyncio
                        async def demo():
                            await asyncio.sleep(1)
                            return 100 + await demo_second_function_call()
                        "#}),
            function_invocation: None,
        }), Some(0));
        let (state, _) = state.update_op(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        import asyncio
                        async def demo_second_function_call():
                            await asyncio.sleep(1)
                            return 100
                        "#}),
            function_invocation: None,
        }), Some(1));
        let result = source_code_run_python(&state,
                                            &source_code,
            &RkyvObjectBuilder::new()
                .build(),
            &None,
        ).await;
        assert_eq!(
            result.unwrap(),
            (
                RkyvObjectBuilder::new().insert_number("data", 200).build(),
                vec![],
                vec![]
            )
        );
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
        let result = source_code_run_python(&ExecutionState::new(),
                                            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &None,
        ).await;
        let (result, _, stderr) = result.unwrap();
        assert_eq!(stderr.iter().filter(|x| x.contains("Ran 1 test")).count(), 1);
        assert_eq!(stderr.iter().filter(|x| x.contains("OK")).count(), 1);
    }

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
        let result = source_code_run_python(&ExecutionState::new(),
                                            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
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
        let result = source_code_run_python(&ExecutionState::new(),
                                            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &Some("example".to_string()),
        ).await;
        assert_eq!(result.unwrap(), (RkyvSerializedValue::Number(1), vec![], vec![]));
        let result = source_code_run_python(&ExecutionState::new(),
                                            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &Some("example".to_string()),
        ).await;
        assert_eq!(result.unwrap(), (RkyvSerializedValue::Number(2), vec![], vec![]));
    }
}
