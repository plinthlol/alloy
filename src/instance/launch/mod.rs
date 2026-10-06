// SPDX-FileCopyrightText: 2026 Constantin Bauer
// SPDX-License-Identifier: GPL-3.0-only

// builds the java command and spawns minecraft: classpath, auth injection,
// log capture. loader-specific patches live in submodules (e.g. lwjgl3ify).

pub(crate) mod parser;
mod patches;

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::auth::AccountType;
use crate::instance::models::{InstanceConfig, ModLoader};
use crate::launch_profile::model::LaunchProfile;
use crate::launch_profile::rules::{self, FeatureSet, RuleContext};
use crate::launch_profile::templates::TemplateContext;
use crate::launch_profile::{render, resolve, system};

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("Version metadata not found: {0}. Re-create the instance to fix this.")]
    MetaNotFound(String),
    #[error("Profile error: {0}")]
    Parse(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("This instance requires Java {required}, but alloy is using Java {detected}: {java}")]
    JavaTooOld {
        java: String,
        required: u32,
        detected: u32,
    },
    #[error("This instance requires Java {required}, but alloy could not check {java}: {reason}")]
    JavaCheckFailed {
        java: String,
        required: u32,
        reason: String,
    },
    #[error("{0}")]
    Auth(String),
}

fn build_game_args(
    profile: &LaunchProfile,
    rule_ctx: &RuleContext<'_>,
    template_ctx: &TemplateContext<'_>,
) -> Result<(Vec<String>, Vec<String>), LaunchError> {
    let rendered = render::render_args(profile, rule_ctx, template_ctx)
        .map_err(|e| LaunchError::Parse(format!("Failed to render args: {e}")))?;
    Ok((rendered.jvm, rendered.game))
}

// finds a real, loadable path (or dlopen-able name) for the system's glfw
// shared library. tries the unversioned name first (works when a -dev
// package is installed, or on distros that ship it directly), then falls
// back to the common runtime-only sonames, then asks the dynamic linker's
// own cache (`ldconfig -p`) in case the distro uses a name/path we didn't
// think to guess. returns None only if nothing usable was found anywhere.
fn resolve_system_glfw() -> Option<String> {
    const LIB_DIRS: &[&str] = &[
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/local/lib",
    ];
    // newest/most common soname first; .so.1 covers the (much rarer)
    // older GLFW 2.x packaging some distros still carry.
    const CANDIDATE_NAMES: &[&str] = &["libglfw.so", "libglfw.so.3", "libglfw.so.1"];

    for dir in LIB_DIRS {
        for name in CANDIDATE_NAMES {
            let path = Path::new(dir).join(name);
            if path.is_file() {
                return Some(path.to_string_lossy().into_owned());
            }
        }
    }

    // not in any of the usual spots (e.g. a distro with a non-standard
    // multiarch layout) — ask the linker cache directly rather than
    // giving up. `ldconfig -p` lists every shared lib it knows about with
    // its full resolved path; grep for glfw and take the first hit.
    let output = std::process::Command::new("ldconfig").arg("-p").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    listing
        .lines()
        .find(|line| line.contains("libglfw.so"))
        .and_then(|line| line.rsplit("=> ").next())
        .map(|path| path.trim().to_string())
}

async fn check_java_version(java: &str, required: Option<u32>) -> Result<(), LaunchError> {
    let Some(required) = required.filter(|major| *major > 0) else {
        return Ok(());
    };

    let output = tokio::process::Command::new(java)
        .arg("-version")
        .output()
        .await
        .map_err(|e| LaunchError::JavaCheckFailed {
            java: java.to_owned(),
            required,
            reason: e.to_string(),
        })?;

    let version_text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let detected = crate::net::parse_java_major_version(&version_text).ok_or_else(|| {
        LaunchError::JavaCheckFailed {
            java: java.to_owned(),
            required,
            reason: format!("could not parse `java -version` output: {version_text:?}"),
        }
    })?;

    if detected < required {
        return Err(LaunchError::JavaTooOld {
            java: java.to_owned(),
            required,
            detected,
        });
    }

    Ok(())
}

