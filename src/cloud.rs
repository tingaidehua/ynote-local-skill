use crate::model::{Item, ItemKind, Resource, ResourceRef};
use crate::repository::Repository;
use crate::winhttp;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudStats {
    pub metadata_entries: usize,
    pub active_entries: usize,
    pub deleted_entries: usize,
    pub note_bodies_downloaded: usize,
    pub note_bodies_reused: usize,
    pub resources_downloaded: usize,
    pub resources_reused: usize,
    pub root_version: i64,
}

struct DesktopAuth {
    cookie_header: String,
    pc_header: String,
    cstk: String,
}

pub fn refresh(local: Repository, output: &Path) -> Result<(Repository, CloudStats)> {
    let auth = DesktopAuth::load()?;
    let remote_values = pull_all(&auth)?;
    let cache_root = output.join("_ynote").join("cloud");
    let raw_root = cache_root.join("raw");
    let resource_root = cache_root.join("resources");
    fs::create_dir_all(&raw_root)?;
    fs::create_dir_all(&resource_root)?;

    let root_id = local.root_id().unwrap_or_default().to_string();
    let mut repo = local;
    let mut item_indices = repo
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| (item.id.to_ascii_lowercase(), index))
        .collect::<HashMap<_, _>>();
    let mut remote_active = HashSet::new();
    let mut metadata_entries = 0;
    let mut active_entries = 0;
    let mut deleted_entries = 0;
    let mut bodies_downloaded = 0;
    let mut bodies_reused = 0;
    let mut resources_downloaded = 0;
    let mut resources_reused = 0;
    let mut root_version = 0;

    for wrapper in remote_values {
        metadata_entries += 1;
        let entry = wrapper
            .get("entry")
            .and_then(Value::as_object)
            .context("remote metadata entry was malformed")?;
        let id = string_value(entry.get("id"));
        if id.is_empty() {
            continue;
        }
        let deleted = bool_value(entry.get("deleted"));
        if deleted {
            deleted_entries += 1;
            continue;
        }
        active_entries += 1;
        remote_active.insert(id.to_ascii_lowercase());
        let version = int_value(entry.get("version"));
        root_version = root_version.max(version);
        let is_dir = bool_value(entry.get("dir"));
        let parent_id = string_value(entry.get("parentId"));
        let title = string_value(entry.get("name"));
        let meta_value = wrapper.get("meta").and_then(|x| x.get("value"));
        let resources: Vec<ResourceRef> = meta_value
            .and_then(|x| x.get("resources"))
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(resource_ref).collect())
            .unwrap_or_default();
        let created_at = meta_value
            .and_then(|x| x.get("createTimeForSort"))
            .map(|value| int_value(Some(value)))
            .unwrap_or_default();
        let modified_at = meta_value
            .and_then(|x| x.get("modifyTimeForSort"))
            .map(|value| int_value(Some(value)))
            .unwrap_or_default();
        let kind = if id.eq_ignore_ascii_case(&root_id)
            || (is_dir && (parent_id.is_empty() || parent_id == "0"))
        {
            ItemKind::Root
        } else if is_dir {
            ItemKind::Folder
        } else {
            ItemKind::Note
        };
        let entry_props = entry.get("entryProps");
        let encrypted = entry_props
            .and_then(|x| x.get("encrypted"))
            .map(|value| bool_value(Some(value)))
            .unwrap_or(false);
        let item = Item {
            id: id.clone(),
            version,
            display_title: display_title(&title),
            title,
            parent_id,
            public_key: String::new(),
            public_link: String::new(),
            kind: kind.clone(),
            note_type: int_value(entry.get("noteType")),
            editor_type: int_value(entry.get("orgEditorType")),
            entry_path: None,
            resources: resources.clone(),
            summary: string_value(entry.get("summary")),
            size: int_value(entry.get("fileSize")),
            resource_size: int_value(entry.get("resourceSize")),
            encrypted,
            deleted: false,
            local_modified: false,
            created_at,
            modified_at,
        };
        let index = if let Some(index) = item_indices.get(&id.to_ascii_lowercase()).copied() {
            let old = &repo.items[index];
            let mut merged = item;
            merged.public_key = old.public_key.clone();
            merged.public_link = old.public_link.clone();
            merged.entry_path = old.entry_path.clone();
            if old.local_modified {
                merged.version = old.version;
                merged.title = old.title.clone();
                merged.display_title = old.display_title.clone();
                merged.parent_id = old.parent_id.clone();
                merged.entry_path = old.entry_path.clone();
                merged.resources = old.resources.clone();
                merged.summary = old.summary.clone();
                merged.size = old.size;
                merged.resource_size = old.resource_size;
                merged.modified_at = old.modified_at;
                merged.local_modified = true;
            }
            repo.items[index] = merged;
            index
        } else {
            repo.items.push(item);
            let index = repo.items.len() - 1;
            item_indices.insert(id.to_ascii_lowercase(), index);
            index
        };

        if kind == ItemKind::Note {
            let item = repo.items[index].clone();
            if item.local_modified {
                repo.fidelity_overrides.insert(
                    id.clone(),
                    "desktop_unsynced_raw_plus_normalized".to_string(),
                );
                continue;
            }
            if item.size > 0 {
                let raw_path = raw_root.join(format!("{}--v{}.json", safe_id(&id), version));
                if raw_path.is_file() {
                    bodies_reused += 1;
                } else {
                    let body = download_note(&auth, &id, version)?;
                    let _: Value = serde_json::from_slice(&body)
                        .with_context(|| format!("validate downloaded note body {id}"))?;
                    crate::atomic::write(&raw_path, &body)?;
                    bodies_downloaded += 1;
                }
                let raw: Value = serde_json::from_slice(&fs::read(&raw_path)?)?;
                repo.raw_paths.insert(id.clone(), raw_path);
                repo.raw_values.insert(id.clone(), raw);
                repo.fidelity_overrides
                    .insert(id.clone(), "cloud_raw_plus_normalized".to_string());
            }
            for reference in resources {
                let Some(version) = reference.version else {
                    continue;
                };
                let resource_id = reference.resource_id;
                let existing = repo.resources.get(&resource_id).cloned();
                let base_path =
                    resource_root.join(format!("{}--v{}", safe_id(&resource_id), version));
                let cached = find_cached_resource(&base_path);
                let path = if let Some(path) = cached {
                    resources_reused += 1;
                    path
                } else {
                    let bytes = download_resource(&auth, &resource_id, version)?;
                    let extension = detect_extension(&bytes);
                    let path = base_path.with_extension(extension);
                    crate::atomic::write(&path, &bytes)?;
                    resources_downloaded += 1;
                    path
                };
                let bytes = fs::read(&path)?;
                let extension = path.extension().and_then(|x| x.to_str()).unwrap_or("bin");
                let media_type = existing
                    .as_ref()
                    .map(|x| x.media_type.clone())
                    .filter(|x| !x.is_empty())
                    .unwrap_or_else(|| mime_for_extension(extension).to_string());
                let title = existing
                    .as_ref()
                    .map(|x| x.title.clone())
                    .filter(|x| !x.is_empty())
                    .unwrap_or_else(|| format!("resource.{extension}"));
                repo.resources.insert(
                    resource_id.clone(),
                    Resource {
                        id: resource_id,
                        title,
                        media_type,
                        size: bytes.len() as i64,
                        version,
                        entry: Some(path),
                        remote_url: None,
                        available: true,
                    },
                );
            }
        }
    }

    // Cloud metadata is authoritative for cloud IDs. Local-only/special shared entries remain.
    for item in &mut repo.items {
        if item.kind != ItemKind::Root
            && item.version > 0
            && !remote_active.contains(&item.id.to_ascii_lowercase())
            && item.parent_id != "-2"
        {
            item.deleted = true;
        }
    }
    rebuild_content_index(&mut repo);
    Ok((
        repo,
        CloudStats {
            metadata_entries,
            active_entries,
            deleted_entries,
            note_bodies_downloaded: bodies_downloaded,
            note_bodies_reused: bodies_reused,
            resources_downloaded,
            resources_reused,
            root_version,
        },
    ))
}

