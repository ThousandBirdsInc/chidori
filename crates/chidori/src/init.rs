//! `chidori init` scaffolding: starter agent templates.
//!
//! Templates are embedded into the binary so `chidori init` works from an
//! installed binary with no dependency on the source tree. The worker agent and
//! its sample tool are the canonical files under `examples/`, embedded here with
//! `include_str!` so the scaffold and the runnable example never drift.

use std::path::Path;

use anyhow::{bail, Context, Result};

/// The conversational chat agent — shared by `chidori init --template chat`
/// (written to `agent.ts`) and the fileless `chidori chat` (written to a temp
/// file). Driven mode (a fixed `messages` list) is how `chidori chat` feeds each
/// turn; with no messages it reads the terminal interactively.
pub const CHAT_AGENT_SRC: &str = r#"import type { Chidori } from "chidori:agent";

export async function agent(
  input: { messages?: string[]; system?: string; model?: string; tools?: string[] },
  chidori: Chidori,
) {
  const chat = chidori.conversation({
    system: input.system ?? "You are a helpful, concise assistant.",
    model: input.model || undefined,
    tools: input.tools && input.tools.length ? input.tools : undefined,
    // Opt-in window management: a no-op until the running tail exceeds budget.
    compact: { budgetTokens: 8000 },
  });

  // Driven mode: a fixed list of user turns. This is how `chidori chat` feeds
  // each message, replaying the prior turns for free.
  const messages = input.messages ?? [];
  if (messages.length > 0) {
    for (const message of messages) await chat.say(message);
    return { transcript: chat.history() };
  }

  // Interactive mode: read each turn from the terminal. Type "exit" to end.
  const transcript = await chat.loop({ prompt: "you>" });
  return { transcript };
}
"#;

const CHAT_README: &str = r#"# Chidori chat agent

A conversational assistant built with `chidori.conversation()`.

## Run it

Interactive chat (streams each reply token-by-token; type `exit` to quit):

    chidori chat agent.ts

Or call it with a fixed list of messages:

    chidori run agent.ts --input '{"messages": ["Hi, who are you?"]}'

Every turn is a durable host call, so replaying the whole conversation costs
zero tokens. Set a provider key first (e.g. `ANTHROPIC_API_KEY` or
`OPENAI_API_KEY`).
"#;

const WORKER_README: &str = r#"# Chidori worker agent

An autonomous agent that loops — think, call a tool, observe the result, repeat —
until it finishes. Tools live in `tools/`; a sample `reverse` tool is included.

## Run it

    chidori run agent.ts \
      --input task="Reverse the word 'chidori' and tell me the result." \
      --tools tools

Add your own tools under `tools/` and list their names in the agent's
`.tools([...])` call. Set a provider key first (e.g. `ANTHROPIC_API_KEY` or
`OPENAI_API_KEY`).
"#;

/// The docs-chat agent: an offline-friendly RAG-lite assistant that answers
/// from the Markdown bundled under `docs/`. It demonstrates `workspace.read`
/// (scoped to this project — the agent can only see files placed here) plus the
/// conversation + replay model. The only thing sent to the model is the user's
/// question and these scaffolded docs; nothing else on the machine is touched.
const DOCS_AGENT_SRC: &str = r#"import type { Chidori } from "chidori:agent";

/**
 * Chat with the Chidori docs.
 *
 * The agent loads every Markdown file under `docs/` and answers questions from
 * that text. `chidori.workspace` is scoped to this project directory, so the
 * agent can only ever read files you put here — and the only data sent to the
 * model is your question plus these docs.
 */
export async function agent(
  input: { messages?: string[]; model?: string },
  chidori: Chidori,
) {
  // Load the local docs corpus. workspace.list/read are recorded host calls,
  // so re-running this agent replays them for free (no disk re-read on replay).
  const entries = await chidori.workspace.list();
  const docFiles = entries
    .map((e) => e.path)
    .filter((p) => p.startsWith("docs/") && p.endsWith(".md"))
    .sort();
  const corpus = (
    await Promise.all(
      docFiles.map(async (p) => `## ${p}\n\n${await chidori.workspace.read(p)}`),
    )
  ).join("\n\n");

  const chat = chidori.conversation({
    system:
      "You are the Chidori documentation assistant. Answer the user's question " +
      "using ONLY the documentation below. Quote the exact command or API when " +
      "relevant. If the answer is not in the docs, say so plainly rather than " +
      "guessing.\n\n=== CHIDORI DOCS ===\n\n" + corpus,
    model: input.model || undefined,
    // Fold older turns into a summary once the tail exceeds budget.
    compact: { budgetTokens: 8000 },
  });

  // Driven mode: `chidori chat agent.ts` feeds one user turn at a time here and
  // replays the earlier turns for free.
  const messages = input.messages ?? [];
  if (messages.length > 0) {
    for (const message of messages) await chat.say(message);
    return { transcript: chat.history() };
  }

  // Interactive mode: ask the docs anything. Type "exit" to quit.
  const transcript = await chat.loop({ prompt: "ask the docs>" });
  return { transcript };
}
"#;