// resolved auth for the launch. borrowed strs so callers can pass owned
// strings or slices without allocating.
#[derive(Debug, Clone)]
pub struct LaunchAuth<'a> {
    pub username: &'a str,
    pub uuid: &'a str,
    pub token: &'a str,
    // "msa" for Microsoft, "legacy" for offline; mirrors Mojang's user_type.
    pub user_type: &'a str,
}

// everything the spawner needs to run java. assembled by
// build_launch_invocation, consumed by launch(). public so integration
// tests can assert on the rendered command without spawning a process.
#[derive(Debug, Clone)]
pub struct LaunchInvocation {
    pub java: String,
    pub jvm_args: Vec<String>,
    pub classpath: Vec<PathBuf>,
    pub classpath_string: String,
    pub main_class: String,
    pub extra_args: Vec<String>,
    pub game_args: Vec<String>,
    pub working_dir: PathBuf,
}

// fully-resolves a java invocation for an instance: reads meta.json and
// the loader profile, walks inheritsFrom, applies patches, renders
// templates. all I/O except auth and spawning happens here.
pub async fn build_launch_invocation(
    config: &InstanceConfig,
    instances_dir: &Path,
    meta_dir: &Path,
    auth: &LaunchAuth<'_>,
) -> Result<LaunchInvocation, LaunchError> {
    let instance_dir = instances_dir.join(&config.name);
    let minecraft_dir = instance_dir.join(".minecraft");

    let meta_path = meta_dir
        .join("versions")
        .join(&config.game_version)
        .join("meta.json");
    if !meta_path.exists() {
        return Err(LaunchError::MetaNotFound(meta_path.display().to_string()));
    }
    let meta: LaunchProfile = serde_json::from_slice(&tokio::fs::read(&meta_path).await?)?;

    let current_features = FeatureSet::default();
    let host_os_version = system::mojang_os_version();
    let rule_ctx = RuleContext {
        os_name: system::mojang_os_name(),
        os_version: &host_os_version,
        arch: system::mojang_arch_name(),
        features: &current_features,
    };

    let asset_index_id = meta
        .asset_index
        .as_ref()
        .map(|ai| ai.id.clone())
        .unwrap_or_default();

    let lib_dir = meta_dir.join("libraries");

    let lv = config.loader_version.as_deref().unwrap_or("unknown");
    let profile_filename = match config.loader {
        ModLoader::Vanilla => None,
        ModLoader::Fabric => Some(format!("fabric-{}-{}.json", config.game_version, lv)),
        ModLoader::Quilt => Some(format!("quilt-{}-{}.json", config.game_version, lv)),
        ModLoader::Forge => Some(format!("forge-{}-{}.json", config.game_version, lv)),
        ModLoader::NeoForge => Some(format!("neoforge-{}.json", lv)),
    };

    // load the loader profile (if any) and resolve inheritsFrom against
    // vanilla; profiles that omit it (fabric, quilt, etc.) get it set
    // explicitly below so resolve() walks the chain. no loader = vanilla meta.
    let merged_profile: LaunchProfile = if let Some(filename) = &profile_filename {
        let profile_path = meta_dir.join("loader-profiles").join(filename);
        if !profile_path.exists() {
            return Err(LaunchError::MetaNotFound(
                profile_path.display().to_string(),
            ));
        }
        let mut loader_profile: LaunchProfile =
            serde_json::from_slice(&tokio::fs::read(&profile_path).await?)?;

        // any loader profile that omits inheritsFrom still needs to be
        // layered over vanilla. set the inherit explicitly so resolve()
        // walks the chain.
        if loader_profile.inherits_from.is_none() {
            loader_profile.inherits_from = Some(config.game_version.clone());
        }

        resolve::resolve(loader_profile, meta_dir)
            .await
            .map_err(|e| LaunchError::Parse(format!("Failed to resolve loader profile: {e}")))?
    } else {
        meta.clone()
    };

    let main_class = merged_profile
        .main_class
        .clone()
        .ok_or_else(|| LaunchError::Parse("merged profile missing mainClass".into()))?;

    // rebuild the classpath from the merged profile. vanilla libs carry
    // downloads.artifact.path and live in meta_dir/libraries/; loader-style
    // libs have only a maven coord, and forge/neoforge drop some into
    // <instance>/.minecraft/libraries/, so check there first.
    let has_local_libs = matches!(config.loader, ModLoader::Forge | ModLoader::NeoForge);
    let local_lib_dir = minecraft_dir.join("libraries");
    let library_directory = if has_local_libs {
        &local_lib_dir
    } else {
        &lib_dir
    };

    let mut classpath: Vec<PathBuf> = Vec::new();
    for lib in &merged_profile.libraries {
        if let Some(rules) = &lib.rules
            && !rules::evaluate(rules, &rule_ctx)
        {
            continue;
        }

        // resolve a relative path for this library. prefer downloads.artifact.path
        // when present (vanilla-style), fall back to maven_coord_to_path(name)
        // for loader-style entries that only have a coord.
        let rel: PathBuf = match lib
            .downloads
            .as_ref()
            .and_then(|d| d.artifact.as_ref())
            .map(|a| PathBuf::from(&a.path))
            .or_else(|| crate::net::maven_coord_to_path(&lib.name).map(PathBuf::from))
        {
            Some(p) => p,
            None => continue,
        };

        // forge/neoforge stash some libs (notably the bootstrap one) in the
        // instance's .minecraft/libraries/ rather than the shared cache —
        // check there first regardless of downloads.artifact.
        if has_local_libs {
            let in_local = local_lib_dir.join(&rel);
            if in_local.exists() {
                classpath.push(in_local);
                continue;
            }
        }
        classpath.push(lib_dir.join(rel));
    }

    classpath.push(
        meta_dir
            .join("versions")
            .join(&config.game_version)
            .join(format!("{}.jar", config.game_version)),
    );

    // apply loader-specific patches (lwjgl3ify for old forge on java 9+)
    let (patch_jvm_args, main_class, extra_args) = if matches!(config.loader, ModLoader::Forge) {
        match patches::apply(&minecraft_dir, &lib_dir, &mut classpath).await {
            Some(p) => (p.jvm_args, p.main_class, p.extra_args),
            None => (Vec::new(), main_class, Vec::new()),
        }
    } else {
        (Vec::new(), main_class, Vec::new())
    };

    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp_str = classpath
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep);

    // java resolution: instance override > global setting > auto-detect
    let java = config
        .java_path
        .clone()
        .or_else(|| {
            crate::config::SETTINGS
                .paths
                .effective_java_path()
                .map(str::to_owned)
        })
        .unwrap_or_else(crate::net::detect_java_path);

    check_java_version(
        &java,
        merged_profile
            .java_version
            .as_ref()
            .map(|version| version.major_version),
    )
    .await?;

    let assets_root = meta_dir.join("assets");
    let natives_dir = meta_dir
        .join("versions")
        .join(&config.game_version)
        .join("natives");
    let version_type = merged_profile.type_.as_deref().unwrap_or("release");
    let template_ctx = TemplateContext {
        library_directory,
        classpath_separator: sep,
        version_name: &config.game_version,
        version_type,
        natives_directory: &natives_dir,
        classpath: &cp_str,
        game_directory: &minecraft_dir,
        assets_root: &assets_root,
        assets_index_name: &asset_index_id,
        auth_player_name: auth.username,
        auth_uuid: auth.uuid,
        auth_access_token: auth.token,
        auth_xuid: "0",
        user_type: auth.user_type,
        user_properties: "{}",
        launcher_name: "alloy",
        launcher_version: env!("CARGO_PKG_VERSION"),
        clientid: "0",
    };

    let (upstream_jvm_args, game_args) =
        build_game_args(&merged_profile, &rule_ctx, &template_ctx)?;

    let mut jvm_args: Vec<String> = vec![
        format!("-Xms{}", config.memory_min.as_deref().unwrap_or("512M")),
        format!("-Xmx{}", config.memory_max.as_deref().unwrap_or("2G")),
    ];
    jvm_args.extend(patch_jvm_args);
    jvm_args.extend(upstream_jvm_args);
    jvm_args.extend(config.jvm_args.clone());
    if config.use_system_glfw && cfg!(target_os = "linux") {
        // let lwjgl dlopen the system's glfw (e.g. wayland builds) instead
        // of the bundled natives. lwjgl's Library.loadNative does a plain
        // dlopen() on whatever name we give it here — it does NOT try
        // versioned fallbacks. plenty of distros (Debian/Ubuntu among
        // them) only ship the versioned runtime lib, libglfw.so.3, in the
        // package that actually provides libglfw at runtime; the
        // unversioned libglfw.so symlink only exists if the -dev/-devel
        // package is also installed. passing the bare "libglfw.so" name
        // then fails with `UnsatisfiedLinkError: Failed to locate library:
        // libglfw.so` even though a perfectly usable glfw is on the
        // system — this is a known report against other launchers doing
        // the same thing (e.g. PolyMC#1544). resolve an actual path
        // (versioned or not) instead of guessing the unversioned name.
        match resolve_system_glfw() {
            Some(libname) => jvm_args.push(format!("-Dorg.lwjgl.glfw.libname={libname}")),
            None => {
                tracing::warn!(
                    "System GLFW override is on but no libglfw(.so[.N]) was found; \
                     falling back to alloy's bundled natives"
                );
            }
        }
    }

    Ok(LaunchInvocation {
        java,
        jvm_args,
        classpath,
        classpath_string: cp_str,
        main_class,
        extra_args,
        game_args,
        working_dir: minecraft_dir,
    })
}