fn pull_all(auth: &DesktopAuth) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    let mut marker = String::new();
    loop {
        let body = winhttp::form_encode(&[
            ("baseVersion", "-1".to_string()),
            ("lastId", marker.clone()),
            ("erased", "0".to_string()),
            ("limit", "512".to_string()),
        ]);
        let response =
            authenticated_request(auth, "POST", "/yws/api/personal/sync", "pull", &body)?;
        ensure_success(&response, "pull metadata")?;
        let envelope: Value =
            serde_json::from_slice(&response.body).context("decode cloud metadata")?;
        if let Some(error) = envelope.get("error").filter(|x| !x.is_null()) {
            bail!("Youdao metadata pull returned an application error: {error}");
        }
        values.extend(
            envelope
                .get("values")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        );
        if !envelope
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            break;
        }
        marker = envelope
            .get("truncatedMarker")
            .and_then(Value::as_str)
            .context("metadata response omitted truncatedMarker")?
            .to_string();
    }
    Ok(values)
}

fn download_note(auth: &DesktopAuth, id: &str, version: i64) -> Result<Vec<u8>> {
    let body = winhttp::form_encode(&[
        ("fileId", id.to_string()),
        ("version", version.to_string()),
        ("convert", "true".to_string()),
        ("editorType", "1".to_string()),
        ("editorVersion", "new-json-editor".to_string()),
    ]);
    let response =
        authenticated_request(auth, "POST", "/yws/api/personal/sync", "download", &body)?;
    ensure_success(&response, "download note body")?;
    if response.body.is_empty() {
        bail!("Youdao returned an empty note body");
    }
    Ok(response.body)
}