const DOCS_README: &str = r#"# Chat with the Chidori docs

An assistant that answers questions from the Markdown under `docs/`. It's a tiny
demonstration of the things that make Chidori different: a plain TypeScript
agent, a sandboxed local workspace, and durable replay.

## Run it

Set a provider key, then chat:

    export ANTHROPIC_API_KEY=sk-ant-...   # or OPENAI_API_KEY=...
    chidori chat agent.ts

Try: "What is a host call?", "How do I write a tool?", "How does replay work?"

Or ask a single question non-interactively:

    chidori run agent.ts --input '{"messages": ["How do I run an agent?"]}'

## What it can (and can't) see

- The agent reads **only** the files under `docs/` in this folder. `chidori`
  scopes the workspace to this project directory — it cannot read elsewhere on
  your machine.
- The only data sent to the model is **your question plus these docs**. Your own
  files are never read or transmitted.
- Every turn is a recorded host call, so replaying the whole conversation costs
  zero tokens. Add your own `.md` files under `docs/` to chat with them too.
"#;

/// The bundled knowledge base the docs-chat agent answers from. A concise,
/// self-contained Chidori reference so the scaffolded agent works offline of the
/// source tree (an installed binary has no repo checkout).
const DOCS_CORPUS: &str = r#"# Chidori

Chidori is an agent framework where every run is durable, replayable, and
resumable by default. You write agents as plain async TypeScript. Every side
effect — every LLM call, tool call, and HTTP request — flows through the runtime
as a recorded **host call**. Because the inputs and outputs of those host calls
are journaled, any run can be checkpointed to disk, replayed for byte-identical
output with zero LLM calls, and resumed from any pause — even in a new process
after a crash.

It ships as one Rust binary with an embedded pure-Rust JavaScript engine (no
Node, no native bindings) plus TypeScript and Python SDKs.

## Installing

Chidori is one self-contained binary. Grab the prebuilt one (no Rust toolchain
needed) on macOS or Linux:

    curl -fsSL https://raw.githubusercontent.com/ThousandBirdsInc/chidori/main/scripts/install.sh | sh

Or build it from source with stable Rust (1.95 or newer):

    cargo install chidori

Either way the `chidori` binary lands on your PATH. Check it with
`chidori --version`.

## Writing an agent

An agent is a TypeScript file that exports an `agent` function taking the run
input and the `chidori` host object:

    import type { Chidori } from "chidori:agent";

    export async function agent(input: { document: string }, chidori: Chidori) {
      const summary = await chidori.prompt("Summarize:\n" + input.document);
      return { summary };
    }

That is a complete, durable agent. The prompt is recorded; replay returns it for
free.

## What a host call is

A host call is any interaction an agent has with the outside world, routed
through the runtime so it can be recorded and replayed: an LLM prompt, a tool
call, an HTTP request, a workspace read/write, a human input pause. On a live
run the call really executes and its result is journaled. On replay the runtime
returns the journaled result instead of executing again, so the agent follows
the exact same path with no external calls and no cost.

## The chidori host object

- `chidori.prompt(text, options?)` — call the configured LLM.
- `chidori.context(seed?)` — immutable context builder: `.system()`, `.doc()`,
  `.user()`, `.assistant()`, `.tools()`, `.toolResult()`, then `.prompt()` or
  `.respond()`.
- `chidori.conversation(options?)` — stateful multi-turn chat: `.say(message)`,
  `.respond()`, `.loop()`, `.history()`.
- `chidori.tool(name, args?)` — call a discovered local tool (policy-gated).
- `chidori.workspace` — the sandboxed project filesystem: `.list()`, `.read()`,
  `.write()`, `.delete()`, `.manifest()`. Scoped to the project directory.