// resolves auth, builds the invocation, spawns java. thin wrapper: token
// refresh, spawn, supervision — the heavy lifting lives in
// build_launch_invocation.
pub async fn launch(
    config: &InstanceConfig,
    instances_dir: &Path,
    meta_dir: &Path,
) -> Result<(), LaunchError> {
    let name = config.name.clone();

    // resolve auth credentials, refreshing the microsoft token if needed.
    let mut account_store = crate::auth::AccountStore::load();
    let Some(acc) = account_store.active_account().cloned() else {
        return Err(LaunchError::Auth("No account selected".to_owned()));
    };

    // note: offline accounts no longer require a microsoft account to have
    // ever existed in order to launch. account creation still requires the
    // very first account added to be microsoft (see AccountStore::has_any_account
    // and its use in tui/widgets/account.rs), but once any account exists,
    // launching with an offline account is unrestricted.

    let (token, new_refresh, new_expires) = match acc.account_type {
        AccountType::Microsoft => match crate::auth::refresh_and_get_token(&acc).await {
            Ok(triple) => triple,
            Err(e) => return Err(LaunchError::Auth(format!("Authentication failed: {e}"))),
        },
        AccountType::Offline => ("0".to_string(), None, None),
    };

    if let Some(stored) = account_store
        .accounts
        .iter_mut()
        .find(|a| a.uuid == acc.uuid)
    {
        let mut changed = false;
        if let Some(new_rt) = new_refresh {
            stored.refresh_token = Some(new_rt);
            changed = true;
        }
        if let Some(expires) = new_expires {
            stored.cached_mc_token = Some(token.clone());
            stored.cached_mc_token_expires_at = Some(expires);
            changed = true;
        }
        if changed {
            account_store.save();
        }
    }

    let user_type = match acc.account_type {
        AccountType::Microsoft => "msa",
        AccountType::Offline => "legacy",
    };

    let auth = LaunchAuth {
        username: &acc.username,
        uuid: &acc.uuid,
        token: &token,
        user_type,
    };

    let invocation = build_launch_invocation(config, instances_dir, meta_dir, &auth).await?;
    tracing::debug!(
        "[{}] Prepared launch invocation: working_dir={} classpath_entries={} jvm_args={} extra_args={} game_args={} main_class={}",
        name,
        invocation.working_dir.display(),
        invocation.classpath.len(),
        invocation.jvm_args.len(),
        invocation.extra_args.len(),
        invocation.game_args.len(),
        invocation.main_class
    );
    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
    // the user may have cancelled (Esc kill popup) while auth was
    // resolving, which clears the Authenticating marker. bail quietly
    // instead of spawning — otherwise the launch would continue and
    // resurrect run state the UI already dropped.
    if !matches!(
        crate::running::get(&name),
        Some(crate::running::RunState::Authenticating)
    ) {
        tracing::info!("[{}] Launch cancelled before spawn; not starting", name);
        return Ok(());
    }
    crate::running::register_kill(&name, kill_tx);
    crate::running::set_state(&name, crate::running::RunState::Starting);
    tracing::info!(
        "[{}] Starting Minecraft ({} {})",
        name,
        config.game_version,
        config.loader
    );

    tracing::info!("[{}] Java: {}", name, invocation.java);
    tracing::info!("[{}] JVM args: {:?}", name, invocation.jvm_args);
    tracing::info!(
        "[{}] Classpath:\n{}",
        name,
        invocation
            .classpath
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    tracing::info!("[{}] Main class: {}", name, invocation.main_class);

    // spawn as a detached std::process::Command (not tokio) so the child
    // outlives alloysh: if we die, java gets adopted by PID 1 and keeps
    // running. unix: setsid() before exec gives it its own session, so no
    // SIGHUP on our exit. windows: CREATE_NEW_PROCESS_GROUP. we still pipe
    // stdout/stderr for live logs, but those tasks are fire-and-forget —
    // if we exit, the pipes break and they end naturally.

    let mut cmd = std::process::Command::new(&invocation.java);
    cmd.args(&invocation.jvm_args);
    cmd.arg("-cp").arg(&invocation.classpath_string);
    cmd.arg(&invocation.main_class);
    cmd.args(&invocation.extra_args);
    cmd.args(&invocation.game_args);
    cmd.current_dir(&invocation.working_dir);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // detach from parent process group so the child survives our exit
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs post-fork, pre-exec in the child. setsid()
        // detaches it from our process group so no SIGHUP when we exit —
        // standard daemonizing.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            crate::running::cleanup_kill_sender(&name);
            crate::running::remove(&name);
            tracing::error!("[{}] Failed to spawn Minecraft process: {}", name, e);
            return Err(LaunchError::Io(e));
        }
    };
    tracing::debug!("[{}] Spawned Minecraft process (detached, pid={})", name, child.id());
    crate::running::write_pid_file(&name, child.id());

    crate::running::set_state(&name, crate::running::RunState::Running);

    // last_played is stamped once, here at launch — the "last launched"
    // moment, not the session's end. stamping only at exit left running
    // sessions showing their *previous* session's time and lost sessions
    // that ended while the TUI was closed; a launch-time stamp survives
    // both (a session that dies while alloy is shut down was already
    // recorded when it started). persisted by the TUI event loop via
    // UiEvent::LastPlayed.
    crate::running::push_last_played(&name, chrono::Utc::now());

    // no session log of our own: minecraft's log4j already writes
    // .minecraft/logs/latest.log and rotates it (see log_files.rs). the
    // capture below only feeds the live TUI buffer and catches pre-log4j
    // JVM startup failures.
    let name_for_task = name.clone();

    // background task that babysits the child: capture stdout/stderr into
    // the parser -> live log viewer. fire-and-forget: if we exit the
    // runtime drops and this task dies, but java is already detached.
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        use tokio::sync::mpsc;
        use tokio::time::{Duration, sleep};

        use crate::instance::launch::parser::{LogStream, MinecraftLogParser};

        let (log_tx, mut log_rx) = mpsc::channel::<(LogStream, String)>(1024);
        let parser_name = name_for_task.clone();
        let parser_task = tokio::spawn(async move {
            let mut parser = MinecraftLogParser::new();
            let idle_flush = Duration::from_millis(150);

            loop {
                tokio::select! {
                    maybe_line = log_rx.recv() => {
                        match maybe_line {
                            Some((stream, line)) => {
                                for event in parser.push_line(stream, line) {
                                    emit_parsed_instance_log(&parser_name, event);
                                }
                            }
                            None => break,
                        }
                    }
                    _ = sleep(idle_flush), if parser.has_pending() => {
                        if let Some(event) = parser.flush() {
                            emit_parsed_instance_log(&parser_name, event);
                        }
                    }
                }
            }

            if let Some(event) = parser.flush() {
                emit_parsed_instance_log(&parser_name, event);
            }
        });

        // convert the std process pipes into tokio async reads
        if let Some(stdout) = child.stdout.take() {
            let tx = log_tx.clone();
            let reader = tokio::io::BufReader::new(pipe_to_tokio_file(stdout));
            let mut lines = reader.lines();
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send((LogStream::Stdout, line)).await.is_err() {
                        break;
                    }
                }
                tracing::trace!("Minecraft stdout capture task ended");
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let tx = log_tx.clone();
            let reader = tokio::io::BufReader::new(pipe_to_tokio_file(stderr));
            let mut lines = reader.lines();
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send((LogStream::Stderr, line)).await.is_err() {
                        break;
                    }
                }
                tracing::trace!("Minecraft stderr capture task ended");
            });
        }
        drop(log_tx);

        // wait for a natural exit or a TUI kill. spawn_blocking because
        // std::process::Child has no async wait. the pid is grabbed up
        // front: select! builds every branch's future before polling, so
        // the spawn_blocking closure moves `child` in immediately — using
        // it in the kill_rx arm would be a use-after-move.
        let child_pid = child.id();
        let (code, killed_by_user) = tokio::select! {
            _ = kill_rx => {
                tracing::info!("[{}] Kill requested, terminating process", name_for_task);
                terminate_process(child_pid);
                let mut died = false;
                for _ in 0..KILL_GRACE_POLLS {
                    if !crate::running::pid_is_alive(child_pid) {
                        died = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(KILL_POLL_MS)).await;
                }
                if !died {
                    tracing::warn!(
                        "[{}] Process still alive after graceful kill, escalating",
                        name_for_task
                    );
                    force_kill_process(child_pid);
                    for _ in 0..KILL_GRACE_POLLS {
                        if !crate::running::pid_is_alive(child_pid) {
                            died = true;
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(KILL_POLL_MS)).await;
                    }
                }
                if !died {
                    tracing::error!(
                        "[{}] Process survived kill attempt (pid {})",
                        name_for_task,
                        child_pid
                    );
                }
                (None, died)
            }
            result = tokio::task::spawn_blocking(move || {
                child.wait().ok().and_then(|s| s.code())
            }) => {
                match result {
                    Ok(code) => (code, false),
                    Err(_) => (None, false),
                }
            }
        };
        let _ = parser_task.await;
        tracing::info!("[{}] Exited with code {:?}", name_for_task, code);
        crate::running::remove_pid_file(&name_for_task);

        if code == Some(0) || killed_by_user {
            crate::running::remove(&name_for_task);
            tracing::debug!(
                "[{}] Cleared running state after normal exit (killed_by_user={})",
                name_for_task,
                killed_by_user
            );
        } else {
            crate::running::set_state(&name_for_task, crate::running::RunState::Crashed(code));
            crate::tui::error_buffer::push_error(crate::tui::error_buffer::ErrorEvent {
                id: 0,
                level: tracing::Level::ERROR,
                message: match code {
                    Some(code) => {
                        format!("Minecraft '{}' crashed with exit code {}", name_for_task, code)
                    }
                    None => format!("Minecraft '{}' crashed without an exit code", name_for_task),
                },
                pushed_at: std::time::Instant::now(),
            });
        }

        crate::running::cleanup_kill_sender(&name_for_task);
    });

    Ok(())
}