fn download_resource(auth: &DesktopAuth, id: &str, version: i64) -> Result<Vec<u8>> {
    let path = format!("/yws/api/personal/resource/{}", winhttp::percent_encode(id));
    let response = authenticated_request(
        auth,
        "GET",
        &path,
        "getResource",
        &winhttp::form_encode(&[("version", version.to_string())]),
    )?;
    ensure_success(&response, "download note resource")?;
    if response.body.is_empty() {
        bail!("Youdao returned an empty resource");
    }
    Ok(response.body)
}

fn authenticated_request(
    auth: &DesktopAuth,
    method: &str,
    path: &str,
    api_method: &str,
    body_or_extra: &[u8],
) -> Result<winhttp::Response> {
    let request_id = format!("{}-{}", unix_now(), std::process::id());
    let mut url = format!(
        "https://note.youdao.com{path}?method={}&sev=j1&cstk={}&requestId={}",
        winhttp::percent_encode(api_method),
        winhttp::percent_encode(&auth.cstk),
        winhttp::percent_encode(&request_id)
    );
    let mut body = body_or_extra;
    if method == "GET" && !body_or_extra.is_empty() {
        url.push('&');
        url.push_str(std::str::from_utf8(body_or_extra).context("encode GET parameters")?);
        body = &[];
    }
    let headers = [
        ("Cookie", auth.cookie_header.as_str()),
        ("YNOTE-PC", auth.pc_header.as_str()),
        (
            "Content-Type",
            "application/x-www-form-urlencoded;charset=UTF-8",
        ),
    ];
    winhttp::request(method, &url, &headers, body).context("authenticated Youdao request")
}

fn ensure_success(response: &winhttp::Response, operation: &str) -> Result<()> {
    if response.status != 200 {
        bail!("{operation} returned HTTP {}", response.status);
    }
    Ok(())
}