- `chidori.fetch(url)` — HTTP request (policy-gated).
- `chidori.input(message, options?)` — pause and wait for human input.
- `chidori.callAgent(path, input?)` — run another agent file as a sub-run.
- `chidori.branch(variants, options?)` — fork into parallel sub-runs.
- `chidori.step(name, fn)` — durable checkpoint for an expensive computation.
- `chidori.retry(fn, options?)` — retry with backoff.
- `chidori.parallel(tasks, options?)` — run tasks concurrently.
- `chidori.memory(action, key?, value?)` — persistent key/value store.
- `chidori.log(message, fields?)` — structured log into the call trace.

## Writing a tool

A tool is a TypeScript file in a directory you pass with `--tools`. It exports a
`tool` definition (name, description, JSON-schema parameters) and a `run`
function:

    import type { ToolDefinition } from "chidori:agent";

    export const tool: ToolDefinition = {
      name: "reverse",
      description: "Reverse a string.",
      parameters: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
    };

    export async function run(args: { text: string }) {
      return { reversed: [...args.text].reverse().join("") };
    }

Tools are auto-discovered by name from the `--tools` directory. List the names
you want available in the agent's `.tools([...])` call, then invoke one with
`chidori.tool("reverse", { text })`.

## Running agents (CLI)

- `chidori run <file.ts> [--input key=value] [--tools <dir>] [--trace] [--stream]`
  — execute an agent. `--input` accepts `key=value` pairs or a JSON object;
  `@file` loads a value from a file.
- `chidori chat [agent.ts]` — interactive REPL. With no file it chats with the
  model directly; with a file it drives the agent's `messages` input per turn.
- `chidori demo` — interactive menu of example agents (several need no API key).
- `chidori init [dir] [--template <name>]` — scaffold a starter project.
- `chidori check <file.ts>` — type/parse-check an agent without running it.
- `chidori tools [--dir <dir>]` — list discovered tools.
- `chidori serve <file.ts> [--port N]` — run an agent as an HTTP server.
- `chidori resume <file.ts> <run_id>` — replay/resume a recorded run.
- `chidori trace <run_id>` — pretty-print a run's host-call log.
- `chidori stats` — aggregate cost and token usage across recorded runs.

## How replay works

Every run is recorded under `.chidori/runs/<run_id>/`: the inputs, the ordered
host calls and their results, and a checkpoint. To replay or resume, pass that
run id to `chidori resume <file.ts> <run_id>`. The runtime walks the agent
again, but each host call returns its journaled result instead of executing — so
you get byte-identical output with no LLM calls. If the run was paused (for
example at a `chidori.input(...)`), resume continues past the pause. This is the
foundation for tests, debugging, and human-in-the-loop workflows.

## Configuring a model provider

Set one provider environment variable before runs that call the model:

- `ANTHROPIC_API_KEY` for Anthropic (Claude) models.
- `OPENAI_API_KEY` for OpenAI models.
- `LITELLM_API_URL` + `LITELLM_API_KEY` to route through a LiteLLM proxy.

Agents that make no model calls (pure compute, local tools, workspace reads)
need no key.

## Permissions and sandboxing

Agents run in a pure-Rust JavaScript sandbox — there is no raw shell or
unfettered filesystem access. Effects that touch the outside world go through a
policy layer. By default `chidori run` is permissive; `--untrusted` switches to
deny-by-default (workspace reads allowed, writes and network denied unless
approved). The workspace is always scoped to the project directory.
"#;

/// One file a template writes, relative to the target directory.
struct TemplateFile {
    path: &'static str,
    contents: &'static str,
}

/// A starter project template.
pub struct Template {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    files: &'static [TemplateFile],
    /// Command to print after scaffolding so the user can run it immediately.
    run_hint: &'static str,
}

const DOCS: Template = Template {
    key: "docs",
    title: "Docs chat",
    description: "Chat with the bundled Chidori docs — a 30-second taste of agents + replay.",
    files: &[
        TemplateFile {
            path: "agent.ts",
            contents: DOCS_AGENT_SRC,
        },
        TemplateFile {
            path: "docs/chidori.md",
            contents: DOCS_CORPUS,
        },
        TemplateFile {
            path: "README.md",
            contents: DOCS_README,
        },
    ],
    run_hint: "chidori chat agent.ts",
};

