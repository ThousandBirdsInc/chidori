use std::collections::HashMap;
use reqwest;
use serde::{Deserialize, Serialize};
use futures::stream::{self, StreamExt, TryStreamExt};

use chidori::translations::rust::{Chidori, CustomNodeCreateOpts, GraphBuilder, Handler, PromptNodeCreateOpts};
use anyhow;
use chidori::{create_change_value, NodeWillExecuteOnBranch};
extern crate chidori;
use chidori::register_node_handle;


#[derive(Debug, Deserialize, Serialize)]
struct Story {
    title: String,
    url: Option<String>,
    score: Option<f32>,
    kids: Option<Vec<u32>>,
}

const HN_URL_TOP_STORIES: &'static str = "https://hacker-news.firebaseio.com/v0/topstories.json?print=pretty";

async fn fetch_hn() -> anyhow::Result<Vec<Story>> {
    let client = reqwest::Client::new();
    // Fetch the top 60 story ids
    let story_ids: Vec<u32> = client.get(HN_URL_TOP_STORIES).send().await?.json().await?;

    // Fetch details for each story
    let stories: anyhow::Result<Vec<Story>> = stream::iter(story_ids.into_iter().take(30))
        .map(|id| {
            let client = &client;
            async move {
                let resource = format!("https://hacker-news.firebaseio.com/v0/item/{}.json?print=pretty", id);
                let mut story: Story = client.get(&resource).send().await?.json().await?;
                story.kids.as_mut().unwrap_or(&mut vec![]).resize(5, 0);
                Ok(story)
            }
        })
        .buffer_unordered(10)  // Fetch up to 10 stories concurrently
        .try_collect()
        .await;
    stories
}

async fn handle_fetch_hn(_node_will_exec: NodeWillExecuteOnBranch) -> anyhow::Result<serde_json::Value> {
    let stories = fetch_hn().await.unwrap();
    let mut result = HashMap::new();
    result.insert("output", format!("{:?}", stories));
    Ok(serde_json::to_value(result).unwrap())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut c = Chidori::new(String::from("0"), String::from("http://localhost:9800"));
    c.start_server(Some(":memory:".to_string())).await?;

    let mut g = GraphBuilder::new();

    let h = g.custom_node(CustomNodeCreateOpts {
        name: "FetchTopHN".to_string(),
        node_type_name: "FetchTopHN".to_string(),
        output: Some("type O { output: String }".to_string()),
        ..CustomNodeCreateOpts::default()
    })?;

    // Instead lets capture each item as a separate unit of work
    // Lets send out a prompt for each item in this list
    // Then we're going to summarize those collectively
    // And we're going to send out an email with the summary

    let mut h_interpret = g.prompt_node(PromptNodeCreateOpts {
        name: "InterpretTheGroup".to_string(),
        template: "Based on the following list of HackerNews threads, filter this list to only launches of new AI projects: {{FetchTopHN.output}}".to_string(),
        ..PromptNodeCreateOpts::default()
    })?;
    h_interpret.run_when(&mut g, &h)?;

    // Maintain a list summarizing recent AI launches across the week

    // TODO: choose to read comments about the launch


    // Commit the graph
    g.commit(&c, 0).await?;

    // Start graph execution from the root
    c.play(0, 0).await?;

    // Register the handler for our custom node
    register_node_handle!(c, "FetchTopHN", handle_fetch_hn);

    // Run the node execution loop
    if let Err(x) = c.run_custom_node_loop().await {
        eprintln!("Custom Node Loop Failed On - {:?}", x);
    };
    Ok(())
}
