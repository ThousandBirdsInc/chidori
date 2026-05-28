use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::runtime::snapshot::TypeScriptImportPolicy;

#[derive(Debug, Clone, Copy)]
pub struct TranspileOptions {
    pub import_policy: TypeScriptImportPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleImport {
    pub specifier: String,
    pub resolved_path: Option<PathBuf>,
}

pub fn transpile_module(path: &Path, source: &str, options: &TranspileOptions) -> Result<String> {
    validate_imports(path, source, options.import_policy)?;

    let mut out = String::with_capacity(source.len());
    let mut param_state = ParameterStripState::default();
    let mut type_skip_state: Option<TypeDeclarationSkipState> = None;
    let mut assertion_skip_state: Option<TypeAssertionSkipState> = None;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if let Some(state) = type_skip_state.as_mut() {
            state.observe_line(trimmed);
            if state.is_done() {
                type_skip_state = None;
            }
            out.push('\n');
            continue;
        }
        if let Some(state) = assertion_skip_state.as_mut() {
            state.observe_line(line);
            if state.is_done() {
                assertion_skip_state = None;
            }
            out.push('\n');
            continue;
        }
        if trimmed.starts_with("import type ") || is_chidori_sdk_import(trimmed) {
            out.push('\n');
            continue;
        }
        if trimmed.starts_with("import ") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if is_type_declaration(trimmed) {
            let mut state = TypeDeclarationSkipState::new(trimmed);
            state.observe_line(trimmed);
            if !state.is_done() {
                type_skip_state = Some(state);
            }
            out.push('\n');
            continue;
        }
        if let Some((stripped, state)) = strip_multiline_type_assertion_start(line) {
            out.push_str(&stripped);
            out.push('\n');
            assertion_skip_state = Some(state);
            continue;
        }
        out.push_str(&strip_type_syntax(line, &mut param_state));
        out.push('\n');
    }
    Ok(out)
}

pub fn validate_imports(
    path: &Path,
    source: &str,
    policy: TypeScriptImportPolicy,
) -> Result<Vec<ModuleImport>> {
    let mut imports = Vec::new();
    let project_root = path.parent().unwrap_or_else(|| Path::new("."));

    for (line_no, line) in source.lines().enumerate() {
        if line.contains("import(") || line.contains("import (") {
            anyhow::bail!(
                "{}:{}: dynamic import is disabled in durable TypeScript agents",
                path.display(),
                line_no + 1
            );
        }
    }

    for (line_no, specifier) in import_specifiers(source) {
        if specifier == "chidori" {
            imports.push(ModuleImport {
                specifier,
                resolved_path: None,
            });
            continue;
        }

        match policy {
            TypeScriptImportPolicy::None => {
                anyhow::bail!(
                    "{}:{}: local TypeScript imports are disabled: {}",
                    path.display(),
                    line_no,
                    specifier
                );
            }
            TypeScriptImportPolicy::Relative => {
                if !is_relative_import(&specifier) {
                    anyhow::bail!(
                        "{}:{}: bare TypeScript imports are disabled: {}",
                        path.display(),
                        line_no,
                        specifier
                    );
                }
                let resolved = resolve_relative_import(path, project_root, &specifier, line_no)?;
                imports.push(ModuleImport {
                    specifier,
                    resolved_path: Some(resolved),
                });
            }
            TypeScriptImportPolicy::Project => {
                if is_relative_import(&specifier) {
                    let resolved =
                        resolve_relative_import(path, project_root, &specifier, line_no)?;
                    imports.push(ModuleImport {
                        specifier,
                        resolved_path: Some(resolved),
                    });
                } else {
                    imports.push(ModuleImport {
                        specifier,
                        resolved_path: None,
                    });
                }
            }
        }
    }

    Ok(imports)
}

fn import_specifiers(source: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (line_no, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with("import ") {
            continue;
        }
        if let Some(specifier) =
            specifier_after_from(trimmed).or_else(|| side_effect_import(trimmed))
        {
            out.push((line_no + 1, specifier));
        }
    }
    out
}

