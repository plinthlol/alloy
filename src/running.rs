// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// global tracking for running minecraft. Arc<Mutex<>> because the
// launch/monitor tasks live on separate tokio threads while the TUI
// render loop reads state every frame.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq)]
pub enum RunState {
    Authenticating,
    Starting,
    Running,
    Crashed(Option<i32>),
    // process alive (pidfile verified) but not forked by *this* alloysh —
    // a previous session spawned it detached (see launch/mod.rs) and then
    // died while minecraft kept running. we never forked it, so no wait(),
    // no live logs, no auto exit detection. can still kill it (send_kill)
    // via a raw signal instead of the oneshot-channel dance.
    Orphaned(u32),
}

pub static RUNNING: LazyLock<Arc<Mutex<HashMap<String, RunState>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// oneshot channels so send_kill can tell a launch task to kill its child.
type KillSenders = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>;
pub static KILL_SENDERS: LazyLock<KillSenders> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// one tiny file per running instance holding just its pid, so a later
// alloysh (after a restart/crash) can rediscover still-running instances
// even though its in-memory maps started empty. lives under the config
// dir — launcher bookkeeping, not instance content.

fn pid_dir() -> PathBuf {
    crate::config::get_config_path().join("running")
}

fn pid_file_path(name: &str) -> PathBuf {
    pid_dir().join(format!("{name}.pid"))
}

pub fn write_pid_file(name: &str, pid: u32) {
    let dir = pid_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("Failed to create pidfile dir {}: {}", dir.display(), e);
        return;
    }
    if let Err(e) = std::fs::write(pid_file_path(name), pid.to_string()) {
        tracing::warn!("Failed to write pidfile for '{}': {}", name, e);
    }
}

pub fn remove_pid_file(name: &str) {
    let path = pid_file_path(name);
    if let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("Failed to remove pidfile {}: {}", path.display(), e);
    }
}

// best-effort liveness: kill(pid, 0) sends nothing, just validates the pid
// exists (EPERM = exists but not ours, ESRCH = gone). we don't check it's
// the *same* process — pid-reuse in that narrow window is rare enough to
// ignore.
#[cfg(unix)]
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) sends no signal, just validates the pid.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    // ESRCH ("no such process") is the only errno that actually means the
    // pid is gone; anything else (most commonly EPERM, if the pid exists but
    // belongs to another user) still means it's alive.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    // no libc signal-0 on windows; ask tasklist. best-effort, same spirit.
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|line| line.contains(&pid.to_string()))
        })
        .unwrap_or(false)
}

