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
pub const CHAT_AGENT_SRC: &str = r#"import type { Chidori } from "chidori";

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

const WORKER: Template = Template {
    key: "worker",
    title: "Worker agent",
    description: "An autonomous agent that loops over tools until the task is done.",
    files: &[
        TemplateFile {
            path: "agent.ts",
            contents: include_str!("../examples/agents/worker.ts"),
        },
        TemplateFile {
            path: "tools/reverse.ts",
            contents: include_str!("../examples/tools/reverse.ts"),
        },
        TemplateFile {
            path: "README.md",
            contents: WORKER_README,
        },
    ],
    run_hint:
        "chidori run agent.ts --input task=\"Reverse the word 'chidori' and tell me the result.\" --tools tools",
};

pub const TEMPLATES: &[&Template] = &[&CHAT, &WORKER];

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
        assert!(err.contains("chat, worker"));
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
