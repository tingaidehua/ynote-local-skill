use crate::syncer;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};

pub const DEFAULT_LOCAL_DEBOUNCE_MILLISECONDS: u64 = 800;
pub const DEFAULT_LOCAL_MAX_BATCH_SECONDS: u64 = 5;
pub const DEFAULT_WEB_POLL_MILLISECONDS: u64 = 2_000;
pub const MAX_CLOUD_JITTER_SECONDS: u64 = 3_600;
pub const MIN_LOCAL_DEBOUNCE_MILLISECONDS: u64 = 100;
pub const MAX_LOCAL_DEBOUNCE_MILLISECONDS: u64 = 5_000;
pub const MIN_WEB_POLL_MILLISECONDS: u64 = 500;
pub const MAX_WEB_POLL_MILLISECONDS: u64 = 60_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub cloud_enabled: bool,
    pub cloud_interval_seconds: u64,
    pub cloud_jitter_seconds: u64,
    pub local_debounce_milliseconds: u64,
    pub local_max_batch_seconds: u64,
    pub web_status_poll_milliseconds: u64,
    pub bind: String,
    pub port: u16,
    pub output: PathBuf,
    pub executable: PathBuf,
    pub startup_installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigPatch {
    pub cloud_enabled: Option<bool>,
    pub cloud_interval_seconds: Option<u64>,
    pub cloud_jitter_seconds: Option<u64>,
    pub local_debounce_milliseconds: Option<u64>,
    pub web_status_poll_milliseconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncKind {
    Local,
    Cloud,
}

#[derive(Clone)]
pub struct ConsoleControl {
    config: Arc<RwLock<RuntimeConfig>>,
    config_tx: Option<watch::Sender<RuntimeConfig>>,
    sync_tx: Option<mpsc::Sender<SyncKind>>,
    sampler: Arc<Mutex<ResourceSampler>>,
    started_at_unix: u64,
}

impl ConsoleControl {
    pub fn daemon(
        config: RuntimeConfig,
        config_tx: watch::Sender<RuntimeConfig>,
        sync_tx: mpsc::Sender<SyncKind>,
    ) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            config_tx: Some(config_tx),
            sync_tx: Some(sync_tx),
            sampler: Arc::new(Mutex::new(ResourceSampler::new())),
            started_at_unix: unix_now(),
        }
    }

    pub fn static_server(config: RuntimeConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            config_tx: None,
            sync_tx: None,
            sampler: Arc::new(Mutex::new(ResourceSampler::new())),
            started_at_unix: unix_now(),
        }
    }

    pub fn config(&self) -> RuntimeConfig {
        self.config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn mutable(&self) -> bool {
        self.config_tx.is_some() && self.sync_tx.is_some()
    }

    pub fn update(&self, patch: ConfigPatch) -> Result<RuntimeConfig> {
        let Some(sender) = &self.config_tx else {
            bail!("runtime controls are unavailable in static serve mode");
        };
        let mut config = self
            .config
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let mut updated = config.clone();
        apply_patch(&mut updated, patch)?;
        persist_runtime_config(&updated)?;
        *config = updated.clone();
        let _ = sender.send(updated.clone());
        Ok(updated)
    }

    pub fn queue_sync(&self, kind: SyncKind) -> Result<()> {
        let Some(sender) = &self.sync_tx else {
            bail!("manual synchronization is unavailable in static serve mode");
        };
        sender.try_send(kind).map_err(|error| {
            anyhow::anyhow!("a manual synchronization is already queued: {error}")
        })?;
        if matches!(kind, SyncKind::Cloud)
            && let Some(config_sender) = &self.config_tx
        {
            let _ = config_sender.send(self.config());
        }
        Ok(())
    }

    pub fn metrics(&self) -> ProcessMetrics {
        self.sampler
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sample(self.started_at_unix)
    }
}

