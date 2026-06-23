//! Skill lifecycle: Markdown + frontmatter templates, progressive disclosure,
//! and the `load_skill` built-in tool. See `doc/skill.md`.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

use crate::core::payload::{Content, ToolOutput};
use crate::tool::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult};

/// Name + description extracted from a skill file (used for index injection).
pub struct SkillMeta {
    pub name: String,
    pub description: String,
}

/// Reads skill files from a directory.
pub struct SkillStore {
    dir: PathBuf,
}

impl SkillStore {
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// List available skills. `filter` restricts to named skills; empty = all.
    /// Files starting with `_` are skipped (e.g. `_disabled` markers).
    #[must_use]
    pub fn list(&self, filter: &[String]) -> Vec<SkillMeta> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut skills: Vec<SkillMeta> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                (path.extension()? == "md").then_some(())?;
                let stem = path.file_stem()?.to_string_lossy();
                if stem.starts_with('_') {
                    return None;
                }
                if !filter.is_empty() && !filter.iter().any(|f| f == stem.as_ref()) {
                    return None;
                }
                let content = std::fs::read_to_string(&path).ok()?;
                let name = fm_field(&content, "name").unwrap_or_else(|| stem.into_owned());
                let description = fm_field(&content, "description").unwrap_or_default();
                Some(SkillMeta { name, description })
            })
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(format!("{name}.md"))).ok()
    }
}

/// The system-prompt skill index section listing name + description.
#[must_use]
pub fn skill_index_block(skills: &[SkillMeta]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\n## Available Skills\n\n");
    for m in skills {
        let _ = writeln!(s, "- {}: {}", m.name, m.description);
    }
    s.push_str("\nUse `load_skill` when your task matches a known skill.\n");
    s
}

/// Extract a scalar YAML frontmatter field (between `---` delimiters).
fn fm_field(content: &str, field: &str) -> Option<String> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let prefix = format!("{field}:");
    rest[..end].lines().find_map(|l| {
        l.strip_prefix(&prefix)
            .map(|v| v.trim().trim_matches('"').to_owned())
    })
}

/// Return the body portion of a skill file (after the frontmatter block).
fn skill_body(content: &str) -> &str {
    content
        .strip_prefix("---\n")
        .and_then(|r| r.find("\n---\n").map(|i| &r[i + 5..]))
        .unwrap_or(content)
}

struct TemplateCtx {
    workspace: PathBuf,
    profile: String,
}

/// Expand `{{...}}` template variables. Runs every template, collecting errors
/// instead of failing fast (`doc/skill.md` §4.1). Returns (rendered, errors).
async fn render(content: &str, ctx: &TemplateCtx) -> (String, Vec<String>) {
    let mut result = String::with_capacity(content.len());
    let mut errors = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        let Some(end) = rest.find("}}") else {
            result.push_str("{{");
            continue;
        };
        let inner = rest[..end].trim();
        rest = &rest[end + 2..];
        result.push_str(&expand(inner, ctx, &mut errors).await);
    }
    result.push_str(rest);
    (result, errors)
}

async fn expand(inner: &str, ctx: &TemplateCtx, errors: &mut Vec<String>) -> String {
    match inner {
        "now" => chrono::Utc::now().to_rfc3339(),
        "workspace" => ctx.workspace.display().to_string(),
        "profile" => ctx.profile.clone(),
        // session_id is unknown at tool-construction time; left empty until a
        // per-invocation context exists.
        "session_id" => String::new(),
        _ if inner.starts_with("exec ") => {
            exec_cmd(inner[5..].trim().trim_matches('"'), errors).await
        }
        _ if inner.starts_with("env ") => {
            let var = inner[4..].trim().trim_matches('"');
            std::env::var(var).unwrap_or_default()
        }
        // Unknown template: leave the literal in place for visibility.
        _ => format!("{{{{{inner}}}}}"),
    }
}

async fn exec_cmd(cmd: &str, errors: &mut Vec<String>) -> String {
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output(),
    )
    .await;
    match result {
        Ok(Ok(out)) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
        }
        Ok(Ok(out)) => {
            let code = out.status.code().unwrap_or(-1);
            errors.push(format!(
                "`{cmd}`: exit {code}, {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            format!("[FAILED: {cmd}]")
        }
        Ok(Err(e)) => {
            errors.push(format!("`{cmd}`: {e}"));
            format!("[FAILED: {cmd}]")
        }
        Err(_) => {
            errors.push(format!("`{cmd}`: timeout after 5s"));
            format!("[TIMEOUT: {cmd}]")
        }
    }
}

/// The `load_skill` built-in tool: load and render a named skill file.
pub struct LoadSkillTool {
    store: SkillStore,
    ctx: TemplateCtx,
}

