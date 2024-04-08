
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

// TODO: validate suspension and resumption of execution based on a method that we provide
// TODO: we want to capture the state of the interpreter and resume it later when we invoke the target function
// TODO: need to be able to capture output results

#[pyclass]
struct LoggingToChannel {
    sender: Sender<String>,
}

impl LoggingToChannel {
    fn new(sender: Sender<String>) -> Self {
        LoggingToChannel { sender }
    }
}

#[pymethods]
impl LoggingToChannel {
    fn write(&mut self, data: &str) {
        let _ = self.sender.send(data.to_string());
        // You might want to handle the error in real code
    }

    fn flush(&mut self) {
    }
}


async fn execute_async_block(total_arg_payload: RkyvSerializedValue, cell: &CellTypes, clone_function_name: &str) -> RkyvSerializedValue {
    // modify code cell to indicate execution of the target function
    // reconstruction of the cell
    let mut op = match cell {
        CellTypes::Code(c) => {
            let mut c = c.clone();
            c.function_invocation =
                Some(clone_function_name.to_string());
            crate::cells::code_cell::code_cell(&c)
        }
        CellTypes::Prompt(c) => {
            let mut c = c.clone();
            match c {
                LLMPromptCell::Chat{ref mut function_invocation, ..} => {
                    *function_invocation = true;
                    crate::cells::llm_prompt_cell::llm_prompt_cell(&c)
                }
                _ => {
                    crate::cells::llm_prompt_cell::llm_prompt_cell(&c)
                }
            }
        }
        _ => {
            unreachable!("Unsupported cell type");
        }
    };

    // invocation of the operation
    // TODO: the total arg payload here does not include necessary function calls for this cell itself
    op.execute(&ExecutionState::new(), total_arg_payload, None).await
}


pub async fn source_code_run_python(
    execution_state: &ExecutionState,
    source_code: &String,
    payload: &RkyvSerializedValue,
    function_invocation: &Option<String>,
) -> anyhow::Result<(RkyvSerializedValue, Vec<String>, Vec<String>)> {
    pyo3::prepare_freethreaded_python();
    let (sender_stdout, receiver_stdout) = mpsc::channel();
    let (sender_stderr, receiver_stderr) = mpsc::channel();

    let dependencies = extract_dependencies_python(&source_code);
    let report = build_report(&dependencies);

    let result =  Python::with_gil(|py| {
        let current_event_loop = pyo3_asyncio::tokio::get_current_loop(py);

        // Initialize our event loop if one is not established
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
        create_function_shims(&execution_state, &report, py, globals)?;

        let output = Arc::new(Mutex::new(HashMap::new()));

        // Create new module
        let chidori_module = PyModule::new(py, "chidori")?;
        chidori_module.add_function(wrap_pyfunction!(on_event, chidori_module)?)?;
        chidori_module.add_function(wrap_pyfunction!(identity_function, chidori_module)?)?;
        let output_clone = output.clone();
        let chidori_set_value = PyCFunction::new_closure(
            py,
            None,
            None,
            move |args: &PyTuple, kwargs: Option<&PyDict>| {
                if args.len() == 2 {
                    let name: String = args.get_item(0).unwrap().extract::<String>().unwrap();
                    let mut output_lock = output_clone.lock().unwrap();
                    let value = args.get_item(1).unwrap(); // Keep as PyAny
                    output_lock.insert(name, pyany_to_rkyv_serialized_value(value));
                }
            },
        )?;
        chidori_module.add("set_value", chidori_set_value)?;

        // Import and get sys.modules
        let sys = py.import("sys")?;
        let py_modules = sys.getattr("modules")?;

        // Insert our custom module
        py_modules.set_item("chidori", chidori_module)?;

        // Set up capture of stdout from python process and storing it into a Vec
        let stdout_capture = LoggingToChannel::new(sender_stdout);
        let stdout_capture_py = stdout_capture.into_py(py);
        let stderr_capture = LoggingToChannel::new(sender_stderr);
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
        let mut initial_source_code = source_code.clone();

        // If any instances of these lines are located, skip wrapping anything because the code will initialize its own async runtime.
        let does_contain_async_runtime = initial_source_code
            .lines()
            .any(|line| line.contains("asyncio.run") || line.contains("unittest.IsolatedAsyncioTestCase") || line.contains("loadTestsFromTestCase"));

        let complete_code = if does_contain_async_runtime {
            // If we have an async function, we don't need to wrap it in an async function
            initial_source_code
        } else {
            for (name, report_item) in &report.cell_exposed_values {
                initial_source_code.push_str("\n");
                initial_source_code.push_str(&format!(
                    r#"chidori.set_value("{name}", {name})"#,
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

        // Important: this is the point of initial execution of the source code
        py.run(&complete_code, Some(globals), None).unwrap();

        // With the source environment established, we can now invoke specific methods provided by this node
        return match function_invocation {
            None => {
                let output_lock = output.lock().unwrap().clone();
                Ok(Box::pin(async move {
                    RkyvSerializedValue::Object(output_lock)
                }) as Pin<Box<dyn Future<Output = RkyvSerializedValue> + Send>>)
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
                        }) as Pin<Box<dyn Future<Output = RkyvSerializedValue> + Send>>)
                    }
                } else {
                    Err(anyhow::anyhow!("Function not found"))
                }
            }
        }
    });
    let output_stdout: Vec<String> = receiver_stdout.try_iter().collect();
    let output_stderr: Vec<String> = receiver_stderr.try_iter().collect();
    if let Ok(result) = result {
        Ok((result.await, output_stdout, output_stderr))
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
                    let mut exec_state = execution_state_handle.lock().unwrap();
                    let mut new_exec_state = exec_state.clone();
                    // TODO: update the state with the args we're about to execute

                    std::mem::swap(&mut *exec_state, &mut new_exec_state);


                    pyo3_asyncio::tokio::future_into_py(py, async move {
                        // TODO: await here, before we execute the dispatch, pausing before running the next operation
                        let (result, execution_state) = new_exec_state.dispatch(&clone_function_name, total_arg_payload).await;
                        // let result = execute_async_block(total_arg_payload, &cell, &clone_function_name).await;
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