pub fn load_runtime_config(
    output: &std::path::Path,
    mut base: RuntimeConfig,
) -> Result<RuntimeConfig> {
    let path = runtime_config_path(output);
    if !path.is_file() {
        return Ok(base);
    }
    let patch: ConfigPatch = serde_json::from_slice(
        &std::fs::read(&path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?,
    )
    .map_err(|error| anyhow::anyhow!("decode {}: {error}", path.display()))?;
    apply_patch(&mut base, patch)?;
    Ok(base)
}

pub fn runtime_config_path(output: &std::path::Path) -> PathBuf {
    output.join("_ynote").join("runtime-config.json")
}

fn persist_runtime_config(config: &RuntimeConfig) -> Result<()> {
    let patch = ConfigPatch {
        cloud_enabled: Some(config.cloud_enabled),
        cloud_interval_seconds: Some(config.cloud_interval_seconds),
        cloud_jitter_seconds: Some(config.cloud_jitter_seconds),
        local_debounce_milliseconds: Some(config.local_debounce_milliseconds),
        web_status_poll_milliseconds: Some(config.web_status_poll_milliseconds),
    };
    crate::atomic::write(
        &runtime_config_path(&config.output),
        &serde_json::to_vec_pretty(&patch)?,
    )
}

fn apply_patch(config: &mut RuntimeConfig, patch: ConfigPatch) -> Result<()> {
    if let Some(value) = patch.cloud_enabled {
        config.cloud_enabled = value;
    }
    if let Some(value) = patch.cloud_interval_seconds {
        syncer::validate_interval(value)?;
        config.cloud_interval_seconds = value;
    }
    if let Some(value) = patch.cloud_jitter_seconds {
        if value > MAX_CLOUD_JITTER_SECONDS {
            bail!("cloud jitter must be at most {MAX_CLOUD_JITTER_SECONDS} seconds");
        }
        config.cloud_jitter_seconds = value;
    }
    if let Some(value) = patch.local_debounce_milliseconds {
        if !(MIN_LOCAL_DEBOUNCE_MILLISECONDS..=MAX_LOCAL_DEBOUNCE_MILLISECONDS).contains(&value) {
            bail!(
                "local debounce must be between {MIN_LOCAL_DEBOUNCE_MILLISECONDS} and {MAX_LOCAL_DEBOUNCE_MILLISECONDS} milliseconds"
            );
        }
        config.local_debounce_milliseconds = value;
    }
    if let Some(value) = patch.web_status_poll_milliseconds {
        if !(MIN_WEB_POLL_MILLISECONDS..=MAX_WEB_POLL_MILLISECONDS).contains(&value) {
            bail!(
                "web poll must be between {MIN_WEB_POLL_MILLISECONDS} and {MAX_WEB_POLL_MILLISECONDS} milliseconds"
            );
        }
        config.web_status_poll_milliseconds = value;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessMetrics {
    pub pid: u32,
    pub uptime_seconds: u64,
    pub logical_processors: usize,
    pub cpu_one_core_percent: f64,
    pub cpu_machine_percent: f64,
    pub working_set_bytes: u64,
    pub private_bytes: u64,
    pub peak_working_set_bytes: u64,
    pub handle_count: u32,
}

struct ResourceSampler {
    last_wall: Instant,
    last_cpu_100ns: u64,
}

impl ResourceSampler {
    fn new() -> Self {
        Self {
            last_wall: Instant::now(),
            last_cpu_100ns: process_cpu_100ns().unwrap_or_default(),
        }
    }

    fn sample(&mut self, started_at_unix: u64) -> ProcessMetrics {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_wall).as_secs_f64();
        let cpu = process_cpu_100ns().unwrap_or(self.last_cpu_100ns);
        let cpu_delta_seconds = cpu.saturating_sub(self.last_cpu_100ns) as f64 / 10_000_000.0;
        let logical_processors = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let one_core = if elapsed > 0.0 {
            100.0 * cpu_delta_seconds / elapsed
        } else {
            0.0
        };
        self.last_wall = now;
        self.last_cpu_100ns = cpu;
        let memory = process_memory().unwrap_or_default();
        ProcessMetrics {
            pid: std::process::id(),
            uptime_seconds: unix_now().saturating_sub(started_at_unix),
            logical_processors,
            cpu_one_core_percent: round4(one_core),
            cpu_machine_percent: round4(one_core / logical_processors as f64),
            working_set_bytes: memory.working_set,
            private_bytes: memory.private,
            peak_working_set_bytes: memory.peak_working_set,
            handle_count: process_handle_count().unwrap_or_default(),
        }
    }
}

#[derive(Default)]
struct MemoryUsage {
    working_set: u64,
    private: u64,
    peak_working_set: u64,
}

#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}

#[repr(C)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn GetProcessHandleCount(process: *mut c_void, count: *mut u32) -> i32;
    fn GetProcessTimes(
        process: *mut c_void,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn K32GetProcessMemoryInfo(
        process: *mut c_void,
        counters: *mut ProcessMemoryCountersEx,
        size: u32,
    ) -> i32;
}

fn process_cpu_100ns() -> Option<u64> {
    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    (ok != 0).then(|| filetime_value(&kernel).saturating_add(filetime_value(&user)))
}

fn process_memory() -> Option<MemoryUsage> {
    let mut counters = ProcessMemoryCountersEx {
        cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
        private_usage: 0,
    };
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
        )
    };
    (ok != 0).then_some(MemoryUsage {
        working_set: counters.working_set_size as u64,
        private: counters.private_usage as u64,
        peak_working_set: counters.peak_working_set_size as u64,
    })
}

fn process_handle_count() -> Option<u32> {
    let mut count = 0;
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    (ok != 0).then_some(count)
}

