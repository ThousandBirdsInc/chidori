use std::time::Duration;
use tonic::codegen::Body;
use crate::cells::{MemoryCell, SupportedMemoryProviders};
use crate::execution::primitives::operation::{AsyncRPCCommunication, InputSignature, InputType, OperationNode, OutputItemConfiguation, OutputSignature};
use futures_util::FutureExt;
use serde_json::json;
use crate::execution::primitives::serialized_value::RkyvSerializedValue;
use crate::library::std::ai::memory::in_memory::InMemoryVectorDb;

/// Memory cells when first executed have no inputs. They initialize the connection to the memory store.
/// Once initialized they become communicated with over the functions that they provide to the workspace.
#[tracing::instrument]
pub fn memory_cell(cell: &MemoryCell) -> OperationNode {
    match cell.provider {
        SupportedMemoryProviders::InMemory => {
            let mut input_signature = InputSignature::new();
            // input_signature.globals.insert(
            //     key.clone(),
            //     InputItemConfiguation {
            //         ty: Some(InputType::String),
            //         default: None,
            //     },
            // );

            let triggerable_functions = vec![
                ("run", "run",)
            ];

            let mut output_signature = OutputSignature::new();

            for (key, value) in &triggerable_functions {
                output_signature.functions.insert(
                    key.to_string(),
                    OutputItemConfiguation {
                        ty: Some(InputType::Function),
                    },
                );
            }

            let cell = cell.clone();
            OperationNode::new(
                None,
                input_signature,
                output_signature,
                Box::new(move |_, x, _, async_rpccommunication| {
                    async move {
                        let mut db = InMemoryVectorDb::new();
                        db.new_collection("default".to_string());
                        let mut async_rpccommunication: AsyncRPCCommunication = async_rpccommunication.unwrap();
                        async_rpccommunication.callable_interface_sender.send(vec!["run".to_string()]).unwrap();
                        tokio::spawn(async move {
                            loop {
                                if let Ok((key, value, sender)) = async_rpccommunication.receiver.try_recv() {
                                    match key.as_str() {
                                        "run" => {
                                            let mut embedding = vec![0.1, 0.2, 0.3];
                                            let contents = json!({"name": "test"});
                                            let row = vec![(&embedding, contents)];
                                            db.insert("default".to_string(), &row);
                                            sender.send(RkyvSerializedValue::String(format!("{}", 1))).unwrap();
                                        }
                                        _ => {}
                                    }
                                } else {
                                    tokio::time::sleep(Duration::from_millis(10)).await; // Sleep for 10 milliseconds
                                }
                            }
                        }).await.unwrap();
                        RkyvSerializedValue::Null
                    }.boxed()
                }),
            )
        }
    }
}
#[cfg(test)]
mod test {
    use crate::execution::execution::ExecutionState;

    #[tokio::test]
    async fn test_memory_cell() {
    }
}
