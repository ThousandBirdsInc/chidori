use std::collections::HashMap;
use std::env;
use std::net::ToSocketAddrs;
use anyhow;
use futures::stream::{self, StreamExt, TryStreamExt};
use reqwest;
use serde::{Deserialize, Serialize};
use serde_json::json;
use _chidori::{create_change_value, NodeWillExecuteOnBranch};
use _chidori::register_node_handle;
use _chidori::translations::rust::{Chidori, CustomNodeCreateOpts, DenoCodeNodeCreateOpts, GraphBuilder, Handler, PromptNodeCreateOpts, serialized_value_to_string};


/// Maintain a list summarizing recent AI launches across the week
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut c = Chidori::new(String::from("0"), String::from("http://localhost:9800"));
    c.start_server(Some(":memory:".to_string())).await?;

    let mut g = GraphBuilder::new();

    let mut h_interpret = g.prompt_node(PromptNodeCreateOpts {
        name: "InterpretTheGroup".to_string(),
        template: "Based on the following list of HackerNews threads, filter this list to only launches of new AI projects: {{FetchTopHN.output}}".to_string(),
        ..PromptNodeCreateOpts::default()
    })?;
    h_interpret.run_when(&mut g, &h)?;

    let mut h_format_and_rank = g.prompt_node(PromptNodeCreateOpts {
        name: "FormatAndRank".to_string(),
        template: "Format this list of new AI projects in markdown, ranking the most interesting projects from most interesting to least. {{InterpretTheGroup.promptResult}}".to_string(),
        ..PromptNodeCreateOpts::default()
    })?;
    h_format_and_rank.run_when(&mut g, &h_interpret)?;

    // Commit the graph
    g.commit(&c, 0).await?;

    // Start graph execution from the root
    c.play(0, 0).await?;

    // Register the handler for our custom node
    register_node_handle!(c, "FetchTopHN", handle_fetch_hn);

    Ok(())
}
