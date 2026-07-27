use crate::console::{
    self, ConfigPatch, ConsoleControl, DEFAULT_LOCAL_DEBOUNCE_MILLISECONDS,
    DEFAULT_LOCAL_MAX_BATCH_SECONDS, DEFAULT_WEB_POLL_MILLISECONDS, RuntimeConfig, SyncKind,
};
use crate::mirror;
use crate::model::ItemKind;
use crate::repository::Repository;
use crate::syncer;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {
    repo: Arc<RwLock<Repository>>,
    sync_status: Arc<RwLock<Value>>,
    console: ConsoleControl,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SyncRequest {
    kind: SyncKind,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SqlRequest {
    sql: String,
}

type ApiResult = Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)>;

pub async fn serve(repo: Repository, bind: &str, port: u16) -> anyhow::Result<()> {
    let config = RuntimeConfig {
        cloud_enabled: false,
        cloud_interval_seconds: syncer::DEFAULT_INTERVAL_SECONDS,
        cloud_jitter_seconds: syncer::DEFAULT_JITTER_SECONDS,
        local_debounce_milliseconds: DEFAULT_LOCAL_DEBOUNCE_MILLISECONDS,
        local_max_batch_seconds: DEFAULT_LOCAL_MAX_BATCH_SECONDS,
        web_status_poll_milliseconds: DEFAULT_WEB_POLL_MILLISECONDS,
        bind: bind.to_string(),
        port,
        output: PathBuf::new(),
        executable: std::env::current_exe().unwrap_or_default(),
        startup_installed: false,
    };
    serve_shared(
        Arc::new(RwLock::new(repo)),
        Arc::new(RwLock::new(json!({"mode": "static", "revision": 1}))),
        ConsoleControl::static_server(config),
        bind,
        port,
    )
    .await
}

pub async fn serve_shared(
    repo: Arc<RwLock<Repository>>,
    sync_status: Arc<RwLock<Value>>,
    console: ConsoleControl,
    bind: &str,
    port: u16,
) -> anyhow::Result<()> {
    let state = AppState {
        repo,
        sync_status,
        console,
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/tree", get(tree))
        .route("/api/notes/{id}", get(note))
        .route("/api/search", get(search))
        .route("/api/assets/{id}", get(asset))
        .route("/api/console", get(console_overview))
        .route("/api/console/metrics", get(console_metrics))
        .route("/api/console/config", post(update_console_config))
        .route("/api/console/sync", post(queue_console_sync))
        .route("/api/console/sql", post(console_sql))
        .with_state(state);
    let listener = TcpListener::bind((bind, port)).await?;
    eprintln!("ynote-cli web: http://{bind}:{port}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn index() -> Response {
    let mut response = Html(include_str!("../web/index.html")).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data: https://note.youdao.com; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    let repo = state.repo.read().unwrap_or_else(|error| error.into_inner());
    let sync_status = state
        .sync_status
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    Json(json!({
        "ok": true,
        "data": {
            "account": repo.source.account,
            "readOnly": true,
            "items": repo.items.len(),
            "resources": repo.resources.len(),
            "sync": sync_status,
            "version": env!("CARGO_PKG_VERSION")
        }
    }))
}

async fn tree(State(state): State<AppState>) -> Json<Value> {
    let repo = state.repo.read().unwrap_or_else(|error| error.into_inner());
    Json(json!({ "ok": true, "data": repo.tree() }))
}

async fn note(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let repo = state.repo.read().unwrap_or_else(|error| error.into_inner());
    repo.read_note(&id)
        .map(|note| Json(json!({ "ok": true, "data": note })))
        .map_err(|error| api_error(StatusCode::NOT_FOUND, "not_found", error.to_string()))
}

async fn search(State(state): State<AppState>, Query(query): Query<SearchQuery>) -> Json<Value> {
    let q = query.q.unwrap_or_default();
    let hits = if q.trim().is_empty() {
        Vec::new()
    } else {
        state
            .repo
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .search(&q, query.limit.unwrap_or(50).min(500))
    };
    Json(json!({ "ok": true, "data": hits, "meta": {"query": q} }))
}

async fn asset(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, StatusCode> {
    let (path, media_type) = {
        let repo = state.repo.read().unwrap_or_else(|error| error.into_inner());
        let resource = repo.resources.get(&id).ok_or(StatusCode::NOT_FOUND)?;
        let path = resource
            .entry
            .as_ref()
            .filter(|path| path.is_file())
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?;
        (path, resource.media_type.clone())
    };
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(if media_type.is_empty() {
            "application/octet-stream"
        } else {
            &media_type
        })
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=60"),
    );
    Ok(response)
}

async fn console_overview(State(state): State<AppState>) -> Json<Value> {
    let config = state.console.config();
    let (source, counts) = {
        let repo = state.repo.read().unwrap_or_else(|error| error.into_inner());
        (
            repo.source.clone(),
            json!({
                "items": repo.items.iter().filter(|item| !item.deleted).count(),
                "folders": repo.items.iter().filter(|item| !item.deleted && item.kind == ItemKind::Folder).count(),
                "notes": repo.items.iter().filter(|item| !item.deleted && item.kind == ItemKind::Note).count(),
                "resources": repo.resources.len()
            }),
        )
    };
    let sync = state
        .sync_status
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let mirror_status = (!config.output.as_os_str().is_empty())
        .then(|| mirror::status(&config.output).ok())
        .flatten();
    let history = if config.output.as_os_str().is_empty() {
        Vec::new()
    } else {
        mirror::query(
            &config.output,
            "SELECT id,started_at,finished_at,backend,success,message FROM sync_runs ORDER BY id DESC LIMIT 12",
        )
        .unwrap_or_default()
    };
    let pending_outbox = mirror_status
        .as_ref()
        .map(|status| status.pending_outbox)
        .unwrap_or_default();
    let setting = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("ynote-desktop")
        .join("setting.json");
    let mirror_database = if config.output.as_os_str().is_empty() {
        PathBuf::new()
    } else {
        mirror::database_path(&config.output)
    };
    let storage = json!({
        "mirrorDatabaseBytes": file_size(&mirror_database),
        "manifestBytes": file_size(&config.output.join(".ynote-manifest.json")),
        "sourceDatabaseBytes": file_size(&source.database),
        "contentDatabaseBytes": source.content_database.as_ref().map(|path| file_size(path)).unwrap_or_default()
    });
    Json(json!({
        "ok": true,
        "data": {
            "tool": {"name":"ynote-cli","version":env!("CARGO_PKG_VERSION"),"language":"Rust","pid":std::process::id()},
            "mutable": state.console.mutable(),
            "config": config,
            "metrics": state.console.metrics(),
            "sync": sync,
            "counts": counts,
            "storage": storage,
            "mirror": mirror_status,
            "syncHistory": {
                "columns":["id","startedAt","finishedAt","backend","success","message"],
                "rows": history
            },
            "outbox": {"pending":pending_outbox,"cloudApplyEnabled":false},
            "sources": [
                {"name":"桌面登录态","kind":"credential","path":setting,"access":"只读；Cookie 仅在进程内存中使用，不进入 API/日志/镜像"},
                {"name":"客户端元数据","kind":"sqlite_wal","path":source.database,"access":"Windows 文件事件 + 只读 SQLite"},
                {"name":"客户端搜索索引","kind":"sqlite","path":source.content_database,"access":"只读全文索引回退"},
                {"name":"客户端正文","kind":"files","path":source.data_root.join("file"),"access":"只读原始 JSON/二进制"},
                {"name":"客户端资源","kind":"files","path":source.data_root.join("resource"),"access":"只读图片与附件"},
                {"name":"有道云","kind":"https","path":"https://note.youdao.com","access":"低频只读 pull/download/getResource；无 push/update/delete"},
                {"name":"AI 镜像","kind":"sqlite_markdown","path":config.output,"access":"未加密 SQLite + Markdown + JSON + 资源"},
                {"name":"控制台参数","kind":"json","path":console::runtime_config_path(&config.output),"access":"仅保存可热更新参数；显式 --local-only 优先"}
            ],
            "pipeline": [
                {"id":"desktop","label":"桌面客户端","detail":"已登录会话、SQLite/WAL、正文、资源"},
                {"id":"watch","label":"本地事件监听","detail":"原生 Windows 通知；不访问云端"},
                {"id":"cloud","label":"低频云拉取","detail":"间隔 + 抖动 + 指数退避 + 版本缓存"},
                {"id":"normalize","label":"解析与规范化","detail":"保留原始 JSON，同时生成块/Markdown/HTML"},
                {"id":"commit","label":"事务提交","detail":"原子文件 + SQLite 完整性检查"},
                {"id":"consume","label":"CLI / Web / AI","detail":"稳定 ID、层级、全文、SQL、图文资源"}
            ],
            "commands": console::command_catalog(),
            "security": {
                "bindPolicy":"loopback_only",
                "csrfGuard":"same-origin JSON + X-Ynote-Console header",
                "cloudWriteBack":false,
                "cloudMinimumIntervalSeconds":syncer::MIN_INTERVAL_SECONDS,
                "cookiesExposed":false,
                "mutatingSqlAllowed":false
            }
        }
    }))
}

async fn console_metrics(State(state): State<AppState>) -> Json<Value> {
    let repo = state.repo.read().unwrap_or_else(|error| error.into_inner());
    let sync = state
        .sync_status
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    Json(json!({
        "ok": true,
        "data": {
            "metrics": state.console.metrics(),
            "config": state.console.config(),
            "sync": sync,
            "counts": {"items":repo.items.len(),"resources":repo.resources.len()}
        }
    }))
}

async fn update_console_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(patch): Json<ConfigPatch>,
) -> ApiResult {
    require_console_header(&headers)?;
    match state.console.update(patch) {
        Ok(config) => {
            let mut status = state
                .sync_status
                .write()
                .unwrap_or_else(|error| error.into_inner());
            status["configurationUpdatedAtUnix"] = json!(console::unix_now());
            Ok((
                StatusCode::OK,
                Json(json!({"ok":true,"data":{"config":config}})),
            ))
        }
        Err(error) => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_configuration",
            error.to_string(),
        )),
    }
}

