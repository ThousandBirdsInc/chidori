# Coding Agent Example

A goose-style coding agent built on chidori. It can read, edit, search, analyze, and navigate codebases, run shell commands, and track progress with a todo list.

## Tools

| Tool | Description |
|------|-------------|
| `read` | Read file contents with line numbers. Supports `start_line`/`end_line` for partial reads of large files |
| `text_editor` | Edit files via find-and-replace with uniqueness validation |
| `create_file` | Create new files with automatic parent directory creation |
| `tree` | List directory structure with line counts per file |
| `search` | Ripgrep-powered code search with glob filtering |
| `shell_exec` | Run shell commands (tests, builds, git). Output >2000 lines is truncated and saved to a temp file |
| `todo` | Track multi-step task progress via a persistent checklist |
| `analyze` | Show functions, classes, and symbols in a file without reading the full content (uses ctags or grep fallback) |

## Usage

```bash
# Allow the shell commands the agent needs
export CHIDORI_SHELL_ALLOW=rg,find,git,cargo,npm,python,pytest,make,ls,cat,echo,head,tail,wc,date,test,mkdir,sleep,ctags

# Run the coding agent on a task (tools/ is auto-discovered next to agent.star)
./target/release/chidori run examples/coding-agent/agent.star \
  --input task="Add error handling to the parse_config function in src/config.rs" \
  --input working_dir="/path/to/your/project"

# Use current directory as working_dir (the default)
./target/release/chidori run examples/coding-agent/agent.star \
  --input task="Fix the failing test in tests/integration.rs"

# Stream output for real-time feedback
./target/release/chidori run examples/coding-agent/agent.star \
  --input task="Refactor the database module" \
  --stream
```

## How It Works

1. The agent gathers context: git branch, timestamp, and a top-level directory listing
2. A system prompt (Jinja template) is rendered with this context, establishing the coding agent persona and workflow
3. The LLM runs in an agentic loop (up to 50 turns) with access to all 8 tools
4. It follows a structured workflow: understand → plan → edit → verify
5. The todo tool uses the framework's memory system to persist task state across turns
6. Shell output is automatically truncated to prevent context overflow

## Customization

- **System prompt**: Edit `prompts/system.jinja` to change the agent's behavior
- **Model**: Change `config(model = ...)` in `agent.star`
- **Tools**: Add `.star` files to `tools/` to give the agent new capabilities
- **Max turns**: Adjust `max_turns` in `agent.star` for longer/shorter sessions
- **Context**: Add custom context via the `context` template variable in `agent.star`
