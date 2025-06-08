/// Interacting with ipython formatted files
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notebook {
    pub cells: Vec<Cell>,
    pub metadata: HashMap<String, Value>,
    pub nbformat: usize,
    pub nbformat_minor: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "cell_type")]
pub enum Cell {
    #[serde(rename = "markdown")]
    Markdown(MarkdownCell),
    #[serde(rename = "code")]
    Code(CodeCell),
    #[serde(rename = "raw")]
    Raw(RawCell),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "output_type")]
#[serde(rename_all = "snake_case")]
pub enum Output {
    ExecuteResult(ExecuteResultOutput),
    Stream(StreamOutput),
    DisplayData(DisplayDataOutput),
    Error(ErrorOutput),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCell {
    pub metadata: HashMap<String, Value>,
    pub source: Vec<String>,
    pub id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeCell {
    pub metadata: HashMap<String, Value>,
    pub source: Vec<String>,
    pub id: Option<String>,
    // cell can be not executed, and it will then be null.
    pub execution_count: Option<usize>,
    pub outputs: Vec<Output>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamOutput {
    pub data: Option<HashMap<String, Value>>,
    pub metadata: Option<HashMap<String, Value>>,
    pub name: Option<String>,
    pub text: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteResultOutput {
    pub data: Option<HashMap<String, Value>>,
    pub metadata: Option<HashMap<String, Value>>,
    pub name: Option<String>,
    pub execution_count: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorOutput {
    pub ename: String, //pub data: Option<HashMap<String, Value>>,
    pub evalue: String,
    pub traceback: Vec<String>,
    //pub metadata: Option<HashMap<String, Value>>,
    //pub name: Option<String>,
    //pub execution_count: usize
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayDataOutput {
    pub data: Option<HashMap<String, Value>>,
    pub metadata: Option<HashMap<String, Value>>,
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkdownCell {
    pub metadata: HashMap<String, Value>,
    pub id: Option<String>,
    pub attachments: Option<HashMap<String, Value>>,
    pub source: Vec<String>,
}

#[cfg(test)]
mod tests {

    use super::Notebook;

    // #[test]
    // fn it_works() {
    //     let data = include_str!("../../test4.ipynb");
    //     let notebook: Notebook = serde_json::from_str(data).unwrap();
    //     assert_eq!(notebook.nbformat, 4);
    // }
}