// the child's stdout/stderr come back as ChildStdout/ChildStderr, which
// aren't std::fs::File, so tokio::fs::File::from_std can't take them
// directly. both implement OwnedFd (unix) / OwnedHandle (windows), so
// round-trip through that to hand tokio a real File.
#[cfg(unix)]
fn pipe_to_tokio_file<T: Into<std::os::fd::OwnedFd>>(pipe: T) -> tokio::fs::File {
    tokio::fs::File::from_std(std::fs::File::from(pipe.into()))
}

#[cfg(windows)]
fn pipe_to_tokio_file<T: Into<std::os::windows::io::OwnedHandle>>(pipe: T) -> tokio::fs::File {
    tokio::fs::File::from_std(std::fs::File::from(pipe.into()))
}

const KILL_GRACE_POLLS: usize = 50;
const KILL_POLL_MS: u64 = 100;

#[cfg(unix)]
fn terminate_process(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(windows)]
fn terminate_process(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .output();
}

#[cfg(unix)]
fn force_kill_process(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn force_kill_process(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
}

fn emit_parsed_instance_log(
    instance_name: &str,
    event: crate::instance::launch::parser::ParsedLogEvent,
) {
    let text = event.lines.join("\n");
    match event.level {
        crate::instance::launch::parser::LogLevel::Error => {
            tracing::error!(target: "mc_instance", "[{}] {}", instance_name, text);
        }
        crate::instance::launch::parser::LogLevel::Warn => {
            tracing::warn!(target: "mc_instance", "[{}] {}", instance_name, text);
        }
        crate::instance::launch::parser::LogLevel::Info => {
            tracing::info!(target: "mc_instance", "[{}] {}", instance_name, text);
        }
        crate::instance::launch::parser::LogLevel::Debug => {
            tracing::debug!(target: "mc_instance", "[{}] {}", instance_name, text);
        }
        crate::instance::launch::parser::LogLevel::Trace => {
            tracing::trace!(target: "mc_instance", "[{}] {}", instance_name, text);
        }
    }
    crate::instance_logs::push_event(instance_name, event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_game_args_renders_upstream_arguments() {
        use crate::launch_profile::model::{Argument, Arguments, LaunchProfile};
        use crate::launch_profile::rules::{FeatureSet, RuleContext};
        use TemplateContext;
        use std::path::PathBuf;

        let lib = PathBuf::from("/m/libraries");
        let nat = PathBuf::from("/m/natives");
        let game_dir = PathBuf::from("/i/.minecraft");
        let assets = PathBuf::from("/m/assets");

        let template_ctx = TemplateContext {
            library_directory: &lib,
            classpath_separator: ":",
            version_name: "1.20.1",
            natives_directory: &nat,
            classpath: "a.jar:b.jar",
            game_directory: &game_dir,
            assets_root: &assets,
            assets_index_name: "5",
            auth_player_name: "Player",
            auth_uuid: "00000000-0000-0000-0000-000000000000",
            auth_access_token: "token",
            auth_xuid: "0",
            user_type: "msa",
            user_properties: "{}",
            launcher_name: "alloy",
            launcher_version: "test",
            clientid: "0",
            version_type: "release",
        };
        let features = FeatureSet::default();
        let rule_ctx = RuleContext {
            os_name: "linux",
            os_version: "6.0",
            arch: "x86_64",
            features: &features,
        };

        let profile = LaunchProfile {
            id: "1.20.1".into(),
            inherits_from: None,
            main_class: Some("net.minecraft.client.main.Main".into()),
            libraries: Vec::new(),
            arguments: Some(Arguments {
                game: vec![
                    Argument::Literal("--username".into()),
                    Argument::Literal("${auth_player_name}".into()),
                ],
                jvm: vec![Argument::Literal(
                    "-Djava.library.path=${natives_directory}".into(),
                )],
            }),
            ..Default::default()
        };

        let (jvm, game_args) = build_game_args(&profile, &rule_ctx, &template_ctx).unwrap();
        assert_eq!(jvm, vec!["-Djava.library.path=/m/natives"]);
        assert_eq!(game_args, vec!["--username", "Player"]);
    }
}