impl DesktopAuth {
    fn load() -> Result<Self> {
        let setting = env::var_os("APPDATA")
            .map(PathBuf::from)
            .context("APPDATA is unavailable")?
            .join("ynote-desktop")
            .join("setting.json");
        let root: Value = serde_json::from_slice(
            &fs::read(&setting).with_context(|| format!("read {}", setting.display()))?,
        )
        .context("decode desktop client setting.json")?;
        if !root
            .get("isLogin")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bail!("the Youdao desktop client is not logged in");
        }
        let now = unix_now() as f64;
        let cookies = root
            .get("cookies")
            .and_then(Value::as_array)
            .context("desktop client cookies are unavailable")?;
        let mut serialized = Vec::new();
        let mut cstk = None;
        let mut pc = None;
        for cookie in cookies {
            let domain = cookie.get("domain").and_then(Value::as_str).unwrap_or("");
            let name = cookie.get("name").and_then(Value::as_str).unwrap_or("");
            let normalized_domain = domain.trim_start_matches('.');
            let valid_domain = normalized_domain == "note.youdao.com"
                || normalized_domain == "youdao.com"
                || (name == "YNOTE-PC" && domain.is_empty());
            let expired = cookie
                .get("expirationDate")
                .and_then(Value::as_f64)
                .is_some_and(|value| value <= now);
            if !valid_domain || expired {
                continue;
            }
            let Some(name) = cookie.get("name").and_then(Value::as_str) else {
                continue;
            };
            let value = cookie.get("value").and_then(Value::as_str).unwrap_or("");
            if name == "YNOTE_CSTK" {
                cstk = Some(value.to_string());
            }
            if name == "YNOTE-PC" {
                pc = Some(value.to_string());
            }
            serialized.push(format!("{name}={value}"));
        }
        Ok(Self {
            cookie_header: serialized.join("; "),
            pc_header: pc.context("desktop login is missing YNOTE-PC")?,
            cstk: cstk.context("desktop login is missing YNOTE_CSTK")?,
        })
    }
}

fn resource_ref(value: &Value) -> Option<ResourceRef> {
    Some(ResourceRef {
        resource_id: value.get("resourceId")?.as_str()?.to_string(),
        version: value.get("version").and_then(Value::as_i64),
        resource_type: value.get("resourceType").and_then(Value::as_i64),
        resource_sub_type: value.get("resourceSubType").and_then(Value::as_i64),
    })
}

fn string_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn int_value(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(value)) => value.as_i64().unwrap_or_default(),
        Some(Value::String(value)) => value.parse().unwrap_or_default(),
        Some(Value::Bool(value)) => i64::from(*value),
        _ => 0,
    }
}

fn bool_value(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_i64().unwrap_or_default() != 0,
        Some(Value::String(value)) => value == "1" || value.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn display_title(title: &str) -> String {
    for suffix in [".note", ".clip", ".drawio"] {
        if let Some(value) = title.strip_suffix(suffix) {
            return value.to_string();
        }
    }
    title.to_string()
}

fn safe_id(id: &str) -> String {
    id.chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .collect()
}

fn find_cached_resource(base: &Path) -> Option<PathBuf> {
    let parent = base.parent()?;
    let prefix = format!("{}.", base.file_name()?.to_string_lossy());
    fs::read_dir(parent)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let path = entry.path();
            path.file_name()
                .and_then(|x| x.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
                .then_some(path)
        })
}

fn rebuild_content_index(repo: &mut Repository) {
    let mut index = repo.content_index.clone();
    for item in repo
        .items
        .iter()
        .filter(|item| item.kind == ItemKind::Note && !item.deleted)
    {
        if let Ok(note) = repo.read_note(&item.id) {
            index.insert(
                item.id.clone(),
                format!("{}\n{}", item.title, note.markdown),
            );
        }
    }
    repo.content_index = index;
}

fn detect_extension(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "jpg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "webp"
    } else if bytes.starts_with(b"%PDF") {
        "pdf"
    } else {
        "bin"
    }
}

fn mime_for_extension(extension: &str) -> &'static str {
    match extension {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{bool_value, int_value, safe_id};
    use serde_json::json;

    #[test]
    fn parses_flexible_remote_scalars() {
        assert_eq!(int_value(Some(&json!("42"))), 42);
        assert!(bool_value(Some(&json!("true"))));
        assert_eq!(safe_id("WEB-1/../../x"), "WEB-1x");
    }
}
