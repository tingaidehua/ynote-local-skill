use crate::console::{self, ConsoleControl, RuntimeConfig, SyncKind};
use crate::syncer;
use crate::web;
use anyhow::{Context, Result, bail};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::time::Instant;

pub const TASK_NAME: &str = "YnoteCliMirror";
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

pub struct RunOptions {
    pub data_root: Option<PathBuf>,
    pub account: Option<String>,
    pub output: PathBuf,
    pub interval: u64,
    pub jitter: u64,
    pub bind: String,
    pub port: u16,
    pub local_only: bool,
}

pub async fn run(options: RunOptions) -> Result<()> {
    let RunOptions {
        data_root,
        account,
        output,
        interval,
        jitter,
        bind,
        port,
        local_only,
    } = options;
    syncer::validate_interval(interval)?;
    if jitter > console::MAX_CLOUD_JITTER_SECONDS {
        bail!(
            "cloud jitter must be at most {} seconds",
            console::MAX_CLOUD_JITTER_SECONDS
        );
    }
    validate_loopback(&bind)?;
    let base_runtime_config = RuntimeConfig {
        cloud_enabled: !local_only,
        cloud_interval_seconds: interval,
        cloud_jitter_seconds: jitter,
        local_debounce_milliseconds: console::DEFAULT_LOCAL_DEBOUNCE_MILLISECONDS,
        local_max_batch_seconds: console::DEFAULT_LOCAL_MAX_BATCH_SECONDS,
        web_status_poll_milliseconds: console::DEFAULT_WEB_POLL_MILLISECONDS,
        bind: bind.clone(),
        port,
        output: output.clone(),
        executable: std::env::current_exe().context("resolve ynote-cli executable")?,
        startup_installed: task_status().is_ok(),
    };
    let mut runtime_config = console::load_runtime_config(&output, base_runtime_config)?;
    if local_only {
        runtime_config.cloud_enabled = false;
    }
    let initial_local_only = !runtime_config.cloud_enabled;
    let initial = tokio::task::spawn_blocking({
        let data_root = data_root.clone();
        let account = account.clone();
        let output = output.clone();
        move || syncer::refresh_once(data_root, account, &output, initial_local_only)
    })
    .await??;
    let source = initial.repo.source.clone();
    let initial_was_cloud = initial.summary.cloud.is_some();
    let initial_finished = initial.summary.finished_at_unix;
    let initial_summary = initial.summary.clone();
    let repo = Arc::new(RwLock::new(initial.repo));
    let status = Arc::new(RwLock::new(json!({
        "state": "waiting",
        "revision": 1,
        "lastSuccess": initial_summary,
        "lastCloudSuccess": initial_was_cloud.then_some(initial.summary),
        "lastCloudAttemptUnix": if initial_was_cloud { initial_finished } else { 0 },
        "failures": 0,
        "localWatch": "starting",
        "localRefreshCount": 0,
        "cloudRefreshCount": if initial_was_cloud { 1 } else { 0 },
        "manualRequestPending": false
    })));
    let sync_gate = Arc::new(Mutex::new(()));
    let (config_tx, mut config_rx) = tokio::sync::watch::channel(runtime_config.clone());
    let (manual_tx, mut manual_rx) = mpsc::channel(1);
    let control = ConsoleControl::daemon(runtime_config, config_tx, manual_tx);
    let refresh_context = RefreshContext {
        gate: sync_gate.clone(),
        repo: repo.clone(),
        status: status.clone(),
        source: source.clone(),
        output: output.clone(),
    };

    let sync_status = status.clone();
    let cloud_refresh = refresh_context.clone();
    tokio::spawn(async move {
        let mut failures = 0u32;
        loop {
            let config = config_rx.borrow().clone();
            if !config.cloud_enabled {
                {
                    let mut value = sync_status
                        .write()
                        .unwrap_or_else(|error| error.into_inner());
                    value["cloudSchedule"] = json!("paused");
                    value["nextAttemptInSeconds"] = serde_json::Value::Null;
                }
                if config_rx.changed().await.is_err() {
                    break;
                }
                continue;
            }
            let delay = syncer::next_delay(
                config.cloud_interval_seconds,
                config.cloud_jitter_seconds,
                failures,
            );
            {
                let mut value = sync_status
                    .write()
                    .unwrap_or_else(|error| error.into_inner());
                value["cloudSchedule"] = json!("waiting");
                value["nextAttemptInSeconds"] = json!(delay.as_secs());
                value["state"] = json!("waiting");
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                changed = config_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    continue;
                }
            }
            match perform_refresh(&cloud_refresh, false, "syncing_cloud", "scheduled_cloud").await {
                Ok(()) => {
                    failures = 0;
                    let mut value = sync_status
                        .write()
                        .unwrap_or_else(|error| error.into_inner());
                    value["failures"] = json!(failures);
                    value
                        .as_object_mut()
                        .map(|object| object.remove("lastError"));
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    eprintln!("ynote-cli daemon sync failed: {error:#}");
                    let mut value = sync_status
                        .write()
                        .unwrap_or_else(|error| error.into_inner());
                    value["state"] = json!("backoff");
                    value["lastError"] = json!(error.to_string());
                    value["failures"] = json!(failures);
                }
            }
        }
    });
    let (watcher, mut events) = desktop_watcher(&source)?;
    {
        let mut value = status.write().unwrap_or_else(|error| error.into_inner());
        value["localWatch"] = json!("active");
    }
    let local_status = status.clone();
    let local_control = control.clone();
    let local_source = source.clone();
    let local_refresh = refresh_context.clone();
    tokio::spawn(async move {
        let _watcher = watcher;
        while let Some(event) = events.recv().await {
            let event = match event {
                Ok(event) if relevant_event(&event, &local_source) => event,
                Ok(_) => continue,
                Err(error) => {
                    eprintln!("ynote-cli local watcher error: {error}");
                    let mut value = local_status
                        .write()
                        .unwrap_or_else(|lock_error| lock_error.into_inner());
                    value["localWatch"] = json!("degraded");
                    value["lastLocalWatchError"] = json!(error.to_string());
                    continue;
                }
            };
            let _ = event;
            debounce_events(&mut events, &local_source, &local_control).await;
            match perform_refresh(
                &local_refresh,
                true,
                "refreshing_local",
                "desktop_filesystem",
            )
            .await
            {
                Ok(()) => {
                    let mut value = local_status
                        .write()
                        .unwrap_or_else(|error| error.into_inner());
                    value["localWatch"] = json!("active");
                    value
                        .as_object_mut()
                        .map(|object| object.remove("lastLocalWatchError"));
                }
                Err(error) => {
                    eprintln!("ynote-cli local refresh failed: {error:#}");
                    let mut value = local_status
                        .write()
                        .unwrap_or_else(|lock_error| lock_error.into_inner());
                    value["state"] = json!("waiting");
                    value["lastLocalWatchError"] = json!(error.to_string());
                }
            }
        }
    });

    let manual_status = status.clone();
    let manual_refresh = refresh_context;
    tokio::spawn(async move {
        while let Some(kind) = manual_rx.recv().await {
            {
                let mut value = manual_status
                    .write()
                    .unwrap_or_else(|error| error.into_inner());
                value["manualRequestPending"] = json!(false);
            }
            let (local_only, state, trigger) = match kind {
                SyncKind::Local => (true, "refreshing_local", "manual_local"),
                SyncKind::Cloud => (false, "syncing_cloud", "manual_cloud"),
            };
            if let Err(error) = perform_refresh(&manual_refresh, local_only, state, trigger).await {
                eprintln!("ynote-cli manual sync failed: {error:#}");
                let mut value = manual_status
                    .write()
                    .unwrap_or_else(|lock_error| lock_error.into_inner());
                value["state"] = json!("waiting");
                value["lastManualError"] = json!(error.to_string());
            }
        }
    });
    web::serve_shared(repo, status, control, &bind, port).await
}

