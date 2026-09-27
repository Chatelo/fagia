//! Rules: categories as data. Built-in rules ship in the binary; user rules
//! override them by id.

use crate::config::Config;
use crate::disk::walk::Classifier;
use crate::model::{Match, Risk};
use crate::paths::display_path;
use crate::platform::Dirs;
use crate::{Error, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod mem;

pub use mem::{MemRule, MemRuleSpec};

pub const BUILTIN: &str = include_str!("../../../../rules/builtin.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RuleKind {
    Dir,
    File,
    Path,
}

/// A rule as written in TOML. Every field but `id` is optional so a user
/// entry can override just one field of a built-in rule.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    pub id: String,
    pub category: Option<String>,
    pub kind: Option<RuleKind>,
    pub names: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub require_sibling: Option<Vec<String>>,
    pub require_inside: Option<Vec<String>>,
    pub regenerable: Option<bool>,
    pub regenerate: Option<String>,
    pub risk: Option<Risk>,
    pub priority: Option<i32>,
    pub enabled: Option<bool>,
}

impl RuleSpec {
    fn overlay(&mut self, o: RuleSpec) {
        macro_rules! take {
            ($($f:ident),*) => { $( if o.$f.is_some() { self.$f = o.$f; } )* };
        }
        take!(
            category,
            kind,
            names,
            extensions,
            paths,
            require_sibling,
            require_inside,
            regenerable,
            regenerate,
            risk,
            priority,
            enabled
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Builtin,
    User,
    Overridden,
}

/// Glob patterns matched against single file names.
#[derive(Debug, Clone)]
pub struct NamePatterns {
    pub patterns: Vec<String>,
    set: GlobSet,
}

impl NamePatterns {
    fn new(id: &str, patterns: Vec<String>) -> Result<Self> {
        let mut b = GlobSetBuilder::new();
        for p in &patterns {
            let g = GlobBuilder::new(p)
                .literal_separator(true)
                .build()
                .map_err(|e| Error::Rule {
                    id: id.to_string(),
                    message: format!("bad pattern {p:?}: {e}"),
                })?;
            b.add(g);
        }
        let set = b.build().map_err(|e| Error::Rule {
            id: id.to_string(),
            message: e.to_string(),
        })?;
        Ok(Self { patterns, set })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// First name in `names` matching any pattern.
    pub fn find<'a>(&self, names: &'a [OsString]) -> Option<&'a OsString> {
        names
            .iter()
            .find(|n| self.set.is_match(Path::new(n.as_os_str())))
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: Arc<str>,
    pub category: Arc<str>,
    pub kind: RuleKind,
    pub names: Vec<String>,
    pub extensions: Vec<String>,
    pub raw_paths: Vec<String>,
    pub paths: Vec<PathBuf>,
    pub require_sibling: NamePatterns,
    pub require_inside: NamePatterns,
    pub regenerable: bool,
    pub regenerate: Option<Arc<str>>,
    pub risk: Risk,
    pub priority: i32,
    pub enabled: bool,
    pub origin: Origin,
}

impl Rule {
    fn from_spec(spec: RuleSpec, origin: Origin, dirs: &Dirs) -> Result<Self> {
        let id = spec.id.clone();
        let err = |m: &str| Error::Rule {
            id: id.clone(),
            message: m.to_string(),
        };
        let kind = spec
            .kind
            .ok_or_else(|| err("missing `kind` (dir, file or path)"))?;
        let raw_paths = spec.paths.unwrap_or_default();
        let paths = raw_paths
            .iter()
            .map(|p| {
                resolve_vars(p, dirs).ok_or_else(|| err(&format!("unknown variable in path {p:?}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let rule = Rule {
            id: Arc::from(spec.id.as_str()),
            category: Arc::from(spec.category.unwrap_or_else(|| spec.id.clone()).as_str()),
            kind,
            names: spec.names.unwrap_or_default(),
            extensions: spec
                .extensions
                .unwrap_or_default()
                .into_iter()
                .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                .collect(),
            raw_paths,
            paths,
            require_sibling: NamePatterns::new(&spec.id, spec.require_sibling.unwrap_or_default())?,
            require_inside: NamePatterns::new(&spec.id, spec.require_inside.unwrap_or_default())?,
            regenerable: spec.regenerable.unwrap_or(false),
            regenerate: spec.regenerate.map(|s| Arc::from(s.as_str())),
            risk: spec.risk.unwrap_or_default(),
            priority: spec.priority.unwrap_or(0),
            enabled: spec.enabled.unwrap_or(true),
            origin,
        };
        rule.check_shape()?;
        Ok(rule)
    }

    fn check_shape(&self) -> Result<()> {
        let err = |m: String| {
            Err(Error::Rule {
                id: self.id.to_string(),
                message: m,
            })
        };
        match self.kind {
            RuleKind::Dir if self.names.is_empty() => err("a dir rule needs `names`".into()),
            RuleKind::File if self.names.is_empty() && self.extensions.is_empty() => {
                err("a file rule needs `names` or `extensions`".into())
            }
            RuleKind::Path if self.paths.is_empty() => err("a path rule needs `paths`".into()),
            _ if self
                .names
                .iter()
                .any(|n| n.contains('/') || n.is_empty() || n == "." || n == "..") =>
            {
                err("`names` must be plain file or folder names".into())
            }
            RuleKind::Path if self.paths.iter().any(|p| !p.is_absolute()) => {
                err("`paths` must be absolute (use {home} and friends)".into())
            }
            _ => Ok(()),
        }
    }

    /// Number of marker patterns; more markers means a more specific rule.
    fn specificity(&self) -> usize {
        self.require_sibling.patterns.len() + self.require_inside.patterns.len()
    }

    fn to_match(&self, evidence: String, project_dir: Option<PathBuf>) -> Match {
        Match {
            rule_id: self.id.clone(),
            category: self.category.clone(),
            evidence,
            regenerable: self.regenerable,
            regenerate: self.regenerate.clone(),
            risk: self.risk,
            project_dir,
        }
    }

    /// Auto-selected in `clean` only when regenerable and low risk.
    pub fn auto_select(regenerable: bool, risk: Risk) -> bool {
        regenerable && risk == Risk::Low
    }
}

/// Replaces `{var}` placeholders; `None` for an unknown variable.
pub fn resolve_vars(raw: &str, dirs: &Dirs) -> Option<PathBuf> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let end = rest[start..].find('}')? + start;
        out.push_str(dirs.var(&rest[start + 1..end])?.to_str()?);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(crate::paths::expand_tilde(&out, &dirs.home))
}

/// Outcome of evaluating one rule against one path, for `rules test`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RuleCheck {
    pub rule_id: String,
    pub category: String,
    pub matched: bool,
    pub reason: String,
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub struct RuleSet {
    rules: Vec<Rule>,
    mem_rules: Vec<MemRule>,
    dir_names: HashMap<OsString, Vec<usize>>,
    file_names: HashMap<OsString, Vec<usize>>,
    extensions: HashMap<String, Vec<usize>>,
    path_rules: HashMap<PathBuf, Vec<usize>>,
    max_ext_dots: usize,
    /// For showing known paths with `~`.
    home: PathBuf,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RuleFile {
    rule: Vec<RuleSpec>,
    mem_rule: Vec<MemRuleSpec>,
}

impl RuleSet {
    pub fn builtin(dirs: &Dirs) -> Result<Self> {
        Self::load(&Config::default(), dirs)
    }

    /// Built-in rules with the user's config merged over them by id.
    pub fn load(cfg: &Config, dirs: &Dirs) -> Result<Self> {
        let file: RuleFile = toml::from_str(BUILTIN).map_err(|e| Error::Rule {
            id: "builtin".into(),
            message: e.to_string(),
        })?;
        let mut specs: BTreeMap<String, (RuleSpec, Origin, usize)> = BTreeMap::new();
        for (i, s) in file.rule.into_iter().enumerate() {
            specs.insert(s.id.clone(), (s, Origin::Builtin, i));
        }
        let base = specs.len();
        for (i, s) in cfg.rule.iter().cloned().enumerate() {
            match specs.get_mut(&s.id) {
                Some((existing, origin, _)) => {
                    existing.overlay(s);
                    *origin = Origin::Overridden;
                }
                None => {
                    specs.insert(s.id.clone(), (s, Origin::User, base + i));
                }
            }
        }
        let mut ordered: Vec<_> = specs.into_values().collect();
        ordered.sort_by_key(|(_, _, i)| *i);
        let rules = ordered
            .into_iter()
            .map(|(s, o, _)| Rule::from_spec(s, o, dirs))
            .collect::<Result<Vec<_>>>()?;
        let mem_rules = mem::merge(file.mem_rule, &cfg.mem_rule)?;
        let mut set = Self::index(rules, mem_rules);
        set.home = dirs.home.clone();
        Ok(set)
    }

    fn index(rules: Vec<Rule>, mem_rules: Vec<MemRule>) -> Self {
        let mut s = Self {
            rules,
            mem_rules,
            dir_names: HashMap::new(),
            file_names: HashMap::new(),
            extensions: HashMap::new(),
            path_rules: HashMap::new(),
            max_ext_dots: 1,
            home: PathBuf::new(),
        };
        for (i, r) in s.rules.iter().enumerate().filter(|(_, r)| r.enabled) {
            match r.kind {
                RuleKind::Dir => {
                    for n in &r.names {
                        s.dir_names.entry(n.into()).or_default().push(i);
                    }
                }
                RuleKind::File => {
                    for n in &r.names {
                        s.file_names.entry(n.into()).or_default().push(i);
                    }
                    for e in &r.extensions {
                        s.max_ext_dots = s.max_ext_dots.max(e.matches('.').count() + 1);
                        s.extensions.entry(e.clone()).or_default().push(i);
                    }
                }
                RuleKind::Path => {
                    for p in &r.paths {
                        s.path_rules.entry(p.clone()).or_default().push(i);
                    }
                }
            }
        }
        s
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn mem_rules(&self) -> &[MemRule] {
        &self.mem_rules
    }

    pub fn get(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| &*r.id == id)
    }

    /// Problems that make a rule unsafe to use: fixed paths that are, or
    /// contain, a protected path.
    pub fn validate(&self, protected: &[PathBuf]) -> Vec<String> {
        let mut problems = Vec::new();
        for r in self.rules.iter().filter(|r| r.enabled) {
            for p in &r.paths {
                if let Some(bad) = protected.iter().find(|pr| pr.starts_with(p)) {
                    problems.push(format!(
                        "rule {}: path {} would cover protected path {}",
                        r.id,
                        p.display(),
                        bad.display()
                    ));
                }
            }
        }
        problems
    }

    fn evidence(
        &self,
        r: &Rule,
        path: &Path,
        siblings: &[OsString],
        inside: &mut dyn FnMut() -> Option<Vec<OsString>>,
    ) -> std::result::Result<(String, Option<PathBuf>), String> {
        let parent = path.parent().map(Path::to_path_buf);
        let mut parts = Vec::new();
        if r.kind == RuleKind::Path {
            return Ok((
                format!("known path {}", display_path(path, Some(&self.home))),
                None,
            ));
        }
        if !r.require_sibling.is_empty() {
            match r.require_sibling.find(siblings) {
                Some(n) => parts.push(format!("{} in parent", n.to_string_lossy())),
                None => {
                    return Err(format!(
                        "no {} in parent",
                        r.require_sibling.patterns.join(" or ")
                    ));
                }
            }
        }
        if !r.require_inside.is_empty() {
            let names = inside().unwrap_or_default();
            match r.require_inside.find(&names) {
                Some(n) => parts.push(format!("{} inside", n.to_string_lossy())),
                None => {
                    return Err(format!(
                        "no {} inside",
                        r.require_inside.patterns.join(" or ")
                    ));
                }
            }
        }
        if parts.is_empty() {
            parts.push(if r.kind == RuleKind::File && !r.extensions.is_empty() {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let ext = r
                    .extensions
                    .iter()
                    .filter(|e| name.to_ascii_lowercase().ends_with(&format!(".{e}")))
                    .max_by_key(|e| e.len())
                    .cloned()
                    .unwrap_or_default();
                format!("extension .{ext}")
            } else {
                "name match".to_string()
            });
        }
        let project = (r.kind == RuleKind::Dir).then_some(parent).flatten();
        Ok((parts.join(", "), project))
    }

    fn candidates(&self, path: &Path, name: &OsStr, is_dir: bool) -> Vec<usize> {
        let mut c: Vec<usize> = self.path_rules.get(path).cloned().unwrap_or_default();
        if is_dir {
            c.extend(self.dir_names.get(name).into_iter().flatten());
        } else {
            c.extend(self.file_names.get(name).into_iter().flatten());
            let lower = name.to_string_lossy().to_ascii_lowercase();
            // Every suffix after a dot, up to the longest extension known
            // (`a.tar.gz` checks `gz` and `tar.gz`).
            for (idx, _) in lower
                .match_indices('.')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .take(self.max_ext_dots)
            {
                if idx == 0 {
                    continue;
                }
                if let Some(v) = self.extensions.get(&lower[idx + 1..]) {
                    c.extend(v);
                }
            }
        }
        c.sort_unstable();
        c.dedup();
        c
    }

    fn best(
        &self,
        path: &Path,
        name: &OsStr,
        is_dir: bool,
        siblings: &[OsString],
        inside: &mut dyn FnMut() -> Option<Vec<OsString>>,
    ) -> Option<Match> {
        let mut winner: Option<(&Rule, String, Option<PathBuf>)> = None;
        for i in self.candidates(path, name, is_dir) {
            let r = &self.rules[i];
            if r.kind == RuleKind::File && is_dir {
                continue;
            }
            if let Ok((ev, proj)) = self.evidence(r, path, siblings, inside) {
                let better = match &winner {
                    None => true,
                    Some((w, _, _)) => rank(r) > rank(w),
                };
                if better {
                    winner = Some((r, ev, proj));
                }
            }
        }
        winner.map(|(r, ev, proj)| r.to_match(ev, proj))
    }

    /// Every rule that could apply to `path`, whether it matched and why,
    /// in winning order. Used by `fagia rules test`.
    pub fn explain(&self, path: &Path) -> Vec<RuleCheck> {
        let is_dir = std::fs::symlink_metadata(path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        let name = path.file_name().unwrap_or_default();
        let siblings = path.parent().map(list_names).unwrap_or_default();
        let mut inside_cache: Option<Vec<OsString>> = None;
        let mut inside = || Some(inside_cache.get_or_insert_with(|| list_names(path)).clone());
        let mut checks: Vec<(&Rule, RuleCheck)> = self
            .candidates(path, name, is_dir)
            .into_iter()
            .map(|i| &self.rules[i])
            .filter(|r| !(r.kind == RuleKind::File && is_dir))
            .map(|r| {
                let (matched, reason) = match self.evidence(r, path, &siblings, &mut inside) {
                    Ok((ev, _)) => (true, ev),
                    Err(why) => (false, why),
                };
                (
                    r,
                    RuleCheck {
                        rule_id: r.id.to_string(),
                        category: r.category.to_string(),
                        matched,
                        reason,
                        priority: r.priority,
                    },
                )
            })
            .collect();
        checks.sort_by(|(ra, a), (rb, b)| b.matched.cmp(&a.matched).then(rank(rb).cmp(&rank(ra))));
        checks.into_iter().map(|(_, c)| c).collect()
    }
}

/// Higher wins: priority, then specificity, then the alphabetically first id.
fn rank(r: &Rule) -> (i32, usize, std::cmp::Reverse<&str>) {
    (r.priority, r.specificity(), std::cmp::Reverse(&*r.id))
}

fn list_names(dir: &Path) -> Vec<OsString> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default()
}

impl Classifier for RuleSet {
    fn classify_dir(
        &self,
        path: &Path,
        name: &OsStr,
        siblings: &[OsString],
        inside: &mut dyn FnMut() -> Option<Vec<OsString>>,
    ) -> Option<Match> {
        self.best(path, name, true, siblings, inside)
    }

    fn classify_file(&self, path: &Path, name: &OsStr, siblings: &[OsString]) -> Option<Match> {
        self.best(path, name, false, siblings, &mut || None)
    }
}

#[cfg(test)]
mod tests;
