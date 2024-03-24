// use chidori_static_analysis::language::python::parse::{extract_dependencies_python, ContextPath};
//
// use rustpython::vm::{
//     pymodule, PyPayload, PyResult, VirtualMachine,
// };
// use rustpython_vm as vm;
// use rustpython_vm::builtins::{PyBool, PyDict, PyInt, PyList, PyStr};
//
//
//
// use crate::execution::primitives::serialized_value::RkyvSerializedValue;
// use rustpython_vm::PyObjectRef;
// use std::collections::HashMap;
//
// fn pyobject_to_rkyv(vm: &VirtualMachine, obj: PyObjectRef) -> Result<RkyvSerializedValue, String> {
//     match obj.downcast_ref::<PyDict>() {
//         Some(py_dict) => {
//             let mut map = HashMap::new();
//             for (key, value) in py_dict {
//                 map.insert(
//                     pyobject_to_rkyv(vm, key)?.as_string().unwrap().to_string(),
//                     pyobject_to_rkyv(vm, value)?,
//                 );
//             }
//             Ok(RkyvSerializedValue::Object(map))
//         }
//         None => match obj.downcast_ref::<PyList>() {
//             Some(_py_list) => {
//                 if let Some(list) = obj.payload_if_exact::<PyList>(vm) {
//                     let vec: Result<Vec<_>, _> = list
//                         .borrow_vec()
//                         .iter()
//                         .map(|x| pyobject_to_rkyv(vm, x.clone()))
//                         .collect();
//                     Ok(RkyvSerializedValue::Array(vec?))
//                 } else {
//                     Err("Unsupported PyObject type".to_string())
//                 }
//             }
//             None => match obj.downcast_ref::<PyStr>() {
//                 Some(py_str) => Ok(RkyvSerializedValue::String(py_str.as_str().to_string())),
//                 None => match obj.downcast_ref::<PyInt>() {
//                     Some(py_int) => Ok(RkyvSerializedValue::Number(
//                         py_int.try_to_primitive(vm).unwrap(),
//                     )),
//                     None => match obj.downcast_ref::<PyBool>() {
//                         Some(_py_bool) => {
//                             Ok(RkyvSerializedValue::Boolean(obj.try_to_bool(vm).unwrap()))
//                         }
//                         None => {
//                             // Handle other types or return an error
//                             Err("Unsupported PyObject type".to_string())
//                         }
//                     },
//                 },
//             },
//         },
//     }
// }
//
// // Helper function for String conversion if needed
// impl RkyvSerializedValue {
//     fn as_string(&self) -> Option<&String> {
//         if let RkyvSerializedValue::String(s) = self {
//             Some(s)
//         } else {
//             None
//         }
//     }
// }
//
// // TODO: validate suspension and resumption of execution based on a method that we provide
// // TODO: we want to capture the state of the interpreter and resume it later when we invoke the target function
// // TODO: need to be able to capture output results
//
// pub fn source_code_run_python(source_code: String) -> anyhow::Result<RkyvSerializedValue> {
//     let dependencies = extract_dependencies_python(&source_code);
//
//     let interp = rustpython::InterpreterConfig::new()
//         .init_stdlib()
//         .init_hook(Box::new(|vm| {
//             vm.add_native_module("chidori".to_owned(), Box::new(rust_py_module::make_module));
//         }))
//         .interpreter();
//
//     interp.enter(|vm| {
//         let scope = vm.new_scope_with_builtins();
//         let code_obj = vm
//             .compile(
//                 &source_code,
//                 vm::compiler::Mode::Exec,
//                 "<embedded>".to_owned(),
//             )
//             .map_err(|err| vm.new_syntax_error(&err, Some(&source_code)))
//             .unwrap();
//         // TODO: should serialize these exceptions to show them to the user
//
//         // scope.globals.set_item()
//
//         let module = vm.run_code_obj(code_obj, scope.clone());
//
//         match module {
//             Err(exc) => {
//                 vm.print_exception(exc);
//             }
//             Ok(_module) => {
//                 let mut result_map = HashMap::new();
//                 for dependency in &dependencies {
//                     if let Some(ContextPath::VariableAssignment(name)) = dependency.first() {
//                         let p = vm.new_pyobj(name.clone());
//                         let s = p.downcast_ref::<PyStr>().unwrap();
//                         let global = scope.globals.get_item(&*s, vm);
//                         if let Ok(global) = global {
//                             let parsed = pyobject_to_rkyv(vm, global).unwrap();
//                             result_map.insert(name.clone(), parsed);
//                         }
//                     }
//                 }
//                 return Ok(RkyvSerializedValue::Object(result_map));
//                 // let init_fn = scope.globals.get_item("fun", vm).unwrap();
//                 // init_fn.call((), vm).unwrap();
//             }
//         }
//
//         Ok(RkyvSerializedValue::Object(HashMap::new()))
//     })
// }
//
// #[pymodule]
// mod rust_py_module {
//     use super::*;
//
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
//
// #[cfg(test)]
// mod tests {
//     use super::*;
//
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
//
//     #[test]
//     fn test_py_source_without_entrypoint() {
//         let source_code = String::from(
//             r#"
// y = 42
// x = 12 + y
// li = [x, y]
//         "#,
//         );
//         let result = source_code_run_python(source_code);
//         assert_eq!(
//             result.unwrap(),
//             RkyvSerializedValue::Object(HashMap::from_iter(vec![
//                 ("y".to_string(), RkyvSerializedValue::Number(42),),
//                 ("x".to_string(), RkyvSerializedValue::Number(54),),
//                 (
//                     "li".to_string(),
//                     RkyvSerializedValue::Array(vec![
//                         RkyvSerializedValue::Number(54),
//                         RkyvSerializedValue::Number(42)
//                     ]),
//                 )
//             ]))
//         );
//     }
// }