#[derive(Clone)]
struct RefreshContext {
    gate: Arc<Mutex<()>>,
    repo: Arc<RwLock<crate::repository::Repository>>,
    status: Arc<RwLock<serde_json::Value>>,
    source: crate::model::SourceInfo,
    output: PathBuf,
}

async fn perform_refresh(
    context: &RefreshContext,
    local_only: bool,
    state: &'static str,
    trigger: &'static str,
) -> Result<()> {
    let _guard = context.gate.lock().await;
    {
        let mut value = context
            .status
            .write()
            .unwrap_or_else(|error| error.into_inner());
        value["state"] = json!(state);
        value["lastTrigger"] = json!(trigger);
        if !local_only {
            value["lastCloudAttemptUnix"] = json!(console::unix_now());
        }
    }
    let source = context.source.clone();
    let output = context.output.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        syncer::refresh_once(
            Some(source.data_root),
            Some(source.account),
            &output,
            local_only,
        )
    })
    .await
    .context("join synchronization worker")??;
    *context
        .repo
        .write()
        .unwrap_or_else(|error| error.into_inner()) = outcome.repo;
    let mut value = context
        .status
        .write()
        .unwrap_or_else(|error| error.into_inner());
    bump_revision(&mut value);
    value["state"] = json!("waiting");
    value["lastSuccess"] = json!(outcome.summary);
    if local_only {
        value["lastLocalSuccess"] = value["lastSuccess"].clone();
        value["localRefreshCount"] =
            json!(value["localRefreshCount"].as_u64().unwrap_or_default() + 1);
    } else {
        value["lastCloudSuccess"] = value["lastSuccess"].clone();
        value["cloudRefreshCount"] =
            json!(value["cloudRefreshCount"].as_u64().unwrap_or_default() + 1);
    }
    value
        .as_object_mut()
        .map(|object| object.remove("lastManualError"));
    Ok(())
}

