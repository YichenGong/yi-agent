//! 发布清单解析。
//!
//! 为什么不用 `jsonschema` crate 直接校验：契约的权威定义是
//! `plugins/contracts/*.schema.json`，但把它在运行期加载意味着发布包里
//! 要带一份 schema 文件与一个完整的 JSON Schema 实现。清单是静态声明，
//! 字段集合固定，所以这里用强类型 + 显式语义检查，并在测试里与 JSON
//! Schema 检查器共用同一份 fixture —— 两份实现互相印证，比单靠一个
//! 通用校验器更容易给出能指向具体字段的错误信息。

use serde::{Deserialize, Serialize};

use crate::error::PluginError;
use crate::types::{Effect, PluginId};

/// 清单里的 methods[] 条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MethodDecl {
    pub name: String,
    pub effect: Effect,
}

/// 清单里的 views[] 条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewDecl {
    pub id: String,
    pub kind: ViewKind,
    pub entry: String,
    pub digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewKind {
    Main,
    Settings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramEntry {
    pub entry: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEntry {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliEntry {
    pub verb: String,
    pub effect: Effect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub required: Vec<Effect>,
    pub optional: Vec<Effect>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDep {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependencies {
    pub workflows: Vec<WorkflowDep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    pub from: u32,
    pub to: u32,
    pub entry: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataDecl {
    pub format: u32,
    pub migrations: Vec<Migration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Compatibility {
    pub host_api: String,
    pub ui_api: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    pub package_format: u32,
    pub plugin: PluginId,
    pub version: String,
    pub target: String,
    pub views: Vec<ViewDecl>,
    pub program: Option<ProgramEntry>,
    pub skills: Vec<SkillEntry>,
    pub methods: Vec<MethodDecl>,
    pub cli: Vec<CliEntry>,
    pub capabilities: Capabilities,
    pub dependencies: Dependencies,
    pub data: DataDecl,
    pub compatibility: Compatibility,
}

/// 声明为 Read 却命名破坏性动词的方法必须拒绝。
///
/// 这条无法由字段校验表达：名字符合 `namespace/verb` 正则，effect 也合法，
/// 只有把两者放在一起看才错。与 `scripts/plugins/check-contracts.mjs` 的
/// `DESTRUCTIVE_VERBS` 保持一致——两处都要有，因为插件可能绕过其一。
const DESTRUCTIVE_VERBS: &[&str] = &[
    "delete",
    "remove",
    "purge",
    "destroy",
    "drop",
    "erase",
    "wipe",
    "clear",
    "reset",
    "truncate",
    "revoke",
    "uninstall",
];

/// 包内相对路径：拒绝绝对路径、任何 `..` 段与反斜杠。
///
/// 反斜杠必须在清单层拒绝：macOS/Linux 把它当普通文件名字符，所以 OS 不会拦，
/// 而同一份包在 Windows 上会把它解释成分隔符——那就成了两个平台指向不同文件。
fn validate_package_path(value: &str, field: &str) -> Result<(), PluginError> {
    if value.is_empty() || value.len() > 512 {
        return Err(PluginError::InvalidManifest(format!(
            "{field}: package path must be 1..=512 bytes, got {}",
            value.len()
        )));
    }
    if value.starts_with('/') {
        return Err(PluginError::InvalidManifest(format!(
            "{field}: package path must be relative, got an absolute path {value:?}"
        )));
    }
    if value.contains('\\') {
        return Err(PluginError::InvalidManifest(format!(
            "{field}: package path must not contain a backslash, got {value:?}"
        )));
    }
    if value.contains('\u{0}') {
        return Err(PluginError::InvalidManifest(format!(
            "{field}: package path must not contain a NUL byte"
        )));
    }
    if value.split('/').any(|segment| segment == "..") {
        return Err(PluginError::InvalidManifest(format!(
            "{field}: package path must not contain a `..` segment, got {value:?}"
        )));
    }
    Ok(())
}

fn validate_version(value: &str) -> Result<(), PluginError> {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() != 3 {
        return Err(PluginError::InvalidManifest(format!(
            "version must be `major.minor.patch`, got {value:?}"
        )));
    }
    for part in parts {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return Err(PluginError::InvalidManifest(format!(
                "version must be plain digits separated by dots, got {value:?}"
            )));
        }
        // 禁止前导零：`01` 与 `1` 是两个字符串但同一个版本，会让
        // 「同版本不同摘要 → 冲突」这条判断失效。
        if part.len() > 1 && part.starts_with('0') {
            return Err(PluginError::InvalidManifest(format!(
                "version components must not have leading zeros, got {value:?}"
            )));
        }
    }
    Ok(())
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, PluginError> {
        let manifest: Manifest = serde_json::from_slice(bytes)
            .map_err(|error| PluginError::InvalidManifest(error.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// 语义校验。字段级约束由 serde（`deny_unknown_fields` + 类型）覆盖；
    /// 这里处理「组合起来才错」的规则。
    fn validate(&self) -> Result<(), PluginError> {
        if self.schema_version != 1 {
            return Err(PluginError::InvalidManifest(format!(
                "unsupported schemaVersion {}, this host understands 1",
                self.schema_version
            )));
        }
        if self.package_format != 1 {
            return Err(PluginError::InvalidManifest(format!(
                "unsupported packageFormat {}, this host understands 1",
                self.package_format
            )));
        }
        validate_version(&self.version)?;

        if !matches!(
            self.target.as_str(),
            "darwin-aarch64"
                | "darwin-x86_64"
                | "linux-aarch64"
                | "linux-x86_64"
                | "win32-aarch64"
                | "win32-x86_64"
        ) {
            return Err(PluginError::InvalidManifest(format!(
                "unsupported target {:?}",
                self.target
            )));
        }

        // --- views：路径、id 唯一、恰好一个 main ---
        let mut seen_view_ids = std::collections::BTreeSet::new();
        for (i, view) in self.views.iter().enumerate() {
            validate_package_path(&view.entry, &format!("views[{i}].entry"))?;
            if !is_valid_view_id(&view.id) {
                return Err(PluginError::InvalidManifest(format!(
                    "views[{i}].id {:?} is not a valid view id",
                    view.id
                )));
            }
            if !seen_view_ids.insert(view.id.as_str()) {
                return Err(PluginError::InvalidManifest(format!(
                    "views[{i}].id {:?} is declared twice; view ids must be unique",
                    view.id
                )));
            }
            if !is_sha256(&view.digest) {
                return Err(PluginError::InvalidManifest(format!(
                    "views[{i}].digest must be 64 lowercase hex characters"
                )));
            }
        }
        let mains = self
            .views
            .iter()
            .filter(|v| v.kind == ViewKind::Main)
            .count();
        if mains != 1 {
            return Err(PluginError::InvalidManifest(format!(
                "exactly one view must have kind `main`, found {mains}"
            )));
        }

        if let Some(program) = &self.program {
            validate_package_path(&program.entry, "program.entry")?;
            if !is_sha256(&program.digest) {
                return Err(PluginError::InvalidManifest(
                    "program.digest must be 64 lowercase hex characters".to_string(),
                ));
            }
        }

        // --- skills ---
        let mut seen_skills = std::collections::BTreeSet::new();
        for (i, skill) in self.skills.iter().enumerate() {
            validate_package_path(&skill.path, &format!("skills[{i}].path"))?;
            if !seen_skills.insert(skill.name.as_str()) {
                return Err(PluginError::InvalidManifest(format!(
                    "skills[{i}].name {:?} is declared twice; skill names must be unique",
                    skill.name
                )));
            }
        }

        // --- methods：名字形状、唯一、Maintenance 不可声明、Read 不得命名破坏性动词 ---
        if self.methods.is_empty() {
            return Err(PluginError::InvalidManifest(
                "a manifest must declare at least one method".to_string(),
            ));
        }
        let mut seen_methods = std::collections::BTreeSet::new();
        for method in &self.methods {
            if !is_namespaced_name(&method.name) {
                return Err(PluginError::InvalidManifest(format!(
                    "method {:?} must be `namespace/verb` with lowercase namespaces",
                    method.name
                )));
            }
            if !seen_methods.insert(method.name.as_str()) {
                return Err(PluginError::InvalidManifest(format!(
                    "method {:?} is declared twice; method names must be unique",
                    method.name
                )));
            }
            reject_maintenance(method.effect, &format!("method {:?}", method.name))?;
            if method.effect == Effect::Read {
                let lowered = method.name.to_lowercase();
                if let Some(hit) = DESTRUCTIVE_VERBS
                    .iter()
                    .find(|verb| lowered.contains(*verb))
                {
                    return Err(PluginError::InvalidManifest(format!(
                        "method {:?} declares effect Read but its name contains the destructive verb {:?}",
                        method.name, hit
                    )));
                }
            }
        }

        // --- cli ---
        let mut seen_cli = std::collections::BTreeSet::new();
        for entry in &self.cli {
            if !is_cli_verb(&entry.verb) {
                return Err(PluginError::InvalidManifest(format!(
                    "cli verb {:?} is not valid",
                    entry.verb
                )));
            }
            if !seen_cli.insert(entry.verb.as_str()) {
                return Err(PluginError::InvalidManifest(format!(
                    "cli verb {:?} is declared twice",
                    entry.verb
                )));
            }
            reject_maintenance(entry.effect, &format!("cli verb {:?}", entry.verb))?;
            if entry.effect == Effect::Read {
                let lowered = entry.verb.to_lowercase();
                if let Some(hit) = DESTRUCTIVE_VERBS
                    .iter()
                    .find(|verb| lowered.contains(*verb))
                {
                    return Err(PluginError::InvalidManifest(format!(
                        "cli verb {:?} declares effect Read but its name contains the destructive verb {:?}",
                        entry.verb, hit
                    )));
                }
            }
        }

        // --- capabilities：不得重复、required 与 optional 不得重叠 ---
        for list in [&self.capabilities.required, &self.capabilities.optional] {
            for effect in list {
                reject_maintenance(*effect, "capabilities")?;
            }
            let unique: std::collections::BTreeSet<_> = list.iter().collect();
            if unique.len() != list.len() {
                return Err(PluginError::InvalidManifest(
                    "capabilities must not list the same effect twice".to_string(),
                ));
            }
        }
        for effect in &self.capabilities.required {
            if self.capabilities.optional.contains(effect) {
                return Err(PluginError::InvalidManifest(format!(
                    "capability {:?} is listed as both required and optional",
                    effect.as_str()
                )));
            }
        }

        // --- data：迁移必须逐版本相邻 ---
        for (i, migration) in self.data.migrations.iter().enumerate() {
            if migration.to != migration.from + 1 {
                return Err(PluginError::InvalidManifest(format!(
                    "data.migrations[{i}] goes {} -> {}; migrations must advance exactly one version                      (to == from + 1) so recovery can tell which backup to redo",
                    migration.from, migration.to
                )));
            }
            validate_package_path(&migration.entry, &format!("data.migrations[{i}].entry"))?;
        }

        // --- compatibility ---
        for (field, value) in [
            ("compatibility.hostApi", &self.compatibility.host_api),
            ("compatibility.uiApi", &self.compatibility.ui_api),
        ] {
            if !is_api_version(value) {
                return Err(PluginError::InvalidManifest(format!(
                    "{field} must look like `v1`, got {value:?}"
                )));
            }
        }

        Ok(())
    }
}

fn reject_maintenance(effect: Effect, where_: &str) -> Result<(), PluginError> {
    if effect == Effect::Maintenance {
        return Err(PluginError::InvalidManifest(format!(
            "{where_}: Maintenance is issued only by a lifecycle transaction and must never be declared by a plugin"
        )));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn is_valid_view_id(value: &str) -> bool {
    if value.is_empty() || value.len() > 32 {
        return false;
    }
    let mut chars = value.chars();
    let first = chars.next().expect("non-empty");
    first.is_ascii_lowercase()
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_namespaced_name(value: &str) -> bool {
    let parts: Vec<&str> = value.split('/').collect();
    if parts.len() < 2 {
        return false;
    }
    parts.iter().all(|part| {
        let mut chars = part.chars();
        match chars.next() {
            Some(first) if first.is_ascii_lowercase() => chars.all(|c| c.is_ascii_alphanumeric()),
            _ => false,
        }
    })
}

fn is_cli_verb(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    value.split(':').all(|part| {
        let mut chars = part.chars();
        match chars.next() {
            Some(first) if first.is_ascii_lowercase() => {
                chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            }
            _ => false,
        }
    })
}

fn is_api_version(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('v') else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}
