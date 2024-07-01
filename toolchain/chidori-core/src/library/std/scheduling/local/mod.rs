use std::collections::{HashMap, HashSet};
use tokio_cron_scheduler::{Job, JobScheduler, JobSchedulerError};
use uuid::Uuid;
use crate::cells::{CellTypes, CodeCell, ScheduleCell};
use crate::execution::execution::ExecutionState;
use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};

struct ScheduledJob {
    schedule: String,
    function_identity: String,
}

pub fn parse_configuration_string(configuration: &str) -> Vec<ScheduledJob> {
    configuration
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            return Some(ScheduledJob {
                schedule: parts.get(0)?.to_string(),
                function_identity: parts.get(1)?.to_string(),
            });
        })
        .collect()
}

pub async fn run_cron(
    configuration: &ScheduleCell,
    payload: &RkyvSerializedValue,
) -> Result<(), JobSchedulerError> {
    let jobs = parse_configuration_string(&configuration.configuration);
    let mut sched = JobScheduler::new().await?;

    let subscribed_functions: HashSet<String> = jobs.iter().map(|x| x.function_identity.clone()).collect();
    let mut our_functions_map = &HashMap::new();
    if let RkyvSerializedValue::Object(ref payload_map) = payload {
        if let Some(RkyvSerializedValue::Object(functions_map)) = payload_map.get("functions") {
            our_functions_map = functions_map;
        }
    }
    for job in jobs {
        let function_name = &job.function_identity;
        let value = our_functions_map.get(function_name).unwrap();
        if let RkyvSerializedValue::Cell(cell) = value.clone() {
            if let CellTypes::Code(CodeCell { function_invocation, .. }, r) = &cell
            {
                if subscribed_functions
                    .contains(function_name)
                {
                    let cell_clone = cell.clone();
                    let function_name = function_name.clone();
                    sched.add(
                        Job::new(job.schedule.as_str(), move |_uuid, _l| {
                            // modify code cell to indicate execution of the target function
                            // reconstruction of the cell
                            let mut op = match &cell_clone {
                                CellTypes::Code(c, r) => {
                                    let mut c = c.clone();
                                    c.function_invocation =
                                        Some(function_name.clone());
                                    crate::cells::code_cell::code_cell(Uuid::nil(), &c, r)
                                }
                                CellTypes::Prompt(c, r) => {
                                    crate::cells::llm_prompt_cell::llm_prompt_cell(Uuid::nil(), &c, r)
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
                            let result = op.execute(&ExecutionState::new_with_random_id(), argument_payload, None, None);
                        })?
                    ).await?;
                }
            }
        }
    }

    // Add code to be run during/after shutdown
    sched.set_shutdown_handler(Box::new(|| {
        Box::pin(async move {
            println!("Shut down done");
        })
    }));

    // Start the scheduler
    sched.start().await?;
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_parse_configuration_string() {
    }

    #[test]
    fn test_invocation_of_function_on_schedule() {
    }
}