fn desktop_watcher(
    source: &crate::model::SourceInfo,
) -> Result<(
    RecommendedWatcher,
    mpsc::UnboundedReceiver<notify::Result<Event>>,
)> {
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .context("create native filesystem watcher")?;
    watcher
        .watch(&source.data_root, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch {}", source.data_root.display()))?;
    for directory in [
        source.data_root.join("file"),
        source.data_root.join("resource"),
    ] {
        if directory.is_dir() {
            watcher
                .watch(&directory, RecursiveMode::Recursive)
                .with_context(|| format!("watch {}", directory.display()))?;
        }
    }
    Ok((watcher, receiver))
}

async fn debounce_events(
    events: &mut mpsc::UnboundedReceiver<notify::Result<Event>>,
    source: &crate::model::SourceInfo,
    control: &ConsoleControl,
) {
    let config = control.config();
    let started = Instant::now();
    let hard_deadline = started + Duration::from_secs(config.local_max_batch_seconds);
    let mut quiet_deadline = started + Duration::from_millis(config.local_debounce_milliseconds);
    loop {
        let deadline = if quiet_deadline < hard_deadline {
            quiet_deadline
        } else {
            hard_deadline
        };
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(Ok(event))) if relevant_event(&event, source) => {
                quiet_deadline = Instant::now()
                    + Duration::from_millis(control.config().local_debounce_milliseconds);
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
        if Instant::now() >= hard_deadline {
            break;
        }
    }
}

fn relevant_event(event: &Event, source: &crate::model::SourceInfo) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    let file_root = source.data_root.join("file");
    let resource_root = source.data_root.join("resource");
    event.paths.iter().any(|path| {
        path.starts_with(&file_root)
            || path.starts_with(&resource_root)
            || database_family(path, &source.database)
            || source
                .content_database
                .as_ref()
                .is_some_and(|database| database_family(path, database))
    })
}

fn database_family(path: &Path, database: &Path) -> bool {
    path == database
        || path == append_suffix(database, "-wal")
        || path == append_suffix(database, "-shm")
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    value.into()
}

fn bump_revision(value: &mut serde_json::Value) {
    value["revision"] = json!(value["revision"].as_u64().unwrap_or_default() + 1);
}

