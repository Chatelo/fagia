//! User configuration (`~/.config/fagia/config.toml`) and the restricted
//! per-project `.fagia.toml`.

use crate::error::IoContext;
use crate::paths::expand_tilde;
use crate::platform::Dirs;
use crate::rules::{MemRuleSpec, RuleSpec};
use crate::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

pub const PROJECT_FILE: &str = ".fagia.toml";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub protect: Protect,
    pub exclude: Exclude,
    pub ram: RamConfig,
    pub rule: Vec<RuleSpec>,
    pub mem_rule: Vec<MemRuleSpec>,
    /// TUI key bindings: action name to key, for example `clean = "x"`.
    pub keys: BTreeMap<String, String>,
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub min_size: String,
    pub stale_days: u64,
    pub save_history: bool,
    pub history_keep: usize,
}

impl Default for General {
    fn default() -> Self {
        Self {
            min_size: "1M".into(),
            stale_days: 90,
            save_history: true,
            history_keep: 30,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Protect {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Exclude {
    pub globs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RamConfig {
    pub sample_seconds: u64,
    pub forgotten_hours: u64,
    /// A dev process whose working-directory project has had no source
    /// change for this many days also counts as forgotten.
    pub forgotten_idle_days: u64,
    pub leak_min_minutes: u64,
    pub leak_warmup_minutes: u64,
    /// Minimum growth rate for a leak suspect, in MiB per minute.
    pub leak_rate_mib_per_min: f64,
    pub grace_seconds: u64,
}

impl Default for RamConfig {
    fn default() -> Self {
        Self {
            sample_seconds: 5,
            forgotten_hours: 24,
            forgotten_idle_days: 3,
            leak_min_minutes: 10,
            leak_warmup_minutes: 5,
            leak_rate_mib_per_min: 1.0,
            grace_seconds: 5,
        }
    }
}

impl Config {
    pub fn default_path(dirs: &Dirs) -> PathBuf {
        dirs.config.join("fagia/config.toml")
    }

    /// Loads the config at `path`, or defaults when the file is absent.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).at(path)?;
        let mut cfg = Self::parse(&text).map_err(|message| Error::Config {
            path: path.to_path_buf(),
            message,
        })?;
        cfg.source = Some(path.to_path_buf());
        Ok(cfg)
    }

    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    pub fn protect_paths(&self, home: &Path) -> Vec<PathBuf> {
        self.protect
            .paths
            .iter()
            .map(|p| expand_tilde(p, home))
            .collect()
    }

    pub fn exclude_globs(&self, home: &Path) -> Vec<String> {
        self.exclude
            .globs
            .iter()
            .map(|g| expand_tilde(g, home).to_string_lossy().into_owned())
            .collect()
    }

    pub fn min_size(&self) -> Result<u64> {
        crate::size::parse_size(&self.general.min_size)
    }
}

/// What a project's `.fagia.toml` may contribute. It is untrusted (it
/// arrives with a cloned repository), so it can only narrow what fagia
/// touches: extra excludes and protections inside its own project.
#[derive(Debug, Clone, Default)]
pub struct ProjectConfig {
    pub dir: PathBuf,
    pub excludes: Vec<String>,
    pub protect: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

impl ProjectConfig {
    pub fn parse(text: &str, dir: &Path) -> Self {
        let mut out = Self {
            dir: dir.to_path_buf(),
            ..Self::default()
        };
        let file = dir.join(PROJECT_FILE);
        let table: toml::Table = match toml::from_str(text) {
            Ok(t) => t,
            Err(e) => {
                out.warnings
                    .push(format!("{}: ignored, invalid TOML: {e}", file.display()));
                return out;
            }
        };
        for (key, value) in table {
            match key.as_str() {
                "exclude" => out.excludes = string_list(value.get("globs")),
                "protect" => {
                    for raw in string_list(value.get("paths")) {
                        match normalize_within(dir, Path::new(&raw)) {
                            Some(p) => out.protect.push(p),
                            None => out.warnings.push(format!(
                                "{}: protect path {raw:?} is outside the project; ignored",
                                file.display()
                            )),
                        }
                    }
                }
                other => out.warnings.push(format!(
                    "{}: [{other}] ignored; a project file may only set exclude and protect",
                    file.display()
                )),
            }
        }
        out
    }
}

fn string_list(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Joins `rel` onto `base` lexically and returns it only if it stays
/// inside `base`.
pub fn normalize_within(base: &Path, rel: &Path) -> Option<PathBuf> {
    if rel.is_absolute() {
        return None;
    }
    let mut out = base.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || !out.starts_with(base) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    out.starts_with(base).then_some(out)
}

pub const TEMPLATE: &str = r#"# fagia configuration. Every key is optional.

[general]
min_size = "1M"        # hide entries smaller than this
stale_days = 90        # suspects untouched this long count as stale
save_history = true    # store a snapshot per scan for `fagia diff`
history_keep = 30      # snapshots kept per scanned root

[protect]
paths = []             # e.g. ["~/Documents", "~/Pictures/family"]

[exclude]
globs = []             # e.g. ["/mnt/backup/**"]

[ram]
sample_seconds = 5
forgotten_hours = 24

# A new disk category:
# [[rule]]
# id = "elixir-build"
# category = "Elixir build"
# kind = "dir"
# names = ["_build", "deps"]
# require_sibling = ["mix.exs"]
# regenerable = true
# regenerate = "mix deps.get && mix compile"
# risk = "low"

# Turn off a built-in rule:
# [[rule]]
# id = "generic-build"
# enabled = false
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses() {
        let cfg = Config::parse(TEMPLATE).unwrap();
        assert_eq!(cfg.general.stale_days, 90);
    }

    #[test]
    fn documented_example_parses() {
        let text = r#"
[general]
min_size = "50M"
stale_days = 90
[protect]
paths = ["~/Documents"]
[[rule]]
id = "elixir-build"
category = "Elixir build"
kind = "dir"
names = ["_build", "deps"]
require_sibling = ["mix.exs"]
regenerable = true
regenerate = "mix deps.get && mix compile"
risk = "low"
[[rule]]
id = "generic-build"
enabled = false
[[mem_rule]]
id = "ollama"
group = "Local AI models"
exe = ["ollama", "llama-server"]
dev_suspect = true
"#;
        let cfg = Config::parse(text).unwrap();
        assert_eq!(cfg.rule.len(), 2);
        assert_eq!(cfg.mem_rule[0].id, "ollama");
        assert_eq!(
            cfg.protect_paths(Path::new("/home/u")),
            vec![PathBuf::from("/home/u/Documents")]
        );
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(Config::parse("[general]\nmin_sise = \"1M\"\n").is_err());
    }

    #[test]
    fn hostile_project_file_cannot_add_rules() {
        let dir = Path::new("/p/repo");
        let text = r#"
[[rule]]
id = "src-is-junk"
kind = "dir"
names = ["src"]
regenerable = true
[general]
stale_days = 0
[protect]
paths = ["keep", "../../etc"]
[exclude]
globs = ["vendor/**"]
"#;
        let pc = ProjectConfig::parse(text, dir);
        assert_eq!(pc.protect, vec![PathBuf::from("/p/repo/keep")]);
        assert_eq!(pc.excludes, vec!["vendor/**".to_string()]);
        assert_eq!(pc.warnings.len(), 3, "{:?}", pc.warnings);
    }

    #[test]
    fn normalize_stays_inside() {
        let b = Path::new("/a/b");
        assert_eq!(
            normalize_within(b, Path::new("c/../d")),
            Some(PathBuf::from("/a/b/d"))
        );
        assert_eq!(normalize_within(b, Path::new("..")), None);
        assert_eq!(normalize_within(b, Path::new("/etc")), None);
    }
}
