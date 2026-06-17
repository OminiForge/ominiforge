//! The profile data model: agent identity and capability composition.
//!
//! Mirrors `doc/profile.md` §3. A profile names a system prompt, a model
//! reference (`provider_name/model_id`), and a tool set. Connection and pricing
//! details belong to [`super::providers`], not here.
//!
//! Phase 1 wires `[prompt]`, `[model]`, and `[tools]`. The `[context]`,
//! `[skills]`, `[memory]`, `[budget]`, and `[hooks]` sections parse (so a
//! complete config file loads) but are not yet acted on. Loading by name and
//! resolving the `extends` chain live in [`super`]; this module owns the shape,
//! the hardcoded default, and the field-level overlay rule (§4).

use serde::{Deserialize, Serialize};

/// A parsed profile. Optional sections default so partial files load; unknown
/// keys are ignored for forward compatibility.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub profile: ProfileMeta,

    #[serde(default)]
    pub prompt: PromptSection,

    #[serde(default)]
    pub model: ModelSection,

    #[serde(default)]
    pub tools: ToolsSection,

    // Parsed but not yet wired (Phase 2/3). Kept so full config files load.
    #[serde(default)]
    pub context: ContextSection,
    #[serde(default)]
    pub skills: SkillsSection,
    #[serde(default)]
    pub memory: MemorySection,
    #[serde(default)]
    pub budget: BudgetSection,
    #[serde(default)]
    pub hooks: HooksSection,
}

/// `[profile]`: identity and inheritance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileMeta {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Single-inheritance parent profile name (§4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
}

/// `[prompt]`: the system prompt, inline or from a file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Path (relative to the profile file) to read the system prompt from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_file: Option<String>,
}

/// `[model]`: which model to use and parameter overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelSection {
    /// Default model reference, `provider_name/model_id` or short `model_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Fallback model reference (parsed; automatic fallback is a later phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    /// Overrides the model's `default_temperature`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Overrides the model's `max_output_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// `[tools]`: which built-in tools and MCP servers are available.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsSection {
    /// Built-in tools to enable. `None` means "all registered built-ins".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<Vec<String>>,
    /// MCP server names to attach (parsed; MCP client is Phase 2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<String>,
    /// Tools to disable even if otherwise enabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled: Vec<String>,
}

impl ToolsSection {
    /// Whether tool `name` is enabled: present in `builtin` (or `builtin`
    /// unset = all) and not in `disabled`.
    #[must_use]
    pub fn allows(&self, name: &str) -> bool {
        if self.disabled.iter().any(|d| d == name) {
            return false;
        }
        self.builtin
            .as_ref()
            .is_none_or(|list| list.iter().any(|b| b == name))
    }
}

/// `[context]`: compaction and injection knobs (parsed; not yet wired).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injection_max_tokens: Option<u32>,
}

/// `[skills]` (parsed; not yet wired).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsSection {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled: Vec<String>,
}

/// `[memory]` (parsed; not yet wired).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySection {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_write: Option<bool>,
}

/// `[budget]` (parsed; not yet wired).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BudgetSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_max_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_max_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_at_percent: Option<u32>,
}

/// `[hooks]` (parsed; not yet wired).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HooksSection {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before_tool: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after_tool: Vec<String>,
}

impl Profile {
    /// The built-in default profile used when no profile file exists and none
    /// is inherited (`doc/profile.md` §4: "无 `extends` 时使用硬编码默认值").
    ///
    /// It has no model — that must come from a profile file or `--model`, since
    /// the right model depends on the user's configured providers.
    #[must_use]
    pub fn builtin_default() -> Self {
        Self {
            profile: ProfileMeta {
                name: "default".to_owned(),
                description: Some("Built-in default agent profile".to_owned()),
                extends: None,
            },
            prompt: PromptSection {
                system: Some(DEFAULT_SYSTEM_PROMPT.to_owned()),
                system_file: None,
            },
            model: ModelSection::default(),
            tools: ToolsSection::default(),
            context: ContextSection::default(),
            skills: SkillsSection::default(),
            memory: MemorySection::default(),
            budget: BudgetSection::default(),
            hooks: HooksSection::default(),
        }
    }

    /// Overlay `self` (the child) onto `parent`, returning the merged profile.
    ///
    /// Field-level override per §4: any field *present* in the child fully
    /// replaces the parent's (no list merging); absent child fields inherit the
    /// parent value. `profile.name`/`description`/`extends` always come from the
    /// child (it is the concrete profile being resolved).
    #[must_use]
    pub fn overlay_onto(self, parent: Self) -> Self {
        Self {
            profile: self.profile,
            prompt: overlay_prompt(self.prompt, parent.prompt),
            model: overlay_model(self.model, parent.model),
            tools: overlay_tools(self.tools, parent.tools),
            context: overlay_context(&self.context, &parent.context),
            // Sections not yet wired: child wins if non-empty, else parent.
            skills: pick_skills(self.skills, parent.skills),
            memory: pick_memory(self.memory, parent.memory),
            budget: pick_budget(self.budget, parent.budget),
            hooks: pick_hooks(self.hooks, parent.hooks),
        }
    }
}

/// The fallback system prompt baked into the default profile, also used when a
/// loaded profile specifies no prompt.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are Ominiforge, a capable software agent. Use the available tools to \
     accomplish the user's task, and explain what you did.";

fn overlay_prompt(child: PromptSection, parent: PromptSection) -> PromptSection {
    // The prompt is one logical field (system XOR system_file): if the child
    // specifies either, it replaces the parent's wholesale.
    if child.system.is_some() || child.system_file.is_some() {
        child
    } else {
        parent
    }
}

