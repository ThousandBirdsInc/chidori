use std::path::PathBuf;

use anyhow::{Context, Result};
use minijinja::Environment;
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

    /// Render an inline template string with the given variables.
    pub fn render_string(&self, template: &str, vars: &Value) -> Result<String> {
        let mut env = self.create_env();
        env.add_template("__inline__", template)
            .context("Failed to parse inline template")?;
        let tmpl = env.get_template("__inline__").unwrap();
        tmpl.render(vars)
            .context("Failed to render inline template")
    }

    /// Render a template from a .jinja file with the given variables.
    pub fn render_file(&self, path: &str, vars: &Value) -> Result<String> {
        let full_path = self.base_dir.join(path);
        let source = std::fs::read_to_string(&full_path)
            .with_context(|| format!("Failed to read template file: {}", full_path.display()))?;

        let mut env = self.create_env();

        // Set up a loader for includes/extends relative to the template's directory.
        let template_dir = full_path
            .parent()
            .unwrap_or(&self.base_dir)
            .to_path_buf();
        env.set_loader(move |name| {
            let p = template_dir.join(name);
            match std::fs::read_to_string(&p) {
                Ok(content) => Ok(Some(content)),
                Err(_) => Ok(None),
            }
        });

        env.add_template("__file__", &source)
            .with_context(|| format!("Failed to parse template: {path}"))?;
        let tmpl = env.get_template("__file__").unwrap();
        tmpl.render(vars)
            .with_context(|| format!("Failed to render template: {path}"))
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
        env.set_trim_blocks(true);
        env.set_lstrip_blocks(true);
        env
    }
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
}
