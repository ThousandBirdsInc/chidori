// This language exists to be able to author lazily evaluated functions.
// It's possible to do this in Rust, but it's not ergonomic.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod javascript;
pub mod python;

// TODO: implement a function that infers the language from the source code successfully parsing

// TODO: it would be helpful if reports noted if a value is a global, an arg, or a kwarg
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportItem {
    // pub context_path: Vec<ContextPath>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportTriggerableFunctions {
    // pub context_path: Vec<ContextPath>,
    // TODO: these need their own set of depended values
    // TODO: we need to extract signatures for triggerable functions
    pub emit_event: Vec<String>,
    pub trigger_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub cell_exposed_values: HashMap<String, ReportItem>,
    pub cell_depended_values: HashMap<String, ReportItem>,
    pub triggerable_functions: HashMap<String, ReportTriggerableFunctions>,
    pub declared_functions: HashMap<String, ReportTriggerableFunctions>,
}