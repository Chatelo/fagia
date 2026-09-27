//! Docker disk usage via `docker system df`. Docker's data lives in a
//! root-owned directory (or a VM image), so the walker cannot size it.

use super::{Provider, on_path, run_with_timeout};
use crate::model::{EntryKind, Finding, Match, Risk};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

pub struct Docker;

impl Provider for Docker {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn findings(&self) -> Result<Vec<Finding>, String> {
        if !on_path("docker") {
            return Ok(Vec::new());
        }
        let out = run_with_timeout(
            Command::new("docker").args(["system", "df", "--format", "{{json .}}"]),
            Duration::from_secs(5),
        )?;
        Ok(parse(&out))
    }
}

pub(crate) fn parse(text: &str) -> Vec<Finding> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let kind = v.get("Type")?.as_str()?.to_string();
            let size = parse_docker_size(v.get("Size")?.as_str()?)?;
            let reclaimable = v
                .get("Reclaimable")
                .and_then(|r| r.as_str())
                .and_then(|r| parse_docker_size(r.split_whitespace().next()?))
                .unwrap_or(0);
            let count = v
                .get("TotalCount")
                .and_then(|c| c.as_str()?.parse().ok())
                .unwrap_or(0);
            if size == 0 {
                return None;
            }
            let prune = match kind.as_str() {
                "Images" => "docker image prune -a",
                "Containers" => "docker container prune",
                "Local Volumes" => "docker volume prune",
                "Build Cache" => "docker builder prune",
                _ => "docker system prune",
            };
            let m = Match {
                rule_id: Arc::from("docker"),
                category: Arc::from("Docker"),
                evidence: format!("docker system df: {count} {}", kind.to_lowercase()),
                // Removed through docker itself, never by deleting files.
                regenerable: false,
                regenerate: Some(Arc::from("docker pull / docker build")),
                risk: if kind == "Local Volumes" {
                    Risk::High
                } else {
                    Risk::Medium
                },
                project_dir: None,
            };
            let mut f = Finding::from_match(
                PathBuf::from(format!("docker:{}", kind.to_lowercase().replace(' ', "-"))),
                EntryKind::Provider,
                &m,
            );
            f.real = size;
            f.apparent = size;
            f.files = count;
            f.reclaimable = reclaimable;
            f.note = Some(format!("reclaim with `{prune}`"));
            Some(f)
        })
        .collect()
}

/// Docker prints decimal units: `1.5GB`, `512MB`, `0B`, `12.3kB`.
fn parse_docker_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num, unit) = s.split_at(split);
    let v: f64 = num.parse().ok()?;
    let mult = match unit.to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        _ => return None,
    };
    Some((v * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_system_df_json_lines() {
        let text = r#"{"Active":"2","Reclaimable":"1.2GB (40%)","Size":"3GB","TotalCount":"7","Type":"Images"}
{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"0","Type":"Containers"}
{"Active":"1","Reclaimable":"512MB (100%)","Size":"512MB","TotalCount":"3","Type":"Build Cache"}"#;
        let f = parse(text);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].real, 3_000_000_000);
        assert_eq!(f[0].reclaimable, 1_200_000_000);
        assert!(!f[0].regenerable);
        assert_eq!(f[1].path, PathBuf::from("docker:build-cache"));
        assert_eq!(parse_docker_size("12.5kB"), Some(12_500));
    }
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub memory: u64,
    pub memory_limit: u64,
}

/// Running containers and their memory. `docker ps` lists them (fast);
/// memory comes from each container's cgroup, since `docker stats` needs
/// about two seconds to sample.
pub fn containers(platform: &dyn crate::platform::Platform) -> Result<Vec<Container>, String> {
    if !on_path("docker") {
        return Ok(Vec::new());
    }
    let out = run_with_timeout(
        Command::new("docker").args(["ps", "--no-trunc", "--format", "{{json .}}"]),
        Duration::from_secs(5),
    )?;
    let mut list: Vec<Container> = out
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let id = v.get("ID")?.as_str()?.to_string();
            let memory = platform
                .cgroup_memory(&format!("system.slice/docker-{id}.scope"))
                .unwrap_or(0);
            Some(Container {
                name: v.get("Names")?.as_str()?.to_string(),
                id,
                memory,
                memory_limit: 0,
            })
        })
        .collect();
    list.sort_by_key(|c| std::cmp::Reverse(c.memory));
    Ok(list)
}