async fn queue_console_sync(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SyncRequest>,
) -> ApiResult {
    require_console_header(&headers)?;
    {
        let mut status = state
            .sync_status
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if status["manualRequestPending"].as_bool().unwrap_or(false)
            || matches!(
                status["state"].as_str(),
                Some("syncing_cloud" | "refreshing_local")
            )
        {
            return Err(api_error(
                StatusCode::CONFLICT,
                "sync_busy",
                "a synchronization is already running or queued",
            ));
        }
        if matches!(request.kind, SyncKind::Cloud) {
            let last = status["lastCloudAttemptUnix"].as_u64().unwrap_or_default();
            let elapsed = console::unix_now().saturating_sub(last);
            if last > 0 && elapsed < syncer::MIN_INTERVAL_SECONDS {
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({
                        "ok":false,
                        "error":{
                            "type":"cloud_rate_limited",
                            "message":"cloud refresh is protected by the same minimum interval as the CLI",
                            "retryAfterSeconds":syncer::MIN_INTERVAL_SECONDS-elapsed
                        }
                    })),
                ));
            }
        }
        status["manualRequestPending"] = json!(true);
        status["lastQueuedManualKind"] = json!(request.kind);
    }
    if let Err(error) = state.console.queue_sync(request.kind) {
        state
            .sync_status
            .write()
            .unwrap_or_else(|lock_error| lock_error.into_inner())["manualRequestPending"] =
            json!(false);
        return Err(api_error(
            StatusCode::CONFLICT,
            "sync_queue_full",
            error.to_string(),
        ));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"ok":true,"data":{"queued":true,"kind":request.kind}})),
    ))
}