fn is_chidori_sdk_import(line: &str) -> bool {
    line.starts_with("import ") && specifier_after_from(line).as_deref() == Some("chidori")
}

fn is_type_declaration(line: &str) -> bool {
    line.starts_with("type ")
        || line.starts_with("interface ")
        || line.starts_with("export type ")
        || line.starts_with("export interface ")
}

fn specifier_after_from(line: &str) -> Option<String> {
    let from_index = line.find(" from ")?;
    quoted_specifier(&line[from_index + 6..])
}

fn side_effect_import(line: &str) -> Option<String> {
    let rest = line.strip_prefix("import ")?;
    quoted_specifier(rest)
}

fn quoted_specifier(input: &str) -> Option<String> {
    let input = input.trim_start();
    let quote = input.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = input[1..].find(quote)?;
    Some(input[1..1 + end].to_string())
}

fn is_relative_import(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../")
}

fn resolve_relative_import(
    source_path: &Path,
    project_root: &Path,
    specifier: &str,
    line_no: usize,
) -> Result<PathBuf> {
    let path = project_root.join(specifier);
    let normalized = normalize_path(&path);
    let root = normalize_path(project_root);
    if !normalized.starts_with(&root) {
        anyhow::bail!(
            "{}:{}: TypeScript import escapes project root: {}",
            source_path.display(),
            line_no,
            specifier
        );
    }
    // Bundler-style extension probing. ts-node, Bun, Deno, and the TS Bundler
    // resolver all let `import "./tools/x"` resolve to `./tools/x.ts`; agents
    // should not have to know our quickjs loader is stricter. If the specifier
    // already names a file (with or without a known extension), use it as-is.
    // Otherwise probe `.ts`/`.tsx`/`.js`/`.mjs`/`.cjs` and return the first
    // that exists. If none exist, default to the `.ts` candidate so a missing-
    // file error names the most likely intended path.
    if normalized.is_file() {
        return Ok(normalized);
    }
    let known_extensions = ["ts", "tsx", "js", "mjs", "cjs", "json"];
    let has_known_ext = Path::new(specifier)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| known_extensions.contains(&ext))
        .unwrap_or(false);
    if has_known_ext {
        return Ok(normalized);
    }
    for ext in ["ts", "tsx", "js", "mjs", "cjs"] {
        let mut candidate = normalized.clone().into_os_string();
        candidate.push(".");
        candidate.push(ext);
        let candidate = PathBuf::from(candidate);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let mut fallback = normalized.into_os_string();
    fallback.push(".ts");
    Ok(PathBuf::from(fallback))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Default)]
struct ParameterStripState {
    active: bool,
    paren_depth: usize,
}

struct TypeAssertionSkipState {
    brace_depth: usize,
    saw_semicolon: bool,
}

enum TypeDeclarationSkipState {
    Interface {
        brace_depth: usize,
        saw_brace: bool,
    },
    TypeAlias {
        brace_depth: usize,
        saw_semicolon: bool,
    },
}

impl TypeDeclarationSkipState {
    fn new(line: &str) -> Self {
        let trimmed = line.trim_start();
        if trimmed.starts_with("interface ") || trimmed.starts_with("export interface ") {
            Self::Interface {
                brace_depth: 0,
                saw_brace: false,
            }
        } else {
            Self::TypeAlias {
                brace_depth: 0,
                saw_semicolon: false,
            }
        }
    }

    fn observe_line(&mut self, line: &str) {
        let (open, close) = brace_delta(line);
        match self {
            Self::Interface {
                brace_depth,
                saw_brace,
            } => {
                *saw_brace |= open > 0;
                *brace_depth = brace_depth.saturating_add(open).saturating_sub(close);
            }
            Self::TypeAlias {
                brace_depth,
                saw_semicolon,
            } => {
                *brace_depth = brace_depth.saturating_add(open).saturating_sub(close);
                *saw_semicolon |= line.trim_end().ends_with(';');
            }
        }
    }

