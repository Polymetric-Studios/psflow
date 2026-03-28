use crate::execute::blackboard::Blackboard;
use crate::execute::Outputs;
use crate::graph::types::Value;
use std::fmt;

/// Error from template compilation or rendering.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateError {
    /// A `{variable}` placeholder has no closing brace.
    UnclosedPlaceholder { position: usize },
    /// A referenced variable was not found during rendering.
    MissingVariable { name: String },
    /// An `{#if}` block has no matching `{/if}`.
    UnclosedConditional { tag: String },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::UnclosedPlaceholder { position } => {
                write!(f, "unclosed placeholder at position {position}")
            }
            TemplateError::MissingVariable { name } => {
                write!(f, "missing variable: {name}")
            }
            TemplateError::UnclosedConditional { tag } => {
                write!(f, "unclosed conditional block: {tag}")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

/// A segment of a compiled template.
#[derive(Debug, Clone, PartialEq)]
enum Segment {
    /// Literal text, output as-is.
    Literal(String),
    /// Variable reference: `{inputs.key}` or `{ctx.key}`.
    Variable(String),
    /// Conditional block: `{#if var}...{/if}`.
    Conditional {
        var: String,
        body: Vec<Segment>,
    },
}

/// A compiled prompt template.
///
/// Templates use `{var}` for interpolation, where `var` is either:
/// - `inputs.key` — resolved from node inputs
/// - `ctx.key` — resolved from the blackboard (global scope)
/// - A bare name — tried as inputs first, then blackboard
///
/// Conditional blocks: `{#if var}...content...{/if}` — included only when
/// the variable exists and is truthy.
///
/// Compile at graph load time to catch errors early.
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    segments: Vec<Segment>,
}

impl PromptTemplate {
    /// Compile a template string into a `PromptTemplate`.
    ///
    /// Validates that all placeholders are properly closed.
    pub fn compile(template: &str) -> Result<Self, TemplateError> {
        let segments = parse_segments(template, 0)?;
        Ok(Self { segments })
    }

    /// Render the template with the given inputs and blackboard.
    pub fn render(
        &self,
        inputs: &Outputs,
        blackboard: &Blackboard,
    ) -> Result<String, TemplateError> {
        let mut buf = String::new();
        render_segments(&self.segments, inputs, blackboard, &mut buf)?;
        Ok(buf)
    }

    /// List all variable names referenced in this template.
    pub fn variables(&self) -> Vec<&str> {
        let mut vars = Vec::new();
        collect_variables(&self.segments, &mut vars);
        vars
    }
}

fn parse_segments(input: &str, base_offset: usize) -> Result<Vec<Segment>, TemplateError> {
    let mut segments = Vec::new();
    let mut pos = 0;
    let chars: Vec<char> = input.chars().collect();

    while pos < chars.len() {
        if chars[pos] == '{' {
            // Check for escaped brace
            if pos + 1 < chars.len() && chars[pos + 1] == '{' {
                segments.push(Segment::Literal("{".into()));
                pos += 2;
                continue;
            }

            // Find closing brace
            let start = pos + 1;
            let close = find_closing_brace(&chars, start).ok_or(
                TemplateError::UnclosedPlaceholder {
                    position: base_offset + pos,
                },
            )?;

            let content: String = chars[start..close].iter().collect();
            let content = content.trim();

            if let Some(var) = content.strip_prefix("#if ") {
                // Conditional block: find depth-aware matching {/if}
                let var = var.trim().to_string();
                let after_tag = close + 1;
                let remaining: String = chars[after_tag..].iter().collect();

                let end_pos = find_matching_endif(&remaining).ok_or(
                    TemplateError::UnclosedConditional {
                        tag: format!("{{#if {var}}}"),
                    },
                )?;

                let body_str = &remaining[..end_pos];
                let body = parse_segments(body_str, base_offset + after_tag)?;
                segments.push(Segment::Conditional { var, body });
                pos = after_tag + end_pos + "{/if}".len();
            } else if content == "/if" {
                // Stray {/if} — shouldn't happen in well-formed templates at top level
                // Just emit as literal
                segments.push(Segment::Literal("{/if}".into()));
                pos = close + 1;
            } else {
                // Variable reference
                segments.push(Segment::Variable(content.to_string()));
                pos = close + 1;
            }
        } else if chars[pos] == '}' && pos + 1 < chars.len() && chars[pos + 1] == '}' {
            // Escaped closing brace
            segments.push(Segment::Literal("}".into()));
            pos += 2;
        } else {
            // Accumulate literal text
            let start = pos;
            while pos < chars.len()
                && chars[pos] != '{'
                && !(chars[pos] == '}' && pos + 1 < chars.len() && chars[pos + 1] == '}')
            {
                pos += 1;
            }
            let text: String = chars[start..pos].iter().collect();
            segments.push(Segment::Literal(text));
        }
    }

    Ok(segments)
}

