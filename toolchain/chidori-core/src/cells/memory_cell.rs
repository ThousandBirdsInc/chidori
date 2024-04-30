use std::time::Duration;
use tonic::codegen::Body;
use crate::cells::{MemoryCell, SupportedMemoryProviders, TextRange};
use crate::execution::primitives::operation::{AsyncRPCCommunication, InputItemConfiguration, InputSignature, InputType, OperationFnOutput, OperationNode, OutputItemConfiguration, OutputSignature};
use futures_util::FutureExt;
use serde_json::json;
use crate::execution::primitives::serialized_value::{json_value_to_serialized_value, RkyvSerializedValue, serialized_value_to_json_value};
use crate::library::std::ai::memory::in_memory::InMemoryVectorDb;

/// Memory cells when first executed have no inputs. They initialize the connection to the memory store.
/// Once initialized they become communicated with over the functions that they provide to the workspace.
#[tracing::instrument]
pub fn memory_cell(cell: &MemoryCell, range: &TextRange) -> anyhow::Result<OperationNode> {
    match cell.provider {
        SupportedMemoryProviders::InMemory => {
            let mut input_signature = InputSignature::new();
            input_signature.globals.insert(
                cell.embedding_function.clone(),
                InputItemConfiguration {
                    ty: Some(InputType::String),
                    default: None,
                },
            );

            let triggerable_functions = vec![
                "insert",
                "search"
            ];

            let mut output_signature = OutputSignature::new();
            for key in &triggerable_functions {
                output_signature.functions.insert(
                    key.to_string(),
                    OutputItemConfiguration::Function {
                        input_signature: InputSignature::new(),
                        emit_event: vec![],
                        trigger_on: vec![],
                    },
                );
            }
            let cell = cell.clone();
            Ok(OperationNode::new(
                None,
                input_signature,
                output_signature,
                Box::new(move |s, x, _, async_rpccommunication| {
                    let embedding_function = cell.embedding_function.clone();
                    let s = s.clone();
                    async move {
                        let mut db = InMemoryVectorDb::new();
                        db.new_collection("default".to_string());
                        let mut async_rpccommunication: AsyncRPCCommunication = async_rpccommunication.unwrap();
                        async_rpccommunication.callable_interface_sender.send(vec!["insert".to_string(), "search".to_string()]).unwrap();
                        let s = s.clone();
                        tokio::spawn(async move {
                            loop {
                                if let Ok((key, value, sender)) = async_rpccommunication.receiver.try_recv() {
                                    match key.as_str() {
                                        "insert" => {
                                            let (embedded_value, _) = s.dispatch(&embedding_function, value.clone()).await?;
                                            let embedding = rkyv_to_vec_float(embedded_value);
                                            let contents = serialized_value_to_json_value(&value);
                                            let row = vec![(&embedding, contents)];
                                            db.insert("default".to_string(), &row);
                                            sender.send(RkyvSerializedValue::String(String::from("Success"))).unwrap();
                                        }
                                        "search" => {
                                            let (embedded_value, _) = s.dispatch(&embedding_function, value.clone()).await?;
                                            let embedding = rkyv_to_vec_float(embedded_value);
                                            let results = db.search("default".to_string(), embedding, 5);
                                            let mut output = vec![];
                                            for (_, value) in results.iter() {
                                                let value = json_value_to_serialized_value(&value);
                                                output.push(value);
                                            }
                                            sender.send(RkyvSerializedValue::Array(output)).unwrap();
                                        }
                                        _ => {}
                                    }
                                } else {
                                    tokio::time::sleep(Duration::from_millis(10)).await; // Sleep for 10 milliseconds
                                }
                            }
                            anyhow::Ok(())
                        }).await.unwrap();
                        Ok(OperationFnOutput::with_value(RkyvSerializedValue::Null))
                    }.boxed()
                }),
            ))
        },
        // SupportedMemoryProviders::Qdrant => {
        //
        // }
    }
}

fn rkyv_to_vec_float(embedded_value: RkyvSerializedValue) -> Vec<f32> {
    let mut embedding = vec![];
    if let RkyvSerializedValue::Array(arr) = embedded_value {
        arr.iter().for_each(|a| if let RkyvSerializedValue::Float(f) = a {
            embedding.push(f.clone());
        });
    }
    embedding
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;
    use futures_util::FutureExt;
    use indoc::indoc;
    use crate::cells::{CellTypes, CodeCell, SupportedLanguage, TextRange};
    use crate::execution::execution::ExecutionState;
    use crate::execution::primitives::operation::{AsyncRPCCommunication, OperationNode, Signature};
    use crate::execution::primitives::serialized_value::RkyvSerializedValue;

    #[tokio::test]
    async fn test_memory_cell() -> anyhow::Result<()> {
        let (async_rpc_communication, rpc_sender, callable_interface_receiver) = AsyncRPCCommunication::new();
        let mut state = ExecutionState::new();
        let (mut state, _) = state.update_op(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        def fake_embedding():
                            return [0.1, 0.2, 0.3]
                        "#}),
            function_invocation: None,
        }, TextRange::default()), Some(0))?;
        let op = memory_cell(&MemoryCell {
            name: None,
            provider: SupportedMemoryProviders::InMemory,
            embedding_function: "fake_embedding".to_string(),
        }, &TextRange::default())?;
        let ex = op.execute(&state, RkyvSerializedValue::Null, None, Some(async_rpc_communication));
        let join_handle = tokio::spawn(async move {
            ex.await;
        });
        let callable_interface = callable_interface_receiver.await;
        assert_eq!(callable_interface, Ok(vec!["insert".to_string(), "search".to_string()]));
        let (s, r) = tokio::sync::oneshot::channel();
        rpc_sender.send(("insert".to_string(), RkyvSerializedValue::String("Demonstration".to_string()), s)).unwrap();
        let result = r.await.unwrap();
        assert_eq!(result, RkyvSerializedValue::String("Success".to_string()));
        let (s, r) = tokio::sync::oneshot::channel();
        rpc_sender.send(("search".to_string(), RkyvSerializedValue::String("Demo".to_string()), s)).unwrap();
        let result = r.await.unwrap();
        assert_eq!(result, RkyvSerializedValue::Array(vec![RkyvSerializedValue::String("Demonstration".to_string())]));
        Ok(())
    }
}