    fn is_done(&self) -> bool {
        match self {
            Self::Interface {
                brace_depth,
                saw_brace,
            } => *saw_brace && *brace_depth == 0,
            Self::TypeAlias {
                brace_depth,
                saw_semicolon,
            } => *saw_semicolon && *brace_depth == 0,
        }
    }
}

fn brace_delta(line: &str) -> (usize, usize) {
    line.chars().fold((0, 0), |(open, close), ch| match ch {
        '{' => (open + 1, close),
        '}' => (open, close + 1),
        _ => (open, close),
    })
}

fn strip_type_syntax(line: &str, param_state: &mut ParameterStripState) -> String {
    let without_catch = strip_catch_parameter_annotations(line);
    let without_returns = strip_return_type(&without_catch);
    let without_params = strip_parameter_annotations(&without_returns, param_state);
    let without_multiline_return = strip_return_type_after_any_close_paren(&without_params);
    let without_arrow_params = strip_arrow_parameter_annotations(&without_multiline_return);
    let without_variables = strip_variable_annotations(&without_arrow_params);
    strip_type_assertions(&without_variables)
}

fn strip_multiline_type_assertion_start(line: &str) -> Option<(String, TypeAssertionSkipState)> {
    let idx = find_outside_string(line, " as ")?;
    if line[idx..].contains(';') {
        return None;
    }
    let asserted_type = &line[idx + " as ".len()..];
    if !asserted_type.contains('{') && !asserted_type.contains('<') {
        return None;
    }
    let (open, close) = brace_delta(asserted_type);
    let mut stripped = line[..idx].trim_end().to_string();
    stripped.push(';');
    Some((
        stripped,
        TypeAssertionSkipState {
            brace_depth: open.saturating_sub(close),
            saw_semicolon: false,
        },
    ))
}

impl TypeAssertionSkipState {
    fn observe_line(&mut self, line: &str) {
        let (open, close) = brace_delta(line);
        self.brace_depth = self.brace_depth.saturating_add(open).saturating_sub(close);
        self.saw_semicolon |= line.trim_end().ends_with(';');
    }

    fn is_done(&self) -> bool {
        self.brace_depth == 0 && self.saw_semicolon
    }
}

fn strip_catch_parameter_annotations(line: &str) -> String {
    let Some(catch_idx) = line.find("catch") else {
        return line.to_string();
    };
    let Some(open_rel) = line[catch_idx..].find('(') else {
        return line.to_string();
    };
    let open_paren = catch_idx + open_rel;
    let Some(close_paren) = matching_close_paren(line, open_paren) else {
        return line.to_string();
    };
    if !line[open_paren + 1..close_paren].contains(':') {
        return line.to_string();
    }
    format!(
        "{}{}{}",
        &line[..open_paren + 1],
        strip_colon_annotations(&line[open_paren + 1..close_paren]),
        &line[close_paren..]
    )
}

fn strip_return_type(line: &str) -> String {
    let Some(function_start) = line.find("function ") else {
        return line.to_string();
    };
    let Some(open_rel) = line[function_start..].find('(') else {
        return line.to_string();
    };
    let open_paren = function_start + open_rel;
    let Some(close_paren) = matching_close_paren(line, open_paren) else {
        return line.to_string();
    };
    strip_return_type_after_close_paren(line, close_paren).unwrap_or_else(|| line.to_string())
}

fn strip_return_type_after_any_close_paren(line: &str) -> String {
    let mut cursor = 0usize;
    while let Some(close_rel) = line[cursor..].find(')') {
        let close_paren = cursor + close_rel;
        if let Some(stripped) = strip_return_type_after_close_paren(line, close_paren) {
            return stripped;
        }
        cursor = close_paren + 1;
    }
    line.to_string()
}