const CHAT: Template = Template {
    key: "chat",
    title: "Chat agent",
    description: "A conversational assistant you talk to with `chidori chat`.",
    files: &[
        TemplateFile {
            path: "agent.ts",
            contents: CHAT_AGENT_SRC,
        },
        TemplateFile {
            path: "README.md",
            contents: CHAT_README,
        },
    ],
    run_hint: "chidori chat agent.ts",
};

// Inlined rather than include_str!'d from examples/ so the crate packages
// self-contained for crates.io (examples/ lives outside the package root).
// Mirrors examples/agents/worker.ts and examples/tools/reverse.ts.
const WORKER_AGENT_SRC: &str = r#"import type { Chidori } from "chidori:agent";

/**
 * An autonomous "worker" agent: it loops — think, call a tool, observe the
 * result, repeat — until it produces an answer with no further tool calls.
 *
 * The loop is author-driven via `context.respond()`, which returns the model's
 * structured turn (`tool_calls` + `text`). Tool results are appended back to the
 * context with `toolResult(...)`, and the next `respond()` continues from there.
 * Every turn and tool call is a durable host call, so the whole run replays for
 * free.
 *
 * Run:
 *   chidori run examples/agents/worker.ts \
 *     --input task="Reverse the word 'chidori' and tell me the result." \
 *     --tools examples/tools
 */
export async function agent(
  input: { task: string; maxSteps?: number },
  chidori: Chidori,
) {
  const maxSteps = input.maxSteps ?? 8;

  let ctx = chidori
    .context()
    .system(
      "You are an autonomous worker. Use the available tools to complete the " +
        "task. Call a tool when it helps; when you are finished, reply with a " +
        "final answer and no tool calls.",
    )
    .tools(["reverse"]) // tool names discovered from the --tools directory
    .user(input.task);

  const steps: { tool: string; input: unknown; result: unknown }[] = [];

  for (let step = 0; step < maxSteps; step++) {
    const { response, context } = await ctx.respond({ type: "final" });
    ctx = context; // the assistant turn (incl. tool-use blocks) is now in ctx

    // No tool calls means the worker is done.
    if (!response.tool_calls || response.tool_calls.length === 0) {
      return { answer: response.content, steps };
    }

    // Run each requested tool and feed the result back for the next turn.
    for (const call of response.tool_calls) {
      const result = await chidori.tool(call.name, call.input);
      steps.push({ tool: call.name, input: call.input, result });
      ctx = ctx.toolResult(call.id, JSON.stringify(result));
    }
  }

  return { answer: "(stopped: reached maxSteps without finishing)", steps };
}
"#;

const REVERSE_TOOL_SRC: &str = r#"import type { ToolDefinition } from "chidori:agent";

/**
 * A sample tool for the worker agent. Reverses a string. Replace the body (and
 * the schema) with whatever your agent needs — an API call, a DB query, a
 * computation. Every `chidori.tool(...)` call is policy-gated and recorded.
 */
export const tool: ToolDefinition = {
  name: "reverse",
  description: "Reverse a string and return it. A sample tool — replace with your own.",
  parameters: {
    type: "object",
    properties: {
      text: { type: "string", description: "The text to reverse" },
    },
    required: ["text"],
  },
};

export async function run(args: { text: string }) {
  return { reversed: [...String(args.text)].reverse().join("") };
}
"#;

const WORKER: Template = Template {
    key: "worker",
    title: "Worker agent",
    description: "An autonomous agent that loops over tools until the task is done.",
    files: &[
        TemplateFile {
            path: "agent.ts",
            contents: WORKER_AGENT_SRC,
        },
        TemplateFile {
            path: "tools/reverse.ts",
            contents: REVERSE_TOOL_SRC,
        },
        TemplateFile {
            path: "README.md",
            contents: WORKER_README,
        },
    ],
    run_hint:
        "chidori run agent.ts --input task=\"Reverse the word 'chidori' and tell me the result.\" --tools tools",
};

pub const TEMPLATES: &[&Template] = &[&DOCS, &CHAT, &WORKER];