// startup: repopulate RUNNING from pidfiles pointing at live processes, so
// previously-spawned instances show as running (and killable) again. stale
// pidfiles get cleaned up along the way.
pub fn reconcile_orphans() {
    let dir = pid_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pid") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };

        let alive_pid = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|&pid| pid_is_alive(pid));

        match alive_pid {
            Some(pid) => {
                tracing::info!(
                    "Reconciled orphaned instance '{}' still running as pid {}",
                    name,
                    pid
                );
                set_state(name, RunState::Orphaned(pid));
            }
            None => {
                tracing::debug!("Cleaning up stale pidfile for '{}'", name);
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

pub fn set_state(name: &str, state: RunState) {
    if let Ok(mut map) = RUNNING.lock() {
        map.insert(name.to_string(), state);
        crate::tui::request_redraw();
    }
}

pub fn remove(name: &str) {
    if let Ok(mut map) = RUNNING.lock() {
        map.remove(name);
        crate::tui::request_redraw();
    }
}

#[must_use]
pub fn get(name: &str) -> Option<RunState> {
    RUNNING.lock().ok().and_then(|map| map.get(name).cloned())
}

/// true while the named instance has a live session: authenticating,
/// starting, running, or orphaned. this is exactly the set of states for
/// which the Esc kill popup is reachable — mid-auth Enter cancels the
/// pending launch, the rest signal the child (or orphaned pid).
#[must_use]
pub fn is_alive(name: &str) -> bool {
    matches!(
        get(name),
        Some(RunState::Authenticating)
            | Some(RunState::Starting)
            | Some(RunState::Running)
            | Some(RunState::Orphaned(_))
    )
}

#[must_use]
pub fn has_active() -> bool {
    RUNNING.lock().is_ok_and(|map| {
        map.values().any(|state| {
            matches!(
                state,
                RunState::Authenticating
                    | RunState::Starting
                    | RunState::Running
                    | RunState::Orphaned(_)
            )
        })
    })
}

// a play session ended. lands on the UiEvent bus so the TUI event loop
// persists it — the child monitor never writes config files directly.
pub fn push_last_played(name: &str, time: DateTime<Utc>) {
    crate::tui::events::emit(crate::tui::events::UiEvent::LastPlayed(
        name.to_string(),
        time,
    ));
}

// polls every Orphaned pid and clears any that died — the only way we
// learn an orphan exited (we didn't fork it, so no wait()). without this,
// a detached instance would sit "running" forever and last_played would
// stay frozen.
pub fn reap_dead_orphans() -> Vec<String> {
    let dead: Vec<String> = RUNNING
        .lock()
        .map(|map| {
            map.iter()
                .filter_map(|(name, state)| match state {
                    RunState::Orphaned(pid) if !pid_is_alive(*pid) => Some(name.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    for name in &dead {
        tracing::info!("Orphaned instance '{}' is no longer running", name);
        remove_pid_file(name);
        remove(name);
        push_last_played(name, Utc::now());
    }

    dead
}

pub fn register_kill(name: &str, tx: tokio::sync::oneshot::Sender<()>) {
    if let Ok(mut map) = KILL_SENDERS.lock() {
        map.insert(name.to_string(), tx);
    }
}

pub fn send_kill(name: &str) -> bool {
    if let Ok(mut map) = KILL_SENDERS.lock()
        && let Some(tx) = map.remove(name)
    {
        let _ = tx.send(());
        return true;
    }

    // no sender = nothing running under this name, or an Orphaned instance
    // we never forked (no monitor task to signal). fall back to killing the
    // raw pid; we can't wait() on it either, so this is optimistic — fire
    // the signal, assume it worked, clear state.
    if let Some(RunState::Orphaned(pid)) = get(name) {
        tracing::info!("[{}] Kill requested for orphaned pid {}", name, pid);
        #[cfg(unix)]
        // SAFETY: same kill() the `kill` command shells out to, minus the
        // extra process spawn.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .output();
        }
        remove_pid_file(name);
        remove(name);
        return true;
    }

    false
}

pub fn cleanup_kill_sender(name: &str) {
    if let Ok(mut map) = KILL_SENDERS.lock() {
        map.remove(name);
    }
}

pub fn rename_tracked(old_name: &str, new_name: &str) {
    if let Ok(mut map) = RUNNING.lock()
        && let Some(state) = map.remove(old_name)
    {
        map.insert(new_name.to_string(), state);
    }
    if let Ok(mut map) = KILL_SENDERS.lock()
        && let Some(tx) = map.remove(old_name)
    {
        map.insert(new_name.to_string(), tx);
    }
    let old_path = pid_file_path(old_name);
    if old_path.exists() {
        let dir = pid_dir();
        if std::fs::create_dir_all(&dir).is_ok() {
            if let Err(e) = std::fs::rename(&old_path, pid_file_path(new_name)) {
                tracing::warn!(
                    "Failed to move pidfile {} for rename: {}",
                    old_path.display(),
                    e
                );
            }
        }
    }
    crate::tui::request_redraw();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_alive_true_for_live_states() {
        for (suffix, state) in [
            ("auth", RunState::Authenticating),
            ("starting", RunState::Starting),
            ("running", RunState::Running),
            ("orphan", RunState::Orphaned(u32::MAX - 3)),
        ] {
            let name = format!("run_test_alive_{suffix}");
            set_state(&name, state);
            assert!(is_alive(&name), "{suffix} should count as alive");
            remove(&name);
        }
    }

    #[test]
    fn is_alive_false_for_idle_states() {
        assert!(!is_alive("run_never_set_alive_xyz"));
        set_state("run_test_alive_crash", RunState::Crashed(Some(1)));
        assert!(!is_alive("run_test_alive_crash"));
        remove("run_test_alive_crash");
    }

    #[test]
    fn set_and_get_state() {
        set_state("run_test_1", RunState::Starting);
        assert_eq!(get("run_test_1"), Some(RunState::Starting));
    }

    #[test]
    fn get_missing_returns_none() {
        assert_eq!(get("run_never_set_xyz"), None);
    }

    #[test]
    fn remove_clears_state() {
        set_state("run_test_2", RunState::Running);
        remove("run_test_2");
        assert_eq!(get("run_test_2"), None);
    }

    #[test]
    fn set_state_overwrites() {
        set_state("run_test_3", RunState::Starting);
        set_state("run_test_3", RunState::Running);
        assert_eq!(get("run_test_3"), Some(RunState::Running));
    }

    #[test]
    fn crashed_state_stores_exit_code() {
        set_state("run_test_crash", RunState::Crashed(Some(1)));
        assert_eq!(get("run_test_crash"), Some(RunState::Crashed(Some(1))));
    }

    // push_last_played now lands on the UiEvent bus instead of a local
    // queue; in unit tests the bus is uninitialized so the emit is a no-op.
    // this just guards against a panic on that hot path (kill/reap/exit).
    #[test]
    fn push_last_played_without_bus_does_not_panic() {
        push_last_played("run_test_lp", Utc::now());
    }

    #[test]
    fn send_kill_returns_false_for_missing() {
        assert!(!send_kill("run_never_registered_xyz"));
    }

    #[test]
    fn register_and_send_kill() {
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        register_kill("run_test_kill", tx);
        assert!(send_kill("run_test_kill"));
        let _ = rx.try_recv();
    }

    #[test]
    fn cleanup_kill_sender_removes() {
        let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
        register_kill("run_test_cleanup", tx);
        cleanup_kill_sender("run_test_cleanup");
        assert!(!send_kill("run_test_cleanup"));
    }

    #[test]
    fn reap_dead_orphans_leaves_alive_pid_alone() {
        // our own pid is alive by definition.
        set_state("run_test_reap_alive", RunState::Orphaned(std::process::id()));
        let reaped = reap_dead_orphans();
        assert!(!reaped.iter().any(|n| n == "run_test_reap_alive"));
        assert_eq!(get("run_test_reap_alive"), Some(RunState::Orphaned(std::process::id())));
        remove("run_test_reap_alive");
    }

    #[test]
    fn reap_dead_orphans_clears_dead_pid() {
        // an implausibly large pid that shouldn't exist on any real system -
        // same best-effort assumption pid_is_alive's own doc comment makes.
        set_state("run_test_reap_dead", RunState::Orphaned(u32::MAX - 1));
        let reaped = reap_dead_orphans();
        assert!(reaped.iter().any(|n| n == "run_test_reap_dead"));
        assert_eq!(get("run_test_reap_dead"), None);
    }

    #[test]
    fn send_kill_on_orphan_clears_state() {
        set_state("run_test_orphan_kill", RunState::Orphaned(u32::MAX - 2));
        assert!(send_kill("run_test_orphan_kill"));
        assert_eq!(get("run_test_orphan_kill"), None);
    }
}
