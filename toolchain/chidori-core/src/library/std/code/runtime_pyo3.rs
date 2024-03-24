use chidori_static_analysis::language::python::parse::{
    build_report, extract_dependencies_python,
};

use pyo3::prelude::*;
use pyo3::types::{IntoPyDict, PyCFunction, PyDict, PyList, PySet, PyTuple};
use std::sync::mpsc::{self, Sender};

// use rustpython::vm::{pymodule, PyPayload, PyResult, VirtualMachine};
// use rustpython_vm as vm;
// use rustpython_vm::builtins::{PyBool, PyDict, PyInt, PyList, PyStr};

use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};
use std::collections::HashMap;
// use rustpython_vm::PyObjectRef;
use crate::cells::{CellTypes, CodeCell};

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
                println!("Py03 marshalling unsupported type: {}", x);
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
        // Handle other enum variants accordingly...
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
struct LoggingStdout {
    sender: Sender<String>,
}

impl LoggingStdout {
    fn new(sender: Sender<String>) -> Self {
        LoggingStdout { sender }
    }
}

#[pymethods]
impl LoggingStdout {
    fn write(&mut self, data: &str) {
        let _ = self.sender.send(data.to_string());
        // You might want to handle the error in real code
    }
}

pub fn source_code_run_python(
    source_code: &String,
    payload: &RkyvSerializedValue,
    function_invocation: &Option<String>,
) -> anyhow::Result<(RkyvSerializedValue, Vec<String>)> {
    pyo3::prepare_freethreaded_python();
    let (sender, receiver) = mpsc::channel();

    let dependencies = extract_dependencies_python(&source_code);
    let report = build_report(&dependencies);

    return Python::with_gil(|py| {
        // Configure locals and globals passed to evaluation
        let locals = PyDict::new(py);
        let globals = PyDict::new(py);

        // Create shims for functions that are referred to, we look at what functions are being provided
        // and create shims for matches between the function name provided and the identifiers referred to.
        if let RkyvSerializedValue::Object(ref payload_map) = payload {
            if let Some(RkyvSerializedValue::Object(functions_map)) = payload_map.get("functions") {
                for (function_name, value) in functions_map {
                    let clone_function_name = function_name.clone();
                    // TODO: handle llm cells invoked as functions by name
                    if let RkyvSerializedValue::Cell(cell) = value.clone() {
                        if let CellTypes::Code(CodeCell { .. }) = &cell
                        {
                            if report
                                .cell_depended_values
                                .contains_key(&clone_function_name)
                            {
                                let closure_callable = PyCFunction::new_closure(
                                    py,
                                    None,
                                    None,
                                    move |args: &PyTuple, kwargs: Option<&PyDict>| {
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
                                        };

                                        // modify code cell to indicate execution of the target function
                                        // reconstruction of the cell
                                        let mut op = match &cell {
                                            CellTypes::Code(c) => {
                                                let mut c = c.clone();
                                                c.function_invocation =
                                                    Some(clone_function_name.clone());
                                                crate::cells::code_cell::code_cell(&c)
                                            }
                                            CellTypes::Prompt(c) => {
                                                crate::cells::llm_prompt_cell::llm_prompt_cell(&c)
                                            }
                                            _ => {
                                                unreachable!("Unsupported cell type");
                                            }
                                        };

                                        // invocation of the operation
                                        let result = op.execute(total_arg_payload.build(), None);

                                        // Conversion back to python types
                                        let py = args.py();
                                        PyResult::Ok(rkyv_serialized_value_to_pyany(py, &result))
                                    },
                                )?;
                                globals.set_item(function_name.clone(), closure_callable);
                            }
                        }
                    }
                }
            }
        }

        // Create new module
        let chidori_module = PyModule::new(py, "chidori")?;
        chidori_module.add_function(wrap_pyfunction!(on_event, chidori_module)?)?;
        chidori_module.add_function(wrap_pyfunction!(identity_function, chidori_module)?)?;

        // Import and get sys.modules
        let sys = py.import("sys")?;
        let py_modules = sys.getattr("modules")?;

        // Insert foo into sys.modules
        py_modules.set_item("chidori", chidori_module)?;

        // Set up capture of stdout from python process and storing it into a Vec
        let stdout_capture = LoggingStdout::new(sender);
        let stdout_capture_py = stdout_capture.into_py(py);

        sys.setattr("stdout", stdout_capture_py)?;

        if let RkyvSerializedValue::Object(ref payload_map) = payload {
            if let Some(RkyvSerializedValue::Object(globals_map)) = payload_map.get("globals") {
                for (key, value) in globals_map {
                    let py_value = rkyv_serialized_value_to_pyany(py, value); // Implement this function to convert RkyvSerializedValue to PyObject
                    globals.set_item(key, py_value)?;
                }
            }
        }

        // Important: this is the point of initial execution of the source code
        py.run(&source_code, Some(globals), Some(locals)).unwrap();

        // With the source environment established, we can now invoke specific methods provided by this node
        return match function_invocation {
            None => {
                let mut result_map = HashMap::new();
                for (name, report_item) in &report.cell_exposed_values {
                    let local = locals.get_item(name);
                    if let Ok(Some(local)) = local {
                        let parsed = pyany_to_rkyv_serialized_value(local);
                        result_map.insert(name.clone(), parsed);
                    }
                }
                let output: Vec<String> = receiver.try_iter().collect();
                Ok((RkyvSerializedValue::Object(result_map), output))
            }
            Some(name) => {
                let local = locals.get_item(name)?;
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
                    let output: Vec<String> = receiver.try_iter().collect();
                    Ok((pyany_to_rkyv_serialized_value(result), output))
                } else {
                    Err(anyhow::anyhow!("Function not found"))
                }
            }
        };
    });
}