/// Scaffold a template into `dir`. With `template_key` unset, prompt the user to
/// pick one. Refuses to overwrite existing files.
pub fn run(dir: &Path, template_key: Option<&str>) -> Result<()> {
    let Some(template) = select_template(template_key)? else {
        return Ok(()); // user quit the picker
    };

    // Refuse to clobber: collect any conflicts before writing anything.
    let conflicts: Vec<String> = template
        .files
        .iter()
        .map(|f| dir.join(f.path))
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if !conflicts.is_empty() {
        bail!(
            "refusing to overwrite existing file(s): {}",
            conflicts.join(", ")
        );
    }

    println!(
        "Scaffolding '{}' template into {}",
        template.key,
        dir.display()
    );
    for file in template.files {
        let target = dir.join(file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&target, file.contents)
            .with_context(|| format!("writing {}", target.display()))?;
        println!("  created {}", target.display());
    }

    println!();
    println!("Next:");
    if dir != Path::new(".") {
        println!("  cd {}", dir.display());
    }
    println!("  {}", template.run_hint);
    Ok(())
}

fn select_template(key: Option<&str>) -> Result<Option<&'static Template>> {
    if let Some(key) = key {
        return TEMPLATES
            .iter()
            .copied()
            .find(|t| t.key.eq_ignore_ascii_case(key))
            .map(Some)
            .ok_or_else(|| {
                let keys: Vec<&str> = TEMPLATES.iter().map(|t| t.key).collect();
                anyhow::anyhow!("unknown template '{key}'. Available: {}", keys.join(", "))
            });
    }
    prompt_template_choice()
}

fn prompt_template_choice() -> Result<Option<&'static Template>> {
    use std::io::Write;

    println!("Chidori init — choose a template:");
    println!();
    for (idx, template) in TEMPLATES.iter().enumerate() {
        println!(
            "  {}. {} — {}",
            idx + 1,
            template.title,
            template.description
        );
    }
    println!();

    loop {
        print!("Choose a template [1-{}] or q to quit: ", TEMPLATES.len());
        std::io::stdout().flush()?;

        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let value = line.trim();
        if value.eq_ignore_ascii_case("q") || value.eq_ignore_ascii_case("quit") {
            return Ok(None);
        }
        // Accept a number or the template key by name.
        if let Ok(choice) = value.parse::<usize>() {
            if (1..=TEMPLATES.len()).contains(&choice) {
                return Ok(Some(TEMPLATES[choice - 1]));
            }
        }
        if let Some(template) = TEMPLATES
            .iter()
            .copied()
            .find(|t| t.key.eq_ignore_ascii_case(value))
        {
            return Ok(Some(template));
        }
        eprintln!(
            "Enter a number from 1 to {}, a template name, or q.",
            TEMPLATES.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("chidori-init-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scaffolds_docs_template() {
        let dir = temp_dir("docs");
        run(&dir, Some("docs")).unwrap();
        let agent = std::fs::read_to_string(dir.join("agent.ts")).unwrap();
        assert!(agent.contains("chidori.workspace.list("));
        let corpus = std::fs::read_to_string(dir.join("docs/chidori.md")).unwrap();
        assert!(corpus.contains("host call"));
        assert!(dir.join("README.md").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scaffolds_chat_template() {
        let dir = temp_dir("chat");
        run(&dir, Some("chat")).unwrap();
        let agent = std::fs::read_to_string(dir.join("agent.ts")).unwrap();
        assert!(agent.contains("chidori.conversation("));
        assert!(dir.join("README.md").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scaffolds_worker_template_with_tool() {
        let dir = temp_dir("worker");
        run(&dir, Some("worker")).unwrap();
        assert!(dir.join("agent.ts").exists());
        let tool = std::fs::read_to_string(dir.join("tools/reverse.ts")).unwrap();
        assert!(tool.contains("name: \"reverse\""));
        assert!(dir.join("README.md").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_unknown_template() {
        let dir = temp_dir("unknown");
        let err = run(&dir, Some("droid")).unwrap_err().to_string();
        assert!(err.contains("unknown template"));
        assert!(err.contains("docs, chat, worker"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_to_overwrite_existing_files() {
        let dir = temp_dir("conflict");
        std::fs::write(dir.join("agent.ts"), "// existing\n").unwrap();
        let err = run(&dir, Some("chat")).unwrap_err().to_string();
        assert!(err.contains("refusing to overwrite"));
        // The pre-existing file is left untouched.
        assert_eq!(
            std::fs::read_to_string(dir.join("agent.ts")).unwrap(),
            "// existing\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
