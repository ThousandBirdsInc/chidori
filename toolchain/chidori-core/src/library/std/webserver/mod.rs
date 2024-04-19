use std::collections::{HashMap, HashSet};
use axum::{
    http::StatusCode,
    Json,
    Router, routing::{get, post, },
};
use axum::response::{Html, IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;
use tonic::IntoRequest;
use crate::cells::{CellTypes, CodeCell, WebserviceCell, WebserviceCellEndpoint};
use crate::execution::execution::ExecutionState;
use crate::execution::primitives::serialized_value::{json_value_to_serialized_value, RkyvObjectBuilder, RkyvSerializedValue, serialized_value_to_json_value};

pub fn parse_configuration_string(configuration: &str) -> Vec<WebserviceCellEndpoint> {
    configuration
        .lines()
        .filter_map(|line| {
            let parts_top_level: Vec<&str> = line.split('[').collect();
            let parts: Vec<&str> = parts_top_level[0].split_whitespace().collect();
            if parts_top_level.len() == 1 {
                return Some(WebserviceCellEndpoint {
                    method: parts[0].to_string(),
                    route: parts[1].to_string(),
                    depended_function_identity: parts[2].to_string(),
                    arg_mapping: vec![],
                });
            } else {
                let arg_mapping: Vec<(String, String)> = parts_top_level[1]
                    .trim_matches('[').trim_matches(']')
                    .split(',')
                    .enumerate()
                    .filter_map(|(idx, mapping)| {
                        // TODO: handle object notation within positional arguments
                        // mapping.trim_matches('{').trim_matches('}').split(',')
                        return Some((format!("{}",idx), mapping.trim().to_string()));
                        // let mut map_parts = mapping.trim().split(':');
                        // match (map_parts.next(), map_parts.next()) {
                        //     (Some(key), Some(value)) => Some((key.trim().to_string(), value.trim().to_string())),
                        //     _ => None,
                        // }
                    })
                    .collect();

                Some(WebserviceCellEndpoint {
                    method: parts[0].to_string(),
                    route: parts[1].to_string(),
                    depended_function_identity: parts[2].to_string(),
                    arg_mapping, // Add this to your WebserviceCellEndpoint struct
                })
            }
        })
        .collect()
}

#[tracing::instrument]
pub async fn run_webservice(
    configuration: &WebserviceCell,
    payload: &RkyvSerializedValue,
) -> (JoinHandle<()>, u16) {
    let endpoints = parse_configuration_string(&configuration.configuration);

    // build our application
    let mut app = Router::new();

    // Capture subscribed functions
    let subscribed_functions: HashSet<String> = endpoints.iter().map(|x| x.depended_function_identity.clone()).collect();
    let mut our_functions_map = &HashMap::new();
    if let RkyvSerializedValue::Object(ref payload_map) = payload {
        if let Some(RkyvSerializedValue::Object(functions_map)) = payload_map.get("functions") {
            our_functions_map = functions_map;
        }
    }

    // Create shims for functions that are referred to, we look at what functions are being provided
    // and create shims for matches between the function name provided and the identifiers referred to.
    for endpoint in endpoints.iter() {
        match endpoint.method.as_str() {
            "GET" => {
                let function_name = &endpoint.depended_function_identity;
                let value = our_functions_map.get(function_name).unwrap();
                if let RkyvSerializedValue::Cell(cell) = value.clone() {
                    if subscribed_functions
                        .contains(function_name)
                    {
                        let cell_clone = cell.clone();
                        let function_name = function_name.clone();
                        let arg_mapping = endpoint.arg_mapping.clone();
                        app = app.route(&endpoint.route, get(move || {
                            async move {
                                // modify code cell to indicate execution of the target function
                                // reconstruction of the cell
                                let mut op = match &cell_clone {
                                    CellTypes::Code(c) => {
                                        let mut c = c.clone();
                                        c.function_invocation =
                                            Some(function_name.clone());
                                        crate::cells::code_cell::code_cell(&c)
                                    }
                                    CellTypes::Prompt(c) => {
                                        crate::cells::llm_prompt_cell::llm_prompt_cell(&c)
                                    }
                                    CellTypes::Template(c) => {
                                        crate::cells::template_cell::template_cell(&c)
                                    }
                                    _ => {
                                        unreachable!("Unsupported cell type");
                                    }
                                }.unwrap();

                                let mut argument_payload = RkyvObjectBuilder::new();
                                // if &arg_mapping.len() > &0 {
                                //     for (key, value) in &arg_mapping {
                                //         argument_payload = argument_payload.insert_value(key, json_value_to_serialized_value(payload.get(value).unwrap()));
                                //     }
                                // }
                                let argument_payload = argument_payload.build();

                                dbg!(&argument_payload);
                                // invocation of the operation
                                let result = op.execute(&ExecutionState::new(), argument_payload, None, None).await.unwrap();
                                if let RkyvSerializedValue::String(s) = &result.output {
                                    (StatusCode::CREATED, Html(s.clone())).into_response()
                                } else {
                                    (StatusCode::CREATED, Json(serialized_value_to_json_value(&result.output))).into_response()
                                }
                            }
                        }));
                    }
                }
            }
            "POST" => {
                let function_name = &endpoint.depended_function_identity;
                let value = our_functions_map.get(function_name).unwrap();
                if let RkyvSerializedValue::Cell(cell) = value.clone() {
                    if subscribed_functions
                        .contains(function_name)
                    {
                        let cell_clone = cell.clone();
                        let function_name = function_name.clone();
                        let arg_mapping = endpoint.arg_mapping.clone();
                        app = app.route(&endpoint.route, post(move |Json(payload): Json<Value>| {
                            async move {
                                // modify code cell to indicate execution of the target function
                                // reconstruction of the cell
                                let mut op = match &cell {
                                    CellTypes::Code(c) => {
                                        let mut c = c.clone();
                                        c.function_invocation =
                                            Some(function_name.clone());
                                        crate::cells::code_cell::code_cell(&c)
                                    },
                                    CellTypes::Prompt(c) => {
                                        crate::cells::llm_prompt_cell::llm_prompt_cell(&c)
                                    },
                                    CellTypes::Template(c) => {
                                        crate::cells::template_cell::template_cell(&c)
                                    }
                                    _ => {
                                        unreachable!("Unsupported cell type");
                                    }
                                }.unwrap();

                                // invocation of the operation
                                let mut argument_payload = RkyvObjectBuilder::new();
                                if &arg_mapping.len() > &0 {
                                    for (key, value) in &arg_mapping {
                                        // TODO: handle the unwrap here
                                        argument_payload = argument_payload.insert_value(key, json_value_to_serialized_value(payload.get(value).unwrap()));
                                    }
                                }
                                // invocation of the operation
                                let result = op.execute(
                                    &ExecutionState::new(),
                                    RkyvObjectBuilder::new()
                                        .insert_object("args", argument_payload)
                                        .build(),
                                    None,
                                    None).await.unwrap();
                                (StatusCode::CREATED, Json(result.output))
                            }
                        }));
                    }
                }
            }
            _ => {
                panic!("Unsupported method");
            }
        }
    }

    let configuration_clone = configuration.clone();
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", configuration_clone.port)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_task = tokio::spawn(async move {
        dbg!("Running axum server", configuration_clone);
        let serve_result = axum::serve(listener, app).await;
        match serve_result {
            Ok(_) => eprintln!("Server stopped normally"),
            Err(e) => eprintln!("Server stopped with error: {}", e),
        }
    });
    (server_task, port)
}


