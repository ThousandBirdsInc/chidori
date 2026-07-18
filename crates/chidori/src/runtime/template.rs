use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use minijinja::{Environment, ErrorKind, UndefinedBehavior};
use serde_json::Value;

/// Template engine backed by minijinja.
///
/// Supports both inline template strings and loading from .jinja files.
/// Templates use Jinja2 syntax: {{ var }}, {% if %}, {% for %}, filters, etc.
pub struct TemplateEngine {
    /// Base directory for resolving .jinja file paths.
    base_dir: PathBuf,
}

impl TemplateEngine {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// The project root templates resolve against — also the anchor for other
    /// project-relative resources (e.g. `chidori.callAgent` sub-agent paths).
    pub fn base_dir(&self) -> &std::path::Path {
        &self.base_dir
    }

    /// Render an inline template string with the given variables.
    pub fn render_string(&self, template: &str, vars: &Value) -> Result<String> {
        let mut env = self.create_env();
        env.add_template("__inline__", template)
            .map_err(|err| describe_template_error("parse", "inline template", &err))?;
        let tmpl = env.get_template("__inline__").unwrap();
        tmpl.render(vars)
            .map_err(|err| describe_template_error("render", "inline template", &err))
    }

    /// Render a template from a .jinja file with the given variables.
    pub fn render_file(&self, path: &str, vars: &Value) -> Result<String> {
        let full_path = self.base_dir.join(path);
        let source = std::fs::read_to_string(&full_path)
            .with_context(|| format!("Failed to read template file: {}", full_path.display()))?;

        let mut env = self.create_env();

        // Set up a loader for includes/extends relative to the template's directory.
        let template_dir = full_path.parent().unwrap_or(&self.base_dir).to_path_buf();
        env.set_loader(move |name| {
            let p = template_dir.join(name);
            match std::fs::read_to_string(&p) {
                Ok(content) => Ok(Some(content)),
                Err(_) => Ok(None),
            }
        });

        env.add_template("__file__", &source)
            .map_err(|err| describe_template_error("parse", &format!("template {path}"), &err))?;
        let tmpl = env.get_template("__file__").unwrap();
        tmpl.render(vars)
            .map_err(|err| describe_template_error("render", &format!("template {path}"), &err))
    }

    /// Determine if a template arg is a file path or an inline string, and render it.
    pub fn render(&self, template_or_path: &str, vars: &Value) -> Result<String> {
        // If it ends with .jinja or .j2, treat as a file path.
        if template_or_path.ends_with(".jinja") || template_or_path.ends_with(".j2") {
            self.render_file(template_or_path, vars)
        } else {
            self.render_string(template_or_path, vars)
        }
    }

    fn create_env(&self) -> Environment<'static> {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::SemiStrict);
        env.set_trim_blocks(true);
        env.set_lstrip_blocks(true);
        // Embed the template source in errors so failure messages can point at
        // the offending expression and its line/column (on by default only in
        // debug builds; force it on so release builds report the same detail).
        env.set_debug(true);
        env
    }
}

/// Turn a minijinja error into a single self-contained anyhow error whose
/// primary message carries the located detail — error kind, offending
/// expression (e.g. the undefined variable's name), and line/column. Without
/// this the detail lives only in the anyhow source chain, which the runtime
/// drops when it stringifies errors for the user-facing frame.
fn describe_template_error(action: &str, what: &str, err: &minijinja::Error) -> anyhow::Error {
    let mut msg = format!("Failed to {action} {what}: {}", err.kind());
    if let Some(detail) = err.detail() {
        let _ = write!(msg, ": {detail}");
    }
    // The error's span points at the failing expression in the template
    // source; for an undefined-variable error that is the variable itself.
    let located = err
        .template_source()
        .zip(err.range())
        .and_then(|(source, range)| Some((source, source.get(range.clone())?, range.start)));
    if let Some((_, snippet, _)) = located {
        let snippet = snippet.trim();
        if !snippet.is_empty() && !snippet.contains('\n') && snippet.len() <= 80 {
            if err.kind() == ErrorKind::UndefinedError {
                let _ = write!(msg, " (variable `{snippet}`)");
            } else {
                let _ = write!(msg, " (in `{snippet}`)");
            }
        }
    }
    if let Some(line) = err.line() {
        let _ = write!(msg, " at line {line}");
        if let Some(col) = located.and_then(|(source, _, start)| column_at(source, start)) {
            let _ = write!(msg, ", column {col}");
        }
    }
    // Chained errors (e.g. a failing include wraps the real cause) would
    // otherwise stay hidden as sources — fold them into the message.
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let _ = write!(msg, ": {cause}");
        source = cause.source();
    }
    anyhow::anyhow!(msg)
}