pub fn install(output: &Path, interval: u64, jitter: u64, port: u16) -> Result<String> {
    syncer::validate_interval(interval)?;
    let executable = std::env::current_exe().context("resolve ynote-cli executable")?;
    let command = format!(
        "\"{}\" daemon run --output \"{}\" --interval {} --jitter {} --port {}",
        executable.display(),
        output.display(),
        interval,
        jitter,
        port
    );
    let launcher = daemon_launcher_path()?;
    if let Some(parent) = launcher.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create daemon state directory {}", parent.display()))?;
    }
    let script = format!(
        "Set shell = CreateObject(\"WScript.Shell\")\r\nshell.Run \"{}\", 0, False\r\n",
        command.replace('"', "\"\"")
    );
    write_utf16(&launcher, &script)?;
    let registry_command = format!("wscript.exe \"{}\"", launcher.display());
    let result = Command::new("reg.exe")
        .args([
            "add",
            RUN_KEY,
            "/v",
            TASK_NAME,
            "/t",
            "REG_SZ",
            "/d",
            &registry_command,
            "/f",
        ])
        .output()
        .context("install current-user Run entry")?;
    if !result.status.success() {
        bail!(
            "current-user logon startup installation failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(format!(
        "Installed current-user HKCU Run startup via {}",
        launcher.display()
    ))
}

pub fn task_status() -> Result<String> {
    let result = Command::new("reg.exe")
        .args(["query", RUN_KEY, "/v", TASK_NAME])
        .output()
        .context("query current-user Run entry")?;
    if result.status.success() {
        return Ok(String::from_utf8_lossy(&result.stdout).to_string());
    }
    bail!("logon startup {TASK_NAME} is not installed")
}

pub fn uninstall() -> Result<String> {
    let result = Command::new("reg.exe")
        .args(["delete", RUN_KEY, "/v", TASK_NAME, "/f"])
        .output()
        .context("remove current-user Run entry")?;
    if !result.status.success() {
        bail!("logon startup {TASK_NAME} was not installed");
    }
    let launcher = daemon_launcher_path()?;
    if launcher.is_file() {
        fs::remove_file(&launcher).context("remove daemon launcher")?;
    }
    Ok("removed current-user HKCU Run startup".to_string())
}

pub fn validate_loopback(bind: &str) -> Result<()> {
    if !matches!(bind, "127.0.0.1" | "localhost" | "::1") {
        bail!(
            "refusing a non-loopback bind by default: {bind}; local notes may contain private data"
        );
    }
    Ok(())
}

fn daemon_launcher_path() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .context("LOCALAPPDATA and APPDATA are unavailable")?;
    Ok(daemon_launcher_path_from(&base))
}

fn daemon_launcher_path_from(base: &Path) -> PathBuf {
    base.join("ynote-cli").join("ynote-cli-daemon.vbs")
}

fn write_utf16(path: &Path, text: &str) -> Result<()> {
    let mut bytes = vec![0xff, 0xfe];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{daemon_launcher_path_from, desktop_watcher, relevant_event};
    use crate::model::SourceInfo;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn daemon_launcher_uses_user_state_directory() {
        let base = PathBuf::from(r"X:\fixture\AppData\Local");
        assert_eq!(
            daemon_launcher_path_from(&base),
            base.join("ynote-cli").join("ynote-cli-daemon.vbs")
        );
    }

    #[tokio::test]
    async fn native_watcher_detects_sqlite_wal_writes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("ynote-cli-watcher-{}-{nonce}", std::process::id()));
        fs::create_dir_all(root.join("file")).unwrap();
        fs::create_dir_all(root.join("resource")).unwrap();
        let database = root.join("account.db");
        fs::write(&database, b"fixture").unwrap();
        let source = SourceInfo {
            account: "fixture".to_string(),
            data_root: root.clone(),
            database: database.clone(),
            content_database: None,
            client_executable: PathBuf::from("ynote.exe"),
        };
        let (_watcher, mut events) = desktop_watcher(&source).unwrap();
        fs::write(root.join("account.db-wal"), b"changed").unwrap();
        let detected = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                if event
                    .as_ref()
                    .is_ok_and(|event| relevant_event(event, &source))
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        fs::remove_dir_all(&root).unwrap();
        assert!(detected);
    }
}
