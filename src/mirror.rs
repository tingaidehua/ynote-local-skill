use crate::model::{ExportManifest, Item, ItemKind, Resource, SourceInfo};
use crate::repository::Repository;
use crate::sqlite::Connection;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRecord {
    pub started_at_unix: u64,
    pub finished_at_unix: u64,
    pub backend: String,
    pub success: bool,
    pub message: String,
    pub stats: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorStatus {
    pub database: PathBuf,
    pub exists: bool,
    pub integrity: String,
    pub last_sync: Option<Value>,
    pub counts: Value,
    pub pending_outbox: usize,
}

pub fn database_path(output: &Path) -> PathBuf {
    output.join("_ynote").join("ynote-mirror.sqlite")
}

pub fn write_snapshot(repo: &Repository, output: &Path, record: &SyncRecord) -> Result<PathBuf> {
    let database = database_path(output);
    fs::create_dir_all(database.parent().context("mirror database has no parent")?)?;
    let connection = Connection::open_readwrite_create(&database)?;
    connection.execute(SCHEMA)?;
    let mut sql = String::from(
        "BEGIN IMMEDIATE;\nDELETE FROM items;\nDELETE FROM notes;\nDELETE FROM resources;\n",
    );
    for item in &repo.items {
        sql.push_str(&format!(
            "INSERT INTO items(id,parent_id,kind,title,version,modified_at,deleted,item_json) VALUES ({},{},{},{},{},{},{},{});\n",
            quote(&item.id),
            quote(&item.parent_id),
            quote(match item.kind { ItemKind::Root => "root", ItemKind::Folder => "folder", ItemKind::Note => "note" }),
            quote(&item.title),
            item.version,
            item.modified_at,
            i32::from(item.deleted),
            quote(&serde_json::to_string(item)?)
        ));
        if item.kind == ItemKind::Note && !item.deleted {
            let rendered = repo.read_note(&item.id)?;
            let raw_json = rendered
                .raw
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?
                .unwrap_or_default();
            let markdown_sha256 = format!("{:x}", Sha256::digest(rendered.markdown.as_bytes()));
            sql.push_str(&format!(
                "INSERT INTO notes(item_id,fidelity,raw_format,raw_json,blocks_json,markdown,markdown_sha256,html,content_text) VALUES ({},{},{},{},{},{},{},{},{});\n",
                quote(&item.id),
                quote(&rendered.fidelity),
                quote(&rendered.raw_format),
                quote(&raw_json),
                quote(&serde_json::to_string(&rendered.blocks)?),
                quote(&rendered.markdown),
                quote(&markdown_sha256),
                quote(&rendered.html),
                quote(&rendered.markdown)
            ));
        }
    }
    let mut resource_ids = HashSet::new();
    for resource in repo.resources.values() {
        resource_ids.insert(resource.id.clone());
        let relative_path = resource
            .entry
            .as_ref()
            .and_then(|path| path.strip_prefix(output).ok())
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let sha256 = resource
            .entry
            .as_ref()
            .filter(|path| path.is_file())
            .map(hash_file)
            .transpose()?
            .unwrap_or_default();
        sql.push_str(&format!(
            "INSERT INTO resources(id,version,relative_path,sha256,resource_json) VALUES ({},{},{},{},{});\n",
            quote(&resource.id),
            resource.version,
            quote(&relative_path),
            quote(&sha256),
            quote(&serde_json::to_string(resource)?)
        ));
    }
    let manifest_path = output.join(".ynote-manifest.json");
    if manifest_path.is_file() {
        let manifest: ExportManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        for (id, exported) in manifest.resources {
            if resource_ids.contains(&id) {
                continue;
            }
            let entry = exported
                .relative_path
                .as_ref()
                .map(|relative| output.join(relative));
            let resource = Resource {
                id: id.clone(),
                title: exported.title,
                media_type: exported.media_type,
                size: exported.size,
                version: 0,
                entry,
                remote_url: None,
                available: exported.available,
            };
            sql.push_str(&format!(
                "INSERT INTO resources(id,version,relative_path,sha256,resource_json) VALUES ({},0,{},{},{});\n",
                quote(&id),
                quote(exported.relative_path.as_deref().unwrap_or_default()),
                quote(exported.sha256.as_deref().unwrap_or_default()),
                quote(&serde_json::to_string(&resource)?)
            ));
        }
    }
    sql.push_str("DELETE FROM sync_state;\n");
    sql.push_str(&format!(
        "INSERT INTO sync_state(key,value) VALUES ('source',{}),('last_sync',{}),('output',{});\n",
        quote(&serde_json::to_string(&repo.source)?),
        quote(&serde_json::to_string(record)?),
        quote(&output.to_string_lossy())
    ));
    sql.push_str(&format!(
        "INSERT INTO sync_runs(started_at,finished_at,backend,success,message,stats_json) VALUES ({},{},{},{},{},{});\n",
        record.started_at_unix,
        record.finished_at_unix,
        quote(&record.backend),
        i32::from(record.success),
        quote(&record.message),
        quote(&record.stats.to_string())
    ));
    sql.push_str("COMMIT;");
    connection.execute(&sql)?;
    let integrity = connection.query("PRAGMA integrity_check")?;
    let result = integrity
        .first()
        .and_then(|row| row.first())
        .and_then(Clone::clone)
        .unwrap_or_default();
    if result != "ok" {
        bail!("mirror database integrity check failed");
    }
    Ok(database)
}

pub fn load(database: &Path) -> Result<Repository> {
    let connection = Connection::open_readonly(database)?;
    let source_json = state_value(&connection, "source")?.context("mirror source is missing")?;
    let source: SourceInfo = serde_json::from_str(&source_json).context("decode mirror source")?;
    let mut items = Vec::new();
    for row in connection.query("SELECT item_json FROM items ORDER BY rowid")? {
        let raw = row.first().and_then(Clone::clone).unwrap_or_default();
        items.push(serde_json::from_str::<Item>(&raw).context("decode mirror item")?);
    }
    let mut resources = BTreeMap::new();
    for row in connection.query("SELECT resource_json FROM resources ORDER BY id")? {
        let raw = row.first().and_then(Clone::clone).unwrap_or_default();
        let resource: Resource = serde_json::from_str(&raw).context("decode mirror resource")?;
        resources.insert(resource.id.clone(), resource);
    }
    let mut raw_values = HashMap::new();
    let mut content_index = HashMap::new();
    let mut fidelity_overrides = HashMap::new();
    for row in connection
        .query("SELECT item_id,raw_json,content_text,fidelity FROM notes ORDER BY item_id")?
    {
        let id = cell(&row, 0);
        let raw = cell(&row, 1);
        if !raw.is_empty() {
            raw_values.insert(id.clone(), serde_json::from_str(&raw)?);
        }
        content_index.insert(id.clone(), cell(&row, 2));
        fidelity_overrides.insert(id, cell(&row, 3));
    }
    Ok(Repository {
        source,
        items,
        resources,
        content_index,
        raw_paths: HashMap::new(),
        raw_values,
        fidelity_overrides,
    })
}

pub fn status(output: &Path) -> Result<MirrorStatus> {
    let database = database_path(output);
    if !database.is_file() {
        return Ok(MirrorStatus {
            database,
            exists: false,
            integrity: "missing".to_string(),
            last_sync: None,
            counts: json!({}),
            pending_outbox: 0,
        });
    }
    let connection = Connection::open_readonly(&database)?;
    let integrity = cell(
        connection
            .query("PRAGMA integrity_check")?
            .first()
            .cloned()
            .unwrap_or_default()
            .as_slice(),
        0,
    );
    let count = |table: &str| -> Result<usize> {
        Ok(cell(
            connection
                .query(&format!("SELECT count(*) FROM {table}"))?
                .first()
                .cloned()
                .unwrap_or_default()
                .as_slice(),
            0,
        )
        .parse()
        .unwrap_or_default())
    };
    let last_sync = state_value(&connection, "last_sync")?
        .map(|value| serde_json::from_str(&value))
        .transpose()?;
    Ok(MirrorStatus {
        database,
        exists: true,
        integrity,
        last_sync,
        counts: json!({
            "items": count("items")?,
            "notes": count("notes")?,
            "resources": count("resources")?,
            "syncRuns": count("sync_runs")?
        }),
        pending_outbox: cell(
            connection
                .query("SELECT count(*) FROM outbox WHERE status='pending'")?
                .first()
                .cloned()
                .unwrap_or_default()
                .as_slice(),
            0,
        )
        .parse()
        .unwrap_or_default(),
    })
}

pub fn query(output: &Path, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    validate_readonly_sql(sql)?;
    Connection::open_readonly(&database_path(output))?.query(sql)
}

pub fn list_outbox(output: &Path) -> Result<Vec<Vec<Option<String>>>> {
    let database = database_path(output);
    if !database.is_file() {
        return Ok(Vec::new());
    }
    Connection::open_readonly(&database)?.query(
        "SELECT id,note_id,base_version,operation,status,created_at,error FROM outbox ORDER BY id",
    )
}

pub fn discard_outbox(output: &Path, id: i64) -> Result<bool> {
    if id <= 0 {
        bail!("outbox id must be positive");
    }
    let database = database_path(output);
    let connection = Connection::open_readwrite_create(&database)?;
    let existed = !connection
        .query(&format!("SELECT id FROM outbox WHERE id={id} LIMIT 1"))?
        .is_empty();
    if existed {
        connection.execute(&format!("DELETE FROM outbox WHERE id={id}"))?;
    }
    Ok(existed)
}

pub fn capture_external_edits(output: &Path) -> Result<usize> {
    let manifest_path = output.join(".ynote-manifest.json");
    let database = database_path(output);
    if !manifest_path.is_file() || !database.is_file() {
        return Ok(0);
    }
    let manifest: ExportManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let connection = Connection::open_readwrite_create(&database)?;
    connection.execute(SCHEMA)?;
    let versions = connection
        .query("SELECT id,version FROM items")?
        .into_iter()
        .map(|row| (cell(&row, 0), cell(&row, 1).parse().unwrap_or_default()))
        .collect::<HashMap<String, i64>>();
    let mut captured = 0;
    let mut sql = String::from("BEGIN IMMEDIATE;\n");
    for (id, item) in manifest.items {
        if item.kind != ItemKind::Note {
            continue;
        }
        let Some(expected) = item.text_sha256 else {
            continue;
        };
        let relative = Path::new(&item.relative_path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            continue;
        }
        let path = output.join(relative);
        if !path.is_file() {
            continue;
        }
        let payload = fs::read_to_string(&path)?;
        let actual = format!("{:x}", Sha256::digest(payload.as_bytes()));
        if actual == expected {
            continue;
        }
        let base_version = versions.get(&id).copied().unwrap_or_default();
        sql.push_str(&format!(
            "INSERT OR IGNORE INTO outbox(note_id,base_version,base_checksum,operation,payload,content_sha256,status,created_at,error) VALUES ({},{},{},'replace_markdown',{},{} ,'pending',{},'');\n",
            quote(&id),
            base_version,
            quote(&expected),
            quote(&payload),
            quote(&actual),
            unix_now()
        ));
        captured += 1;
    }
    sql.push_str("COMMIT;");
    connection.execute(&sql)?;
    Ok(captured)
}

pub fn validate_readonly_sql(sql: &str) -> Result<()> {
    let trimmed = sql.trim();
    let without_trailing = trimmed.strip_suffix(';').unwrap_or(trimmed);
    if without_trailing.contains(';') {
        bail!("only one read-only SQL statement is allowed");
    }
    let lower = without_trailing.to_ascii_lowercase();
    let allowed = lower.starts_with("select ")
        || lower.starts_with("with ")
        || lower == "pragma integrity_check"
        || lower == "pragma quick_check"
        || lower.starts_with("pragma table_info(");
    if !allowed {
        bail!("mirror query only accepts SELECT, WITH, integrity checks, or table_info");
    }
    Ok(())
}

fn state_value(connection: &Connection, key: &str) -> Result<Option<String>> {
    Ok(connection
        .query(&format!(
            "SELECT value FROM sync_state WHERE key={} LIMIT 1",
            quote(key)
        ))?
        .first()
        .and_then(|row| row.first())
        .and_then(Clone::clone))
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\0', "").replace('\'', "''"))
}

