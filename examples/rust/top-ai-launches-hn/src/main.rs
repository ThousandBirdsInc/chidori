use std::collections::HashMap;
use std::env;
use std::path::Path;
use anyhow::anyhow;
use futures::stream::{self, StreamExt, TryStreamExt};
use reqwest;
use serde::{Deserialize, Serialize};
use _chidori::NodeWillExecuteOnBranch;
use _chidori::register_node_handle;
use _chidori::translations::rust::{Chidori, CustomNodeCreateOpts, GraphBuilder, Handler, PromptNodeCreateOpts};

#[derive(Debug, Deserialize, Serialize)]
struct Story {
    title: String,
    url: Option<String>,
    score: Option<f32>,
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
                let story: Story = client.get(&resource).send().await?.json().await?;
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

// check if we're running inside a docker container.
// used to print meaningful error messages when running inside a container.
fn is_running_inside_docker() -> bool {
    Path::new("/.dockerenv").exists()
}

/// Maintain a list summarizing recent AI launches across the week
#[tokio::main]
async fn main() -> anyhow::Result<()> {
      // Check for the presence of the environment variable
      // return gracefully with a meaningful error message if it's not set.
      if env::var("OPEN_AI_KEY").is_err() {
        if is_running_inside_docker() {
            eprintln!("Error: OPEN_AI_KEY is not set!");
            eprintln!("If you're running this from container, please set the environment variable using:");
            eprintln!("\ndocker run -e OPEN_AI_KEY=your_key_here ...\n");
        } else {
            eprintln!("Error: OPEN_AI_KEY is not set!");
            eprintln!("Please set the environment variable using:");
            eprintln!("\nexport OPEN_AI_KEY=your_key_here\n");
        }
        return Err(anyhow!("Environment variable not set"));
    }

    let mut c = Chidori::new(String::from("0"), String::from("http://localhost:9800"));
    c.start_server(Some(":memory:".to_string())).await?;

    let mut g = GraphBuilder::new();

    let h = g.custom_node(CustomNodeCreateOpts {
        name: "FetchTopHN".to_string(),
        node_type_name: "FetchTopHN".to_string(),
        output: Some("{ output: String }".to_string()),
        ..CustomNodeCreateOpts::default()
    })?;

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

    // Run the node execution loop
    if let Err(x) = c.run_custom_node_loop().await {
        eprintln!("Custom Node Loop Failed On - {:?}", x);
    };
    Ok(())
}
