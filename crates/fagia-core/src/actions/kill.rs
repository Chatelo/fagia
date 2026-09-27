//! Quitting, pausing and resuming app groups. Polite first (SIGTERM),
//! force only after a grace period and a second confirmation; never
//! system processes, never fagia or the shell running it.

use super::log::{ActionLog, LogEntry};
use crate::platform::{Platform, ProcKey, Signal};
use crate::ram::group::Group;
use crate::{Error, Result};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SignalKind {
    Quit,
    ForceKill,
    Pause,
    Resume,
}

impl SignalKind {
    fn signal(self) -> Signal {
        match self {
            Self::Quit => Signal::Terminate,
            Self::ForceKill => Signal::Kill,
            Self::Pause => Signal::Stop,
            Self::Resume => Signal::Continue,
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Quit => "signal-term",
            Self::ForceKill => "signal-kill",
            Self::Pause => "signal-stop",
            Self::Resume => "signal-cont",
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SignalTarget {
    pub key: ProcKey,
    pub name: String,
    pub command: String,
    pub fair: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SignalPlan {
    pub group: String,
    pub targets: Vec<SignalTarget>,
    /// Editors, browsers and office apps may hold unsaved work.
    pub unsaved_work: bool,
}

impl SignalPlan {
    pub fn allowed(&self) -> impl Iterator<Item = &SignalTarget> {
        self.targets.iter().filter(|t| t.refusal.is_none())
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SignalResult {
    pub pid: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// fagia itself and every ancestor: signalling the shell or terminal that
/// runs fagia would end the session doing the cleanup.
fn self_and_ancestors(platform: &dyn Platform) -> HashSet<u32> {
    let mut out = HashSet::new();
    let mut pid = platform.current_pid();
    while pid > 1 && out.insert(pid) {
        match platform.process(pid) {
            Some(p) => pid = p.ppid,
            None => break,
        }
    }
    out
}

pub fn plan(
    platform: &dyn Platform,
    group: &Group,
    home: Option<&std::path::Path>,
    allow_other_users: bool,
) -> SignalPlan {
    let protected = self_and_ancestors(platform);
    let me = platform.current_uid();
    let elevated = platform.effective_uid() == 0;
    let targets = group
        .members
        .iter()
        .map(|m| {
            let p = &m.info;
            let refusal = if p.key.pid <= 1 || p.kernel_thread {
                Some("init or kernel thread".to_string())
            } else if group.system {
                Some("system process".to_string())
            } else if protected.contains(&p.key.pid) {
                Some("fagia itself or the shell running it".to_string())
            } else if p.uid != me && !(elevated && allow_other_users) {
                Some(format!(
                    "belongs to user {}; needs root and --allow-other-users",
                    p.uid
                ))
            } else {
                None
            };
            SignalTarget {
                key: p.key,
                name: p.exe_name(),
                command: crate::paths::escape_control(&p.cmdline.join(" ")),
                fair: m.fair(),
                refusal,
            }
        })
        .collect();
    SignalPlan {
        group: group.display_name(home),
        targets,
        unsaved_work: group.unsaved_work,
    }
}

/// Sends `kind` to every allowed target, each through a pidfd that
/// verifies the process is still the one planned.
pub fn send(
    platform: &dyn Platform,
    log: &ActionLog,
    plan: &SignalPlan,
    kind: SignalKind,
) -> Vec<SignalResult> {
    let run = format!("{}-{}", crate::model::now_epoch(), platform.current_pid());
    plan.allowed()
        .filter(|t| platform.is_alive(t.key))
        .map(|t| {
            #[allow(clippy::disallowed_methods)] // this is the action gate
            let res = platform.signal(t.key, kind.signal());
            let mut entry =
                LogEntry::new(&run, kind.action(), &PathBuf::from(&t.name), t.fair, "ok");
            entry.pid = Some(t.key.pid);
            entry.detail = Some(t.command.clone());
            let result = match res {
                Ok(()) => SignalResult {
                    pid: t.key.pid,
                    ok: true,
                    detail: None,
                },
                Err(e) => {
                    entry.outcome = "failed".into();
                    entry.detail = Some(e.to_string());
                    SignalResult {
                        pid: t.key.pid,
                        ok: false,
                        detail: Some(e.to_string()),
                    }
                }
            };
            let _ = log.append(&entry);
            result
        })
        .collect()
}

/// Waits up to `grace` for the targets to exit; returns those still alive.
pub fn wait_for_exit(platform: &dyn Platform, plan: &SignalPlan, grace: Duration) -> Vec<ProcKey> {
    let deadline = Instant::now() + grace;
    loop {
        let alive: Vec<ProcKey> = plan
            .allowed()
            .map(|t| t.key)
            .filter(|k| platform.is_alive(*k))
            .collect();
        if alive.is_empty() || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Keeps only the targets in `keys` (for the force-kill step).
pub fn narrow(plan: &SignalPlan, keys: &[ProcKey]) -> SignalPlan {
    SignalPlan {
        group: plan.group.clone(),
        targets: plan
            .targets
            .iter()
            .filter(|t| keys.contains(&t.key))
            .cloned()
            .collect(),
        unsaved_work: plan.unsaved_work,
    }
}

/// Stops a Docker container through the Docker CLI (the provider's
/// action), logged like every other action.
pub fn stop_container(log: &ActionLog, name: &str) -> Result<()> {
    let out = crate::providers::run_with_timeout(
        std::process::Command::new("docker").args(["stop", name]),
        Duration::from_secs(60),
    );
    let mut entry = LogEntry::new("docker", "docker-stop", &PathBuf::from(name), 0, "ok");
    if let Err(e) = &out {
        entry.outcome = "failed".into();
        entry.detail = Some(e.clone());
    }
    log.append(&entry)?;
    out.map(|_| ()).map_err(Error::Other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::linux::LinuxPlatform;
    use crate::ram::group::Member;
    use std::process::Command;

    fn group_of(plat: &LinuxPlatform, pids: &[u32]) -> Group {
        Group {
            name: "sleepers".into(),
            category: None,
            rule_id: None,
            uid: plat.current_uid(),
            cwd: None,
            members: pids
                .iter()
                .map(|&pid| Member {
                    info: plat.process(pid).unwrap(),
                    mem: plat.process_memory(pid),
                })
                .collect(),
            dev_suspect: false,
            system: false,
            unsaved_work: false,
            reason: None,
        }
    }

    #[test]
    fn quit_pause_resume_and_refusals() {
        let plat = LinuxPlatform::new();
        let tmp = tempfile::tempdir().unwrap();
        let log = ActionLog::new(tmp.path().join("a.jsonl"));
        let mut kids: Vec<_> = (0..2)
            .map(|_| Command::new("sleep").arg("60").spawn().unwrap())
            .collect();
        let pids: Vec<u32> = kids.iter().map(|c| c.id()).collect();
        let g = group_of(&plat, &pids);
        let p = plan(&plat, &g, None, false);
        assert_eq!(p.allowed().count(), 2);

        send(&plat, &log, &p, SignalKind::Pause);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(plat.process(pids[0]).unwrap().state, 'T');
        send(&plat, &log, &p, SignalKind::Resume);
        std::thread::sleep(Duration::from_millis(100));
        assert_ne!(plat.process(pids[0]).unwrap().state, 'T');

        let res = send(&plat, &log, &p, SignalKind::Quit);
        assert!(res.iter().all(|r| r.ok));
        for k in kids.iter_mut() {
            let _ = k.wait();
        }
        assert!(wait_for_exit(&plat, &p, Duration::from_secs(2)).is_empty());
        let entries = log.read();
        assert_eq!(entries.len(), 6);
        assert!(entries.iter().all(|e| e.pid.is_some()));
    }

    #[test]
    fn reused_pid_is_refused() {
        let plat = LinuxPlatform::new();
        let mut kid = Command::new("sleep").arg("60").spawn().unwrap();
        let mut key = plat.process(kid.id()).unwrap().key;
        key.start_ticks += 1; // as if the PID now belonged to another process
        #[allow(clippy::disallowed_methods)] // testing the primitive itself
        let err = plat.signal(key, Signal::Terminate).unwrap_err();
        assert!(err.to_string().contains("different process"));
        assert!(
            plat.process(kid.id()).is_some(),
            "the process must be untouched"
        );
        let _ = kid.kill();
        let _ = kid.wait();
    }

    #[test]
    fn self_init_and_system_are_refused() {
        let plat = LinuxPlatform::new();
        let me = plat.current_pid();
        let mut g = group_of(&plat, &[me]);
        if let Some(init) = plat.process(1) {
            g.members.push(Member {
                info: init,
                mem: Default::default(),
            });
        }
        let p = plan(&plat, &g, None, false);
        assert_eq!(p.allowed().count(), 0);
        assert!(
            p.targets[0]
                .refusal
                .as_ref()
                .unwrap()
                .contains("fagia itself")
        );
        g.system = true;
        g.members.truncate(1);
        let p = plan(&plat, &g, None, false);
        assert!(p.targets[0].refusal.is_some());
    }
}