fn strip_return_type_after_close_paren(line: &str, close_paren: usize) -> Option<String> {
    let tail = &line[close_paren + 1..];
    let trimmed = tail.trim_start();
    if !trimmed.starts_with(':') {
        return None;
    }
    let whitespace = tail.len() - trimmed.len();
    let type_start = close_paren + 1 + whitespace;
    let Some(body_start) = find_function_body_start(line, type_start + 1) else {
        return None;
    };
    Some(format!(
        "{} {}",
        &line[..close_paren + 1],
        &line[body_start..]
    ))
}

fn matching_close_paren(source: &str, open_paren: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in source[open_paren..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open_paren + idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_function_body_start(line: &str, return_type_start: usize) -> Option<usize> {
    let mut angle_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    for (idx, ch) in line[return_type_start..].char_indices() {
        let abs = return_type_start + idx;
        match ch {
            '<' if brace_depth == 0 && paren_depth == 0 && bracket_depth == 0 => {
                angle_depth += 1;
            }
            '>' if angle_depth > 0
                && brace_depth == 0
                && paren_depth == 0
                && bracket_depth == 0 =>
            {
                angle_depth -= 1;
            }
            '{' if angle_depth == 0
                && brace_depth == 0
                && paren_depth == 0
                && bracket_depth == 0 =>
            {
                let previous = previous_nonspace(line, abs);
                if matches!(
                    previous,
                    Some(':')
                        | Some('<')
                        | Some('|')
                        | Some('&')
                        | Some('(')
                        | Some('[')
                        | Some(',')
                ) {
                    brace_depth += 1;
                } else {
                    return Some(abs);
                }
            }
            '{' => brace_depth += 1,
            '}' if brace_depth > 0 => brace_depth -= 1,
            '(' => paren_depth += 1,
            ')' if paren_depth > 0 => paren_depth -= 1,
            '[' => bracket_depth += 1,
            ']' if bracket_depth > 0 => bracket_depth -= 1,
            _ => {}
        }
    }
    None
}

fn previous_nonspace(source: &str, before: usize) -> Option<char> {
    source[..before]
        .chars()
        .rev()
        .find(|ch| !ch.is_whitespace())
}

fn strip_parameter_annotations(line: &str, state: &mut ParameterStripState) -> String {
    let mut out = String::with_capacity(line.len());
    let mut cursor = 0usize;

    while cursor < line.len() {
        if !state.active {
            let Some(function_rel) = line[cursor..].find("function ") else {
                out.push_str(&line[cursor..]);
                break;
            };
            let function_start = cursor + function_rel;
            let Some(open_rel) = line[function_start..].find('(') else {
                out.push_str(&line[cursor..]);
                break;
            };
            let open_paren = function_start + open_rel;
            out.push_str(&line[cursor..open_paren + 1]);
            cursor = open_paren + 1;
            state.active = true;
            state.paren_depth = 1;
        }

        let segment_start = cursor;
        let mut segment = String::new();
        while cursor < line.len() {
            let Some(ch) = line[cursor..].chars().next() else {
                break;
            };
            let next = cursor + ch.len_utf8();
            match ch {
                '(' => {
                    state.paren_depth += 1;
                    segment.push(ch);
                }
                ')' => {
                    state.paren_depth = state.paren_depth.saturating_sub(1);
                    if state.paren_depth == 0 {
                        out.push_str(&strip_colon_annotations(&segment));
                        out.push(')');
                        cursor = next;
                        state.active = false;
                        break;
                    }
                    segment.push(ch);
                }
                _ => segment.push(ch),
            }
            cursor = next;
        }

        if state.active {
            out.push_str(&strip_colon_annotations(&line[segment_start..cursor]));
            break;
        }
    }

    out
}

fn strip_variable_annotations(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let Some(keyword) = [
        "export const ",
        "export let ",
        "export var ",
        "const ",
        "let ",
        "var ",
    ]
    .iter()
    .find(|keyword| trimmed.starts_with(**keyword)) else {
        return line.to_string();
    };

    let name_start = indent + keyword.len();
    let rest = &line[name_start..];
    let Some(colon_rel) = rest.find(':') else {
        return line.to_string();
    };
    if rest.find('=').is_some_and(|eq_rel| eq_rel < colon_rel) {
        return line.to_string();
    }
    let colon = name_start + colon_rel;
    if let Some(eq_rel) = line[colon + 1..].find('=') {
        let eq = colon + 1 + eq_rel;
        return format!("{} {}", &line[..colon], &line[eq..]);
    }

    if let Some(semicolon_rel) = line[colon + 1..].find(';') {
        let semicolon = colon + 1 + semicolon_rel;
        return format!("{}{}", &line[..colon], &line[semicolon..]);
    }

    line[..colon].to_string()
}

fn strip_type_assertions(line: &str) -> String {
    let without_satisfies = strip_satisfies_operator(line);
    let without_as_const = strip_as_const_assertion(&without_satisfies);
    strip_as_type_assertion(&without_as_const)
}

fn strip_satisfies_operator(line: &str) -> String {
    let Some(idx) = find_outside_string(line, " satisfies ") else {
        return line.to_string();
    };
    let statement_end = line[idx..]
        .find(';')
        .map(|rel| idx + rel)
        .unwrap_or(line.len());
    format!("{}{}", &line[..idx], &line[statement_end..])
}

fn strip_as_const_assertion(line: &str) -> String {
    let mut out = line.to_string();
    while let Some(idx) = find_outside_string(&out, " as const") {
        let end = idx + " as const".len();
        out.replace_range(idx..end, "");
    }
    out
}

fn strip_as_type_assertion(line: &str) -> String {
    let Some(idx) = find_outside_string(line, " as ") else {
        return line.to_string();
    };
    let statement_end = line[idx..]
        .find(';')
        .map(|rel| idx + rel)
        .unwrap_or(line.len());
    format!("{}{}", &line[..idx], &line[statement_end..])
}

fn find_outside_string(source: &str, needle: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in source.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }

        if ch == '"' || ch == '\'' || ch == '`' {
            quote = Some(ch);
            continue;
        }
        if source[idx..].starts_with(needle) {
            return Some(idx);
        }
    }
    None
}