fn cell(row: &[Option<String>], index: usize) -> String {
    row.get(index).and_then(Clone::clone).unwrap_or_default()
}

fn hash_file(path: &PathBuf) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(fs::read(path)?)))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
CREATE TABLE IF NOT EXISTS items(
    id TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    version INTEGER NOT NULL,
    modified_at INTEGER NOT NULL,
    deleted INTEGER NOT NULL,
    item_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS items_parent_idx ON items(parent_id,deleted,kind);
CREATE TABLE IF NOT EXISTS notes(
    item_id TEXT PRIMARY KEY,
    fidelity TEXT NOT NULL,
    raw_format TEXT NOT NULL,
    raw_json TEXT NOT NULL,
    blocks_json TEXT NOT NULL,
    markdown TEXT NOT NULL,
    markdown_sha256 TEXT NOT NULL,
    html TEXT NOT NULL,
    content_text TEXT NOT NULL,
    FOREIGN KEY(item_id) REFERENCES items(id)
);
CREATE TABLE IF NOT EXISTS resources(
    id TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    relative_path TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    resource_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_state(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sync_runs(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER NOT NULL,
    backend TEXT NOT NULL,
    success INTEGER NOT NULL,
    message TEXT NOT NULL,
    stats_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS outbox(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    note_id TEXT NOT NULL,
    base_version INTEGER NOT NULL,
    base_checksum TEXT NOT NULL,
    operation TEXT NOT NULL,
    payload TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    error TEXT NOT NULL,
    UNIQUE(note_id,content_sha256,status)
);
"#;

#[cfg(test)]
mod tests {
    use super::validate_readonly_sql;

    #[test]
    fn rejects_mutating_or_stacked_sql() {
        assert!(validate_readonly_sql("SELECT id FROM notes").is_ok());
        assert!(validate_readonly_sql("PRAGMA integrity_check").is_ok());
        assert!(validate_readonly_sql("DELETE FROM notes").is_err());
        assert!(validate_readonly_sql("SELECT 1; DELETE FROM notes").is_err());
        assert!(validate_readonly_sql("PRAGMA journal_mode=off").is_err());
    }
}