// async fn create_user(
//     // this argument tells axum to parse the request body
//     // as JSON into a `CreateUser` type
//     Json(payload): Json<CreateUser>,
// ) -> (StatusCode, Json<User>) {
//     // insert your application logic here
//     let user = User {
//         id: 1337,
//         username: payload.username,
//     };
//
//     // this will be converted into a JSON response
//     // with a status code of `201 Created`
//     (StatusCode::CREATED, Json(user))
// }
//

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};
    use reqwest;
    use indoc::indoc;
    use tokio::runtime::Runtime;

    use crate::cells::{CellTypes, CodeCell, SupportedLanguage, WebserviceCell};

    #[tokio::test]
    async fn test_webservice() {
        let configuration = WebserviceCell {
            name: None,
            configuration: indoc! {r#"
                    POST / demo
                    "#}.to_string(),
            port: 0,
        };

        let payload = RkyvObjectBuilder::new()
            .insert_object("functions", RkyvObjectBuilder::new().insert_value(
                "demo",
                RkyvSerializedValue::Cell(CellTypes::Code(CodeCell {
                    name: None,
                    language: SupportedLanguage::PyO3,
                    source_code: String::from(indoc! {r#"
                        def demo():
                            return 100
                        "#}),
                    function_invocation: None,
                }))))
            .build();
        let (server_handle , port) = crate::library::std::webserver::run_webservice(&configuration, &payload).await;

        // Receive the server port from the server task.
        // let server_port = rx.await.expect("Failed to receive the server port");

        let client = reqwest::Client::new();
        let res = client.post(format!("http://127.0.0.1:{}", port))
            .header("Content-Type", "application/json")
            .json(&HashMap::<String, String>::new())
            .send()
            .await
            .expect("Failed to send request");

        assert_eq!(res.text().await.unwrap(), "100");
        // assert!(res.status().is_success(), "Request was not successful");
        server_handle.abort(); // or another clean shutdown mechanism
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_webservice_with_payload() {
        // let (tx, rx) = oneshot::channel();

        let configuration = WebserviceCell {
            name: None,
            configuration: indoc! {r#"
                    POST / add [a, b]
                    "#}.to_string(),
            port: 0,
        };

        let payload = RkyvObjectBuilder::new()
            .insert_object("functions", RkyvObjectBuilder::new().insert_value(
                "add",
                RkyvSerializedValue::Cell(CellTypes::Code(CodeCell {
                    name: None,
                    language: SupportedLanguage::PyO3,
                    source_code: String::from(indoc! {r#"
                        def add(a, b):
                            return a + b
                        "#}),
                    function_invocation: None,
                }))))
            .build();

        let (server_handle, port) = crate::library::std::webserver::run_webservice(&configuration, &payload).await;
        let client = reqwest::Client::new();
        let mut payload = HashMap::new();
        payload.insert("a", 123); // Replace 123 with your desired value for "a"
        payload.insert("b", 456); // Replace 456 with your desired value for "b"

        let res = client.post(format!("http://127.0.0.1:{}", port))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .expect("Failed to send request");

        assert_eq!(res.text().await.unwrap(), "579");
        // assert!(res.status().is_success(), "Request was not successful");

        server_handle.abort(); // or another clean shutdown mechanism
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_webservice_with_payload_returning_json_response() {
        // let (tx, rx) = oneshot::channel();

        let configuration = WebserviceCell {
            name: None,
            configuration: indoc! {r#"
                    POST / add [a, b]
                    "#}.to_string(),
            port: 0,
        };

        let payload = RkyvObjectBuilder::new()
            .insert_object("functions", RkyvObjectBuilder::new().insert_value(
                "add",
                RkyvSerializedValue::Cell(CellTypes::Code(CodeCell {
                    name: None,
                    language: SupportedLanguage::PyO3,
                    source_code: String::from(indoc! {r#"
                        def add(a, b):
                            return {"x": a + b}
                        "#}),
                    function_invocation: None,
                }))))
            .build();

        let (server_handle, port) = crate::library::std::webserver::run_webservice(&configuration, &payload).await;

        let client = reqwest::Client::new();
        let mut payload = HashMap::new();
        payload.insert("a", 123); // Replace 123 with your desired value for "a"
        payload.insert("b", 456); // Replace 456 with your desired value for "b"

        let res = client.post(format!("http://127.0.0.1:{}", port))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .expect("Failed to send request");

        assert_eq!(res.text().await.unwrap(), "{\"x\":579}");
        server_handle.abort(); // or another clean shutdown mechanism
        let _ = server_handle.await;
    }

}