async fn console_sql(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SqlRequest>,
) -> ApiResult {
    require_console_header(&headers)?;
    if request.sql.len() > 8_000 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "query_too_long",
            "SQL is limited to 8000 characters",
        ));
    }
    let output = state.console.config().output;
    if output.as_os_str().is_empty() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "mirror_unavailable",
            "this static server was not started with a managed mirror output",
        ));
    }
    match mirror::query(&output, &request.sql) {
        Ok(rows) => Ok((
            StatusCode::OK,
            Json(json!({"ok":true,"data":{"columns":"positional","rows":rows}})),
        )),
        Err(error) => Err(api_error(
            StatusCode::BAD_REQUEST,
            "readonly_sql_rejected",
            error.to_string(),
        )),
    }
}

fn require_console_header(headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    if headers
        .get("x-ynote-console")
        .and_then(|value| value.to_str().ok())
        == Some("1")
    {
        Ok(())
    } else {
        Err(api_error(
            StatusCode::FORBIDDEN,
            "console_header_required",
            "missing same-origin console request header",
        ))
    }
}

fn api_error(
    status: StatusCode,
    error_type: &str,
    message: impl Into<String>,
) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({
            "ok":false,
            "error":{"type":error_type,"message":message.into()}
        })),
    )
}

fn file_size(path: &std::path::Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or_default()
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