fn strip_arrow_parameter_annotations(line: &str) -> String {
    let Some(arrow) = line.find("=>") else {
        return line.to_string();
    };
    let Some(close_paren) = line[..arrow].rfind(')') else {
        return line.to_string();
    };
    let Some(open_paren) = matching_open_paren(&line[..=close_paren], close_paren) else {
        return line.to_string();
    };
    if !line[open_paren + 1..close_paren].contains(':') {
        return line.to_string();
    }

    format!(
        "{}{}{}",
        &line[..open_paren + 1],
        strip_colon_annotations(&line[open_paren + 1..close_paren]),
        &line[close_paren..]
    )
}

fn matching_open_paren(source: &str, close_paren: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in source[..=close_paren].char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_colon_annotations(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let chars: Vec<char> = source.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ':' && is_probable_type_annotation(&chars, i) {
            i += 1;
            let mut brace_depth = 0usize;
            while i < chars.len() {
                match chars[i] {
                    '{' | '<' => {
                        brace_depth += 1;
                        i += 1;
                    }
                    '}' | '>' if brace_depth > 0 => {
                        brace_depth -= 1;
                        i += 1;
                    }
                    ',' | ')' | '=' if brace_depth == 0 => break,
                    _ => i += 1,
                }
            }
            if i < chars.len() && chars[i] == '=' && !out.ends_with(' ') {
                out.push(' ');
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_probable_type_annotation(chars: &[char], colon_index: usize) -> bool {
    if colon_index == 0 {
        return false;
    }
    let mut i = colon_index;
    while i > 0 {
        i -= 1;
        if chars[i].is_whitespace() {
            continue;
        }
        return chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '}';
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::snapshot::TypeScriptImportPolicy;

    #[test]
    fn transpile_strips_basic_type_syntax() {
        let source = r#"
            import type { Chidori } from "chidori";
            type Input = { name: string };
            export async function agent(input: Input, chidori: Chidori): Promise<object> {
                const greeting: string = input.name;
                return { greeting };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/agent.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("import type"));
        assert!(!js.contains("type Input"));
        assert_eq!(source.lines().count(), js.lines().count());
        assert!(js.contains("export async function agent(input, chidori) {"));
        assert!(js.contains("const greeting = input.name;"));
    }

    #[test]
    fn transpile_strips_multiline_function_parameter_types() {
        let source = r#"
            import type { Chidori } from "chidori";
            export async function agent(
                input: { name?: string },
                chidori: Chidori,
            ) {
                const name = input.name ?? "world";
                return { greeting: name };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/agent.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("name?: string"));
        assert!(!js.contains("chidori: Chidori"));
        assert!(js.contains("input,"));
        assert!(js.contains("chidori,"));
        assert_eq!(source.lines().count(), js.lines().count());
    }

    #[test]
    fn transpile_strips_multiline_type_and_interface_declarations() {
        let source = r#"
            import type { Chidori } from "chidori";
            export interface Input {
                topic: string;
                limit?: number;
            }
            export type Result =
                | { ok: true; topic: string }
                | { ok: false; error: string };
            export async function agent(input: Input, chidori: Chidori): Promise<Result> {
                await chidori.log("topic", { topic: input.topic });
                return { ok: true, topic: input.topic };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/agent.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("interface Input"));
        assert!(!js.contains("export interface Input"));
        assert!(!js.contains("topic: string;"));
        assert!(!js.contains("type Result"));
        assert!(!js.contains("export type Result"));
        assert!(!js.contains("ok: true;"));
        assert!(js.contains("topic: input.topic"));
        assert!(js.contains("export async function agent(input, chidori) {"));
        assert_eq!(source.lines().count(), js.lines().count());
    }

    #[test]
    fn transpile_strips_arrow_function_parameter_types() {
        let source = r#"
            export async function agent(input, chidori) {
                const top5 = [];
                const pages = await chidori.parallel(
                    top5.map((result: { url: string; title: string }) => async () => {
                        return { url: result.url, title: result.title };
                    })
                );
                const links = pages.map(
                    (p: { url: string; title: string }, i: number) => ({
                        label: `${i}: ${p.title}`,
                        url: p.url,
                    })
                );
                return { links };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/agent.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("result: { url: string; title: string }"));
        assert!(!js.contains("p: { url: string; title: string }"));
        assert!(!js.contains("i: number"));
        assert!(js.contains("top5.map((result) => async () => {"));
        assert!(js.contains("(p, i) => ({"));
        assert!(js.contains("url: result.url"));
        assert!(js.contains("label: `${i}: ${p.title}`"));
    }

    #[test]
    fn transpile_preserves_object_literal_values() {
        let source = r#"
            import { Chidori, ToolDefinition } from "chidori";

            export const tool: ToolDefinition = {
                name: "web_search",
                parameters: { type: "object" },
            };
            export async function agent(input, chidori) {
                return { now: Date.now(), iso: new Date().toISOString() };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("from \"chidori\""));
        assert!(!js.contains(": ToolDefinition"));
        assert!(js.contains(r#"name: "web_search""#));
        assert!(js.contains(r#"type: "object""#));
        assert!(js.contains("now: Date.now()"));
        assert!(js.contains("iso: new Date().toISOString()"));
    }

    #[test]
    fn transpile_strips_satisfies_and_as_const_assertions() {
        let source = r#"
            import { ToolDefinition } from "chidori";

            const prompt = "Only call chidori.tool(name, args) directly for deterministic work when the exact args are already known and the tool is either implemented by this project or explicitly described by the user as an available runtime tool. ";
            const template = `Do not strip as const or satisfies ToolDefinition inside templates`;
            export const tool = {
                name: "web_search",
                description: "Search",
                parameters: {
                    type: "object",
                    required: ["query"] as const,
                },
            } as const satisfies ToolDefinition;

            const fallback = { ok: true } as { ok: boolean };
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("} as const satisfies ToolDefinition"));
        assert!(!js.contains(r#"["query"] as const"#));
        assert!(!js.contains(" as { ok: boolean }"));
        assert!(js.contains("};"));
        assert!(js.contains(r#"required: ["query"]"#));
        assert!(js.contains("as an available runtime tool"));
        assert!(js.contains("as const or satisfies ToolDefinition inside templates"));
    }

    #[test]
    fn transpile_strips_return_type_with_object_literal() {
        let source = r#"
            import { Chidori } from "chidori";

            export async function run(
                args: { url: string },
                chidori: Chidori,
            ): Promise<{ url: string; content: string }> {
                return { url: args.url, content: "ok" };
            }

            export async function inline(args: { url: string }, chidori: Chidori): { url: string; content: string } {
                return { url: args.url, content: "ok" };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/tools/read_url.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(js.contains("export async function run("));
        assert!(js.contains(") {"));
        assert!(js.contains("export async function inline(args, chidori) {"));
        assert!(!js.contains("Promise<{"));
        assert!(!js.contains("content: string"));
        assert!(js.contains("content: \"ok\""));
    }

    #[test]
    fn transpile_strips_multiline_as_type_assertion_and_catch_annotation() {
        let source = r#"
            export async function run(args, chidori) {
                const response = await chidori.http("https://example.test");
                let content: string;
                const data = response.body as {
                    web?: {
                        results?: Array<{
                            title: string;
                            url: string;
                        }>
                    }
                };
                try {
                    return { count: data?.web?.results?.length ?? 0 };
                } catch (error: any) {
                    return { error: error?.message ?? "failed" };
                }
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(js.contains("const data = response.body;"));
        assert!(js.contains("let content;"));
        assert!(!js.contains("content: string"));
        assert!(!js.contains("web?:"));
        assert!(!js.contains("title: string"));
        assert!(js.contains("catch (error)"));
        assert!(!js.contains("error: any"));
        assert!(js.contains("error: error?.message"));
    }

    #[test]
    fn transpile_preserves_colons_inside_initializers() {
        let source = r#"
            export async function run(args, chidori) {
                const response = await chidori.http("https://api.search.brave.com/res/v1/web/search", {
                    method: "GET",
                });
                const message = args.ok ? "ok" : String(args.error);
                return { response, message };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(js.contains(r#"chidori.http("https://api.search.brave.com/res/v1/web/search", {"#));
        assert!(js.contains(r#"const message = args.ok ? "ok" : String(args.error);"#));
    }

    #[test]
    fn relative_import_policy_rejects_bare_imports() {
        let source = r#"import { x } from "left-pad";"#;
        let err = validate_imports(
            Path::new("/tmp/project/agent.ts"),
            source,
            TypeScriptImportPolicy::Relative,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bare TypeScript imports"));
    }

    #[test]
    fn none_import_policy_rejects_relative_imports() {
        let source = r#"import { x } from "./x.ts";"#;
        let err = validate_imports(
            Path::new("/tmp/project/agent.ts"),
            source,
            TypeScriptImportPolicy::None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("disabled"));
        assert!(err.to_string().contains(":1:"));
    }

    #[test]
    fn dynamic_imports_are_rejected() {
        let source = r#"const x = await import("./x.ts");"#;
        let err = validate_imports(
            Path::new("/tmp/project/agent.ts"),
            source,
            TypeScriptImportPolicy::Project,
        )
        .unwrap_err();
        assert!(err.to_string().contains("dynamic import"));
        assert!(err.to_string().contains(":1:"));
    }
}