/// Find the position of the `{/if}` that closes the current nesting level.
/// Counts nested `{#if ...}` / `{/if}` pairs to handle depth correctly.
fn find_matching_endif(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    let mut pos = 0;
    while pos < s.len() {
        if s[pos..].starts_with("{#if ") {
            depth += 1;
            pos += 5; // skip past "{#if "
        } else if s[pos..].starts_with("{/if}") {
            depth -= 1;
            if depth == 0 {
                return Some(pos);
            }
            pos += 5; // skip past "{/if}"
        } else {
            pos += 1;
        }
    }
    None
}

fn find_closing_brace(chars: &[char], start: usize) -> Option<usize> {
    let mut pos = start;
    while pos < chars.len() {
        if chars[pos] == '}' {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

fn render_segments(
    segments: &[Segment],
    inputs: &Outputs,
    blackboard: &Blackboard,
    buf: &mut String,
) -> Result<(), TemplateError> {
    for seg in segments {
        match seg {
            Segment::Literal(text) => buf.push_str(text),
            Segment::Variable(name) => {
                let value = resolve_variable(name, inputs, blackboard)?;
                buf.push_str(&value);
            }
            Segment::Conditional { var, body } => {
                if let Ok(val) = resolve_variable(var, inputs, blackboard) {
                    if is_truthy(&val) {
                        render_segments(body, inputs, blackboard, buf)?;
                    }
                }
                // Variable not found or falsy — skip the block silently
            }
        }
    }
    Ok(())
}

fn resolve_variable(
    name: &str,
    inputs: &Outputs,
    blackboard: &Blackboard,
) -> Result<String, TemplateError> {
    if let Some(key) = name.strip_prefix("inputs.") {
        inputs
            .get(key)
            .map(value_to_string)
            .ok_or_else(|| TemplateError::MissingVariable {
                name: name.to_string(),
            })
    } else if let Some(key) = name.strip_prefix("ctx.") {
        use crate::execute::blackboard::BlackboardScope;
        blackboard
            .get(key, &BlackboardScope::Global)
            .map(value_to_string)
            .ok_or_else(|| TemplateError::MissingVariable {
                name: name.to_string(),
            })
    } else {
        // Bare name: try inputs first, then blackboard
        if let Some(val) = inputs.get(name) {
            Ok(value_to_string(val))
        } else {
            use crate::execute::blackboard::BlackboardScope;
            blackboard
                .get(name, &BlackboardScope::Global)
                .map(value_to_string)
                .ok_or_else(|| TemplateError::MissingVariable {
                    name: name.to_string(),
                })
        }
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::F32(n) => n.to_string(),
        Value::Null => String::new(),
        // Complex types serialize as JSON for predictable, LLM-parseable output
        other => serde_json::to_string(other).unwrap_or_else(|_| format!("{other:?}")),
    }
}

fn is_truthy(val: &str) -> bool {
    !val.is_empty() && val != "false" && val != "0"
}

fn collect_variables<'a>(segments: &'a [Segment], vars: &mut Vec<&'a str>) {
    for seg in segments {
        match seg {
            Segment::Variable(name) => vars.push(name),
            Segment::Conditional { var, body } => {
                vars.push(var);
                collect_variables(body, vars);
            }
            Segment::Literal(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::blackboard::BlackboardScope;

    fn inputs_with(pairs: &[(&str, &str)]) -> Outputs {
        let mut out = Outputs::new();
        for (k, v) in pairs {
            out.insert((*k).into(), Value::String((*v).into()));
        }
        out
    }

    #[test]
    fn literal_only() {
        let tpl = PromptTemplate::compile("Hello, world!").unwrap();
        let result = tpl.render(&Outputs::new(), &Blackboard::new()).unwrap();
        assert_eq!(result, "Hello, world!");
    }

    #[test]
    fn variable_interpolation() {
        let tpl = PromptTemplate::compile("Hello, {inputs.name}!").unwrap();
        let inputs = inputs_with(&[("name", "Alice")]);
        let result = tpl.render(&inputs, &Blackboard::new()).unwrap();
        assert_eq!(result, "Hello, Alice!");
    }

    #[test]
    fn ctx_variable() {
        let tpl = PromptTemplate::compile("Mode: {ctx.mode}").unwrap();
        let mut bb = Blackboard::new();
        bb.set(
            "mode".into(),
            Value::String("fast".into()),
            BlackboardScope::Global,
        );
        let result = tpl.render(&Outputs::new(), &bb).unwrap();
        assert_eq!(result, "Mode: fast");
    }

    #[test]
    fn bare_variable_tries_inputs_first() {
        let tpl = PromptTemplate::compile("Value: {x}").unwrap();
        let inputs = inputs_with(&[("x", "from_inputs")]);
        let mut bb = Blackboard::new();
        bb.set(
            "x".into(),
            Value::String("from_bb".into()),
            BlackboardScope::Global,
        );
        let result = tpl.render(&inputs, &bb).unwrap();
        assert_eq!(result, "Value: from_inputs");
    }

    #[test]
    fn bare_variable_falls_back_to_blackboard() {
        let tpl = PromptTemplate::compile("Value: {x}").unwrap();
        let mut bb = Blackboard::new();
        bb.set(
            "x".into(),
            Value::String("from_bb".into()),
            BlackboardScope::Global,
        );
        let result = tpl.render(&Outputs::new(), &bb).unwrap();
        assert_eq!(result, "Value: from_bb");
    }

    #[test]
    fn missing_variable_is_error() {
        let tpl = PromptTemplate::compile("Hello, {inputs.missing}!").unwrap();
        let err = tpl.render(&Outputs::new(), &Blackboard::new()).unwrap_err();
        assert!(matches!(err, TemplateError::MissingVariable { .. }));
    }

    #[test]
    fn multiple_variables() {
        let tpl = PromptTemplate::compile("{inputs.a} and {inputs.b}").unwrap();
        let inputs = inputs_with(&[("a", "X"), ("b", "Y")]);
        let result = tpl.render(&inputs, &Blackboard::new()).unwrap();
        assert_eq!(result, "X and Y");
    }

    #[test]
    fn conditional_block_truthy() {
        let tpl =
            PromptTemplate::compile("Start{#if inputs.flag} INCLUDED{/if} End").unwrap();
        let inputs = inputs_with(&[("flag", "yes")]);
        let result = tpl.render(&inputs, &Blackboard::new()).unwrap();
        assert_eq!(result, "Start INCLUDED End");
    }

    #[test]
    fn conditional_block_falsy() {
        let tpl =
            PromptTemplate::compile("Start{#if inputs.flag} INCLUDED{/if} End").unwrap();
        let inputs = inputs_with(&[("flag", "false")]);
        let result = tpl.render(&inputs, &Blackboard::new()).unwrap();
        assert_eq!(result, "Start End");
    }

    #[test]
    fn conditional_block_missing_var() {
        let tpl =
            PromptTemplate::compile("Start{#if inputs.missing} INCLUDED{/if} End").unwrap();
        let result = tpl
            .render(&Outputs::new(), &Blackboard::new())
            .unwrap();
        assert_eq!(result, "Start End");
    }

    #[test]
    fn nested_conditionals() {
        let tpl = PromptTemplate::compile(
            "Start{#if inputs.a} A{#if inputs.b} AB{/if} after-B{/if} End",
        )
        .unwrap();

        // Both truthy
        let inputs = inputs_with(&[("a", "yes"), ("b", "yes")]);
        let result = tpl.render(&inputs, &Blackboard::new()).unwrap();
        assert_eq!(result, "Start A AB after-B End");

        // Outer truthy, inner falsy
        let inputs = inputs_with(&[("a", "yes"), ("b", "false")]);
        let result = tpl.render(&inputs, &Blackboard::new()).unwrap();
        assert_eq!(result, "Start A after-B End");

        // Outer falsy
        let inputs = inputs_with(&[("a", "false"), ("b", "yes")]);
        let result = tpl.render(&inputs, &Blackboard::new()).unwrap();
        assert_eq!(result, "Start End");
    }

    #[test]
    fn escaped_braces() {
        let tpl = PromptTemplate::compile("JSON: {{\"key\": \"val\"}}").unwrap();
        let result = tpl.render(&Outputs::new(), &Blackboard::new()).unwrap();
        assert_eq!(result, "JSON: {\"key\": \"val\"}");
    }

    #[test]
    fn unclosed_placeholder_is_error() {
        let err = PromptTemplate::compile("Hello {name").unwrap_err();
        assert!(matches!(err, TemplateError::UnclosedPlaceholder { .. }));
    }

    #[test]
    fn unclosed_conditional_is_error() {
        let err = PromptTemplate::compile("{#if x}body").unwrap_err();
        assert!(matches!(err, TemplateError::UnclosedConditional { .. }));
    }

    #[test]
    fn variables_list() {
        let tpl = PromptTemplate::compile("{inputs.a} {ctx.b} {#if inputs.c}inner{/if}")
            .unwrap();
        let vars = tpl.variables();
        assert_eq!(vars, vec!["inputs.a", "ctx.b", "inputs.c"]);
    }

    #[test]
    fn non_string_values() {
        let tpl = PromptTemplate::compile("Count: {inputs.n}, Active: {inputs.flag}").unwrap();
        let mut inputs = Outputs::new();
        inputs.insert("n".into(), Value::I64(42));
        inputs.insert("flag".into(), Value::Bool(true));
        let result = tpl.render(&inputs, &Blackboard::new()).unwrap();
        assert_eq!(result, "Count: 42, Active: true");
    }
}