/// 1-based column of `offset` within its line of `source`.
fn column_at(source: &str, offset: usize) -> Option<usize> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let line_start = source[..offset].rfind('\n').map_or(0, |pos| pos + 1);
    Some(source[line_start..offset].chars().count() + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_render_inline_simple() {
        let engine = TemplateEngine::new(".");
        let result = engine
            .render_string("Hello {{ name }}!", &json!({"name": "world"}))
            .unwrap();
        assert_eq!(result, "Hello world!");
    }

    #[test]
    fn test_render_inline_loop() {
        let engine = TemplateEngine::new(".");
        let result = engine
            .render_string(
                "Items:\n{% for item in items %}- {{ item }}\n{% endfor %}",
                &json!({"items": ["a", "b", "c"]}),
            )
            .unwrap();
        assert_eq!(result, "Items:\n- a\n- b\n- c\n");
    }

    #[test]
    fn test_render_inline_conditional() {
        let engine = TemplateEngine::new(".");
        let result = engine
            .render_string(
                "{% if verbose %}Full details{% else %}Summary{% endif %}",
                &json!({"verbose": true}),
            )
            .unwrap();
        assert_eq!(result, "Full details");
    }

    #[test]
    fn test_render_missing_variable_errors() {
        let engine = TemplateEngine::new(".");
        let result = engine.render_string("Hello {{ name }}!", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_render_inline_undefined_variable_error_names_variable_and_location() {
        let engine = TemplateEngine::new(".");
        let err = engine
            .render_string("line one\nHello {{ usernme }}!", &json!({"username": "x"}))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("inline template"),
            "missing template name: {msg}"
        );
        assert!(msg.contains("undefined value"), "missing error kind: {msg}");
        assert!(msg.contains("`usernme`"), "missing variable name: {msg}");
        assert!(msg.contains("line 2"), "missing line number: {msg}");
        assert!(msg.contains("column 10"), "missing column: {msg}");
    }

    #[test]
    fn test_render_inline_unknown_filter_error_names_filter_and_line() {
        let engine = TemplateEngine::new(".");
        let err = engine
            .render_string("{{ name | shuot }}", &json!({"name": "x"}))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("inline template"),
            "missing template name: {msg}"
        );
        assert!(msg.contains("unknown filter"), "missing error kind: {msg}");
        assert!(msg.contains("shuot"), "missing filter name: {msg}");
        assert!(msg.contains("line 1"), "missing line number: {msg}");
    }

    #[test]
    fn test_render_file_error_names_path_variable_and_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("summary.md.j2"),
            "# Report\n\nAuthor: {{ author }}\nUser: {{ usernme }}\n",
        )
        .unwrap();
        let engine = TemplateEngine::new(dir.path());
        let err = engine
            .render_file("summary.md.j2", &json!({"author": "a", "username": "x"}))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("summary.md.j2"),
            "missing template path: {msg}"
        );
        assert!(msg.contains("undefined value"), "missing error kind: {msg}");
        assert!(msg.contains("`usernme`"), "missing variable name: {msg}");
        assert!(msg.contains("line 4"), "missing line number: {msg}");
    }

    #[test]
    fn test_parse_error_names_line() {
        let engine = TemplateEngine::new(".");
        let err = engine
            .render_string("ok\n{% if x %}\nnever closed", &json!({}))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Failed to parse"), "missing action: {msg}");
        assert!(msg.contains("syntax error"), "missing error kind: {msg}");
    }

    #[test]
    fn test_render_wrong_shape_in_loop_errors() {
        let engine = TemplateEngine::new(".");
        let result = engine.render_string(
            "{% for source in sources %}{{ source.title }}{% endfor %}",
            &json!({"sources": "not a source list"}),
        );
        assert!(result.is_err());
    }
}