fn overlay_model(child: ModelSection, parent: ModelSection) -> ModelSection {
    ModelSection {
        default: child.default.or(parent.default),
        fallback: child.fallback.or(parent.fallback),
        temperature: child.temperature.or(parent.temperature),
        max_output_tokens: child.max_output_tokens.or(parent.max_output_tokens),
    }
}

fn overlay_tools(child: ToolsSection, parent: ToolsSection) -> ToolsSection {
    ToolsSection {
        builtin: child.builtin.or(parent.builtin),
        mcp_servers: if child.mcp_servers.is_empty() {
            parent.mcp_servers
        } else {
            child.mcp_servers
        },
        disabled: if child.disabled.is_empty() {
            parent.disabled
        } else {
            child.disabled
        },
    }
}

fn overlay_context(child: &ContextSection, parent: &ContextSection) -> ContextSection {
    ContextSection {
        compaction_threshold: child.compaction_threshold.or(parent.compaction_threshold),
        injection_max_tokens: child.injection_max_tokens.or(parent.injection_max_tokens),
    }
}

fn pick_skills(child: SkillsSection, parent: SkillsSection) -> SkillsSection {
    if child.enabled.is_empty() {
        parent
    } else {
        child
    }
}

fn pick_memory(child: MemorySection, parent: MemorySection) -> MemorySection {
    if child.scopes.is_empty() && child.auto_write.is_none() {
        parent
    } else {
        child
    }
}

fn pick_budget(child: BudgetSection, parent: BudgetSection) -> BudgetSection {
    if child == BudgetSection::default() {
        parent
    } else {
        child
    }
}

fn pick_hooks(child: HooksSection, parent: HooksSection) -> HooksSection {
    if child.before_tool.is_empty() && child.after_tool.is_empty() {
        parent
    } else {
        child
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parses_doc_example_profile() {
        let toml_src = r#"
[profile]
name = "coding"
description = "Software development agent"
extends = "base"

[prompt]
system = "You write clean, tested code."

[model]
default = "openai-main/gpt-4o"
fallback = "openai-main/gpt-4o-mini"
temperature = 0.0
max_output_tokens = 16384

[context]
compaction_threshold = 0.8
injection_max_tokens = 4096

[tools]
builtin = ["read", "write", "shell"]
mcp_servers = ["github"]
disabled = []

[skills]
enabled = ["git-commit"]

[memory]
scopes = ["user", "project"]
auto_write = true

[budget]
session_max_usd = 10.00
daily_max_usd = 50.00
warn_at_percent = 80

[hooks]
before_tool = ["security-guard"]
"#;
        let p: Profile = toml::from_str(toml_src).unwrap();
        assert_eq!(p.profile.name, "coding");
        assert_eq!(p.profile.extends.as_deref(), Some("base"));
        assert_eq!(p.model.default.as_deref(), Some("openai-main/gpt-4o"));
        assert_eq!(
            p.tools.builtin.as_deref().unwrap(),
            ["read", "write", "shell"]
        );
        assert_eq!(p.budget.session_max_usd, Some(10.00));
    }

    #[test]
    fn minimal_profile_loads_with_defaults() {
        let toml_src = r#"
[profile]
name = "tiny"
"#;
        let p: Profile = toml::from_str(toml_src).unwrap();
        assert_eq!(p.profile.name, "tiny");
        assert!(p.model.default.is_none());
        assert!(p.tools.builtin.is_none());
        // builtin unset → every tool allowed.
        assert!(p.tools.allows("shell"));
    }

    #[test]
    fn unknown_keys_are_ignored_for_forward_compat() {
        let toml_src = r#"
[profile]
name = "future"

[some_future_section]
whatever = true
"#;
        let p: Profile = toml::from_str(toml_src).unwrap();
        assert_eq!(p.profile.name, "future");
    }

    #[test]
    fn tools_allows_respects_disabled_and_builtin_list() {
        let tools = ToolsSection {
            builtin: Some(vec!["read".to_owned(), "shell".to_owned()]),
            mcp_servers: vec![],
            disabled: vec!["shell".to_owned()],
        };
        assert!(tools.allows("read"));
        assert!(!tools.allows("shell")); // disabled wins
        assert!(!tools.allows("write")); // not in builtin list
    }

    #[test]
    fn overlay_child_fields_win_absent_inherit() {
        let parent: Profile = toml::from_str(
            r#"
[profile]
name = "base"
[prompt]
system = "base prompt"
[model]
default = "openai-main/gpt-4o"
temperature = 0.0
[tools]
builtin = ["read", "write", "shell"]
"#,
        )
        .unwrap();

        let child: Profile = toml::from_str(
            r#"
[profile]
name = "coding"
extends = "base"
[model]
temperature = 0.7
[tools]
builtin = ["read", "write", "shell", "lsp"]
"#,
        )
        .unwrap();

        let merged = child.overlay_onto(parent);
        assert_eq!(merged.profile.name, "coding");
        // prompt absent in child → inherited.
        assert_eq!(merged.prompt.system.as_deref(), Some("base prompt"));
        // model.default absent in child → inherited; temperature overridden.
        assert_eq!(merged.model.default.as_deref(), Some("openai-main/gpt-4o"));
        assert_eq!(merged.model.temperature, Some(0.7));
        // tools.builtin fully replaced (no merge).
        assert_eq!(
            merged.tools.builtin.as_deref().unwrap(),
            ["read", "write", "shell", "lsp"]
        );
    }
}