// #[pymodule]
// mod rust_py_module {
//     use super::*;
//
//     #[pyfunction]
//     fn suspend(_vm: &VirtualMachine) -> PyResult<usize> {
//         // TODO: we can get the frame and we can get the locals and globals
//         // TODO: question is if we can store that frame somehow
//         // TODO: and can we resume it later
//         // dbg!(vm.current_frame());
//         // dbg!(vm.current_locals());
//         // dbg!(vm.current_globals());
//         println!("suspension_function");
//         Ok(1)
//     }
// }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::SupportedLanguage;
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;

    //     #[test]
    //     fn test_source_code_run_py_success() {
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

    #[test]
    fn test_py_source_without_entrypoint() {
        let source_code = String::from(
            r#"
y = 42
x = 12 + y
li = [x, y]
        "#,
        );
        let result = source_code_run_python(&source_code, &RkyvSerializedValue::Null, &None);
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
                vec![]
            )
        );
    }

    #[test]
    fn test_py_source_without_entrypoint_with_stdout() {
        let source_code = String::from(
            r#"
print("testing")
        "#,
        );
        let result = source_code_run_python(&source_code, &RkyvSerializedValue::Null, &None);
        assert_eq!(
            result.unwrap(),
            (
                RkyvSerializedValue::Object(HashMap::from_iter(vec![])),
                vec![String::from("testing"), String::from("\n")]
            )
        );
    }

    #[test]
    fn test_execution_of_internal_function() {
        let source_code = String::from(
            r#"
import chidori as ch

@ch.on_event("ex")
def example():
    a = 20
    return a
        "#,
        );
        let result = source_code_run_python(
            &source_code,
            &RkyvSerializedValue::Null,
            &Some("example".to_string()),
        );
        assert_eq!(result.unwrap(), (RkyvSerializedValue::Number(20), vec![]));
    }

    #[test]
    fn test_execution_of_internal_function_with_arguments() {
        let source_code = String::from(
            r#"
import chidori as ch

def example(x):
    a = 20 + x
    return a
        "#,
        );
        let result = source_code_run_python(
            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &Some("example".to_string()),
        );
        assert_eq!(result.unwrap(), (RkyvSerializedValue::Number(25), vec![]));
    }

    #[test]
    fn text_execution_of_python_with_function_provided_via_cell() {
        let source_code = String::from(
            r#"
a = 20 + demo()
        "#,
        );
        let result = source_code_run_python(
            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .insert_object(
                    "functions",
                    RkyvObjectBuilder::new().insert_value(
                        "demo",
                        RkyvSerializedValue::Cell(CellTypes::Code(CodeCell {
                            name: None,
                            language: SupportedLanguage::PyO3,
                            source_code: String::from(indoc! {r#"
                        def demo():
                            return 100
                        "#}),
                            function_invocation: None,
                        })),
                    ),
                )
                .build(),
            &None,
        );
        assert_eq!(
            result.unwrap(),
            (
                RkyvObjectBuilder::new().insert_number("a", 120).build(),
                vec![]
            )
        );
    }

    // TODO: the expected behavior is that as we execute the function again and again from another location, the state mutates
    #[ignore]
    #[test]
    fn test_execution_of_internal_function_mutating_internal_state() {
        let source_code = String::from(
            r#"
a = 0
def example(x):
    global a
    a += 1
    return a
        "#,
        );
        let result = source_code_run_python(
            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &Some("example".to_string()),
        );
        assert_eq!(result.unwrap(), (RkyvSerializedValue::Number(1), vec![]));
        let result = source_code_run_python(
            &source_code,
            &RkyvObjectBuilder::new()
                .insert_object("args", RkyvObjectBuilder::new().insert_number("0", 5))
                .build(),
            &Some("example".to_string()),
        );
        assert_eq!(result.unwrap(), (RkyvSerializedValue::Number(2), vec![]));
    }
}