impl LoadSkillTool {
    #[must_use]
    pub const fn new(store: SkillStore, workspace: PathBuf, profile: String) -> Self {
        Self {
            store,
            ctx: TemplateCtx { workspace, profile },
        }
    }
}

#[async_trait::async_trait]
impl Tool for LoadSkillTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "load_skill".to_owned(),
            description: "Load the full instructions for a named skill. Call this when the task matches an available skill.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill name from the available skills index."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let name = input
            .input
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing `name`".to_owned()))?;

        let Some(content) = self.store.read(name) else {
            return Ok(ToolOutput {
                content: vec![Content::Text(format!("skill `{name}` not found"))],
                is_error: true,
                error_code: Some("skill_not_found".to_owned()),
            });
        };

        let (mut rendered, errors) = render(skill_body(&content), &self.ctx).await;
        if !errors.is_empty() {
            let _ = write!(
                rendered,
                "\n\n---\n{} template execution(s) failed:\n",
                errors.len()
            );
            for e in &errors {
                let _ = writeln!(rendered, "- {e}");
            }
        }
        Ok(ToolOutput {
            content: vec![Content::Text(rendered)],
            is_error: false,
            error_code: None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn make_input(name: &str) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "name": name }),
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn fm_field_parses_quoted_and_bare() {
        let content = "---\nname: \"git-commit\"\ndescription: A skill\n---\nbody";
        assert_eq!(fm_field(content, "name").as_deref(), Some("git-commit"));
        assert_eq!(fm_field(content, "description").as_deref(), Some("A skill"));
    }

    #[test]
    fn skill_body_strips_frontmatter() {
        let content = "---\nname: \"x\"\n---\nmy body";
        assert_eq!(skill_body(content), "my body");
    }

    #[test]
    fn skill_body_passthrough_no_frontmatter() {
        assert_eq!(skill_body("just body"), "just body");
    }

    #[test]
    fn skill_index_block_lists_skills() {
        let skills = vec![
            SkillMeta {
                name: "a".to_owned(),
                description: "first".to_owned(),
            },
            SkillMeta {
                name: "b".to_owned(),
                description: "second".to_owned(),
            },
        ];
        let block = skill_index_block(&skills);
        assert!(block.contains("- a: first"));
        assert!(block.contains("- b: second"));
        assert!(block.contains("load_skill"));
    }

    #[test]
    fn skill_index_block_empty_is_blank() {
        assert_eq!(skill_index_block(&[]), "");
    }

    #[test]
    fn list_filters_and_skips_underscore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("one.md"),
            "---\nname: \"one\"\ndescription: \"d1\"\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("two.md"),
            "---\nname: \"two\"\ndescription: \"d2\"\n---\nbody",
        )
        .unwrap();
        std::fs::write(dir.path().join("_hidden.md"), "---\nname: \"h\"\n---\nx").unwrap();
        let store = SkillStore::new(dir.path().to_path_buf());

        let all = store.list(&[]);
        assert_eq!(all.len(), 2, "underscore-prefixed skipped");

        let filtered = store.list(&["one".to_owned()]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "one");
    }

    #[tokio::test]
    async fn load_skill_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::new(dir.path().to_path_buf());
        let tool = LoadSkillTool::new(store, dir.path().to_path_buf(), "default".to_owned());
        let out = tool.invoke(make_input("missing")).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("skill_not_found"));
    }

    #[tokio::test]
    async fn load_skill_renders_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("greet.md"),
            "---\nname: \"greet\"\ndescription: \"says hi\"\n---\nHello from {{workspace}}",
        )
        .unwrap();
        let store = SkillStore::new(dir.path().to_path_buf());
        let tool = LoadSkillTool::new(store, dir.path().to_path_buf(), "default".to_owned());
        let out = tool.invoke(make_input("greet")).await.unwrap();
        assert!(!out.is_error);
        let Content::Text(text) = &out.content[0] else {
            panic!("expected text");
        };
        assert!(text.contains("Hello from"));
        assert!(text.contains(dir.path().to_str().unwrap()));
    }

    #[tokio::test]
    async fn render_exec_failure_appended_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bad.md"),
            "---\nname: \"bad\"\ndescription: \"x\"\n---\nout: {{exec \"exit 3\"}}",
        )
        .unwrap();
        let store = SkillStore::new(dir.path().to_path_buf());
        let tool = LoadSkillTool::new(store, dir.path().to_path_buf(), "default".to_owned());
        let out = tool.invoke(make_input("bad")).await.unwrap();
        assert!(
            !out.is_error,
            "partial template failure is not a tool error"
        );
        let Content::Text(text) = &out.content[0] else {
            panic!("expected text");
        };
        assert!(text.contains("[FAILED: exit 3]"));
        assert!(text.contains("template execution(s) failed"));
    }
}