fn filetime_value(value: &FileTime) -> u64 {
    (u64::from(value.high) << 32) | u64::from(value.low)
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

pub fn command_catalog() -> Value {
    json!([
        {"command":"doctor","parameters":["--data-root","--account","--mirror","--pretty"],"web":"概览/数据源","mode":"read"},
        {"command":"tree","parameters":["--text","--mirror","--pretty"],"web":"笔记目录","mode":"read"},
        {"command":"list","parameters":["--parent","--mirror","--pretty"],"web":"笔记目录","mode":"read"},
        {"command":"read","parameters":["<id>","--output-format structured|markdown|html|raw","--mirror","--pretty"],"web":"笔记阅读","mode":"read"},
        {"command":"search","parameters":["<query>","--limit 1..500","--mirror","--pretty"],"web":"全文搜索","mode":"read"},
        {"command":"resources","parameters":["--note","--mirror","--pretty"],"web":"资源统计/笔记正文","mode":"read"},
        {"command":"export","parameters":["--output","--data-root","--account","--mirror"],"web":"镜像自动导出","mode":"read"},
        {"command":"sync","parameters":["--output","--watch","--interval >=300","--jitter","--local-only"],"web":"参数与即时同步","mode":"control"},
        {"command":"mirror refresh","parameters":["--output","--local-only"],"web":"即时同步","mode":"control"},
        {"command":"mirror status","parameters":["--output","--pretty"],"web":"完整性/存储","mode":"read"},
        {"command":"mirror query","parameters":["--output","<read-only-sql>","--pretty"],"web":"只读 SQL","mode":"control"},
        {"command":"daemon run","parameters":["--output","--interval >=300","--jitter","--bind loopback","--port","--local-only"],"web":"运行参数","mode":"control"},
        {"command":"daemon install","parameters":["--output","--interval >=300","--jitter","--port"],"web":"显示状态；终端执行","mode":"lifecycle"},
        {"command":"daemon status","parameters":["--pretty"],"web":"启动状态","mode":"read"},
        {"command":"daemon uninstall","parameters":[],"web":"仅终端执行","mode":"lifecycle"},
        {"command":"writeback outbox","parameters":["--output","--pretty"],"web":"安全边界/待处理计数","mode":"read"},
        {"command":"writeback discard","parameters":["--output","<id>"],"web":"仅终端执行，避免误删草稿","mode":"destructive"},
        {"command":"serve","parameters":["--bind loopback","--port","--open","--mirror"],"web":"当前服务","mode":"lifecycle"}
    ])
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RuntimeConfig {
        RuntimeConfig {
            cloud_enabled: true,
            cloud_interval_seconds: 900,
            cloud_jitter_seconds: 120,
            local_debounce_milliseconds: 800,
            local_max_batch_seconds: 5,
            web_status_poll_milliseconds: 2_000,
            bind: "127.0.0.1".to_string(),
            port: 4768,
            output: std::env::temp_dir()
                .join(format!("ynote-cli-console-test-{}", std::process::id())),
            executable: PathBuf::from("ynote-cli.exe"),
            startup_installed: true,
        }
    }

    #[tokio::test]
    async fn validates_and_broadcasts_runtime_configuration() {
        let (config_tx, mut config_rx) = watch::channel(config());
        let (sync_tx, _sync_rx) = mpsc::channel(1);
        let control = ConsoleControl::daemon(config(), config_tx, sync_tx);
        assert!(
            control
                .update(ConfigPatch {
                    cloud_enabled: None,
                    cloud_interval_seconds: Some(299),
                    cloud_jitter_seconds: None,
                    local_debounce_milliseconds: None,
                    web_status_poll_milliseconds: None,
                })
                .is_err()
        );
        let updated = control
            .update(ConfigPatch {
                cloud_enabled: Some(false),
                cloud_interval_seconds: Some(600),
                cloud_jitter_seconds: Some(30),
                local_debounce_milliseconds: Some(500),
                web_status_poll_milliseconds: Some(1_000),
            })
            .unwrap();
        config_rx.changed().await.unwrap();
        assert!(!updated.cloud_enabled);
        assert_eq!(config_rx.borrow().cloud_interval_seconds, 600);
        assert_eq!(updated.local_debounce_milliseconds, 500);
        assert!(runtime_config_path(&updated.output).is_file());
        let loaded = load_runtime_config(&updated.output, config()).unwrap();
        assert_eq!(loaded.cloud_interval_seconds, 600);
        std::fs::remove_dir_all(&updated.output).unwrap();
    }

    #[test]
    fn exposes_complete_command_and_process_inventory() {
        assert_eq!(command_catalog().as_array().unwrap().len(), 18);
        let control = ConsoleControl::static_server(config());
        let metrics = control.metrics();
        assert_eq!(metrics.pid, std::process::id());
        assert!(metrics.logical_processors >= 1);
        assert!(metrics.working_set_bytes > 0);
    }
}
