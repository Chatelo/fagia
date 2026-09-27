//! Memory rules: how processes are named, grouped and protected.

use crate::platform::ProcInfo;
use crate::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemRuleSpec {
    pub id: String,
    pub group: Option<String>,
    pub exe: Option<Vec<String>>,
    /// Substrings matched against the joined command line.
    pub cmdline: Option<Vec<String>>,
    pub dev_suspect: Option<bool>,
    pub unsaved_work: Option<bool>,
    pub system: Option<bool>,
    pub boundary: Option<bool>,
    pub split_by_cwd: Option<bool>,
    /// Name each app after its executable (`firefox`, `slack`) and show
    /// `group` as its category, instead of merging all matches into one.
    pub per_app: Option<bool>,
    pub reason: Option<String>,
    pub enabled: Option<bool>,
}

impl MemRuleSpec {
    fn overlay(&mut self, o: MemRuleSpec) {
        macro_rules! take {
            ($($f:ident),*) => { $( if o.$f.is_some() { self.$f = o.$f; } )* };
        }
        take!(
            group,
            exe,
            cmdline,
            dev_suspect,
            unsaved_work,
            system,
            boundary,
            split_by_cwd,
            reason,
            enabled
        );
    }
}

#[derive(Debug, Clone)]
pub struct MemRule {
    pub id: String,
    pub group: String,
    pub exe: Vec<String>,
    pub cmdline: Vec<String>,
    pub dev_suspect: bool,
    pub unsaved_work: bool,
    pub system: bool,
    pub boundary: bool,
    pub split_by_cwd: bool,
    pub per_app: bool,
    pub reason: Option<String>,
}

impl MemRule {
    pub fn matches(&self, p: &ProcInfo) -> bool {
        let exe = p.exe_name();
        if self.exe.iter().any(|e| *e == exe || *e == p.comm) {
            return true;
        }
        if !self.cmdline.is_empty() {
            let joined = p.cmdline.join(" ");
            return self.cmdline.iter().any(|c| joined.contains(c.as_str()));
        }
        false
    }
}

pub(crate) fn merge(builtin: Vec<MemRuleSpec>, user: &[MemRuleSpec]) -> Result<Vec<MemRule>> {
    let mut specs: BTreeMap<String, (MemRuleSpec, usize)> = BTreeMap::new();
    for (i, s) in builtin.into_iter().enumerate() {
        specs.insert(s.id.clone(), (s, i));
    }
    let base = specs.len();
    for (i, s) in user.iter().cloned().enumerate() {
        match specs.get_mut(&s.id) {
            Some((existing, _)) => existing.overlay(s),
            None => {
                specs.insert(s.id.clone(), (s, base + i));
            }
        }
    }
    let mut ordered: Vec<_> = specs.into_values().collect();
    // User rules come after built-ins but are checked first, so a user can
    // claim a process a built-in rule would also match.
    ordered.sort_by_key(|(_, i)| (*i < base, *i));
    ordered
        .into_iter()
        .filter(|(s, _)| s.enabled.unwrap_or(true))
        .map(|(s, _)| {
            if s.exe.as_ref().is_none_or(Vec::is_empty)
                && s.cmdline.as_ref().is_none_or(Vec::is_empty)
            {
                return Err(Error::Rule {
                    id: s.id.clone(),
                    message: "a mem_rule needs `exe` or `cmdline`".into(),
                });
            }
            Ok(MemRule {
                group: s.group.clone().unwrap_or_else(|| s.id.clone()),
                id: s.id,
                exe: s.exe.unwrap_or_default(),
                cmdline: s.cmdline.unwrap_or_default(),
                dev_suspect: s.dev_suspect.unwrap_or(false),
                unsaved_work: s.unsaved_work.unwrap_or(false),
                system: s.system.unwrap_or(false),
                boundary: s.boundary.unwrap_or(false),
                split_by_cwd: s.split_by_cwd.unwrap_or(false),
                per_app: s.per_app.unwrap_or(false),
                reason: s.reason,
            })
        })
        .collect()
}
