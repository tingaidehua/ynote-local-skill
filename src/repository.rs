use crate::model::*;
use crate::render;
use crate::sqlite::Connection;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub struct NoteSource {
    pub raw: Option<Value>,
    pub fidelity_override: Option<String>,
    pub fallback: Option<String>,
}

#[derive(Clone)]
pub struct Repository {
    pub source: SourceInfo,
    pub items: Vec<Item>,
    pub resources: BTreeMap<String, Resource>,
    pub content_index: HashMap<String, String>,
    pub(crate) raw_paths: HashMap<String, PathBuf>,
    pub(crate) raw_values: HashMap<String, Value>,
    pub(crate) fidelity_overrides: HashMap<String, String>,
}

impl Repository {
    pub fn discover(data_root: Option<PathBuf>, account: Option<String>) -> Result<Self> {
        let base = data_root.unwrap_or_else(default_app_data);
        let candidates = discover_accounts(&base)?;
        if candidates.is_empty() {
            bail!(
                "No local Youdao Note account database was found under {}",
                base.display()
            );
        }
        let (account_id, ynote_data, database) = if let Some(wanted) = account {
            candidates
                .into_iter()
                .find(|(id, _, _)| id == &wanted)
                .ok_or_else(|| anyhow!("Account {wanted} was not found under {}", base.display()))?
        } else if candidates.len() == 1 {
            candidates.into_iter().next().unwrap()
        } else {
            let names = candidates
                .iter()
                .map(|x| x.0.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("Multiple local accounts found ({names}); pass --account");
        };
        let content_database = {
            let path = ynote_data.join(format!("{account_id}-content.db"));
            path.exists().then_some(path)
        };
        let source = SourceInfo {
            account: account_id,
            data_root: ynote_data.clone(),
            database,
            content_database,
            client_executable: PathBuf::from(r"C:\Program Files\ynote-desktop\有道云笔记.exe"),
        };
        Self::load(source)
    }

    fn load(source: SourceInfo) -> Result<Self> {
        let db = Connection::open_readonly(&source.database)?;
        let item_sql = r#"
SELECT 'root', fileId, title, parentId, noteType, orgEditorType, entryPath, resources,
       summary, size, resourceSize, encrypted, deleted, createTime, modifyTime, public_key, public_link, version, localModified
FROM root
UNION ALL
SELECT 'folder', fileId, title, parentId, noteType, orgEditorType, entryPath, resources,
       summary, size, resourceSize, encrypted, deleted, createTime, modifyTime, public_key, public_link, version, localModified
FROM note_book
UNION ALL
SELECT 'note', fileId, title, parentId, noteType, orgEditorType, entryPath, resources,
       summary, size, resourceSize, encrypted, deleted, createTime, modifyTime, public_key, public_link, version, localModified
FROM note
"#;
        let mut items = Vec::new();
        for row in db.query(item_sql)? {
            let get = |i: usize| row.get(i).and_then(Clone::clone).unwrap_or_default();
            let kind = match get(0).as_str() {
                "root" => ItemKind::Root,
                "folder" => ItemKind::Folder,
                _ => ItemKind::Note,
            };
            let title = get(2);
            items.push(Item {
                id: get(1),
                version: parse_i64(&get(17)),
                display_title: display_title(&title),
                title,
                parent_id: get(3),
                public_key: get(15),
                public_link: get(16),
                kind,
                note_type: parse_i64(&get(4)),
                editor_type: parse_i64(&get(5)),
                entry_path: nonempty_path(get(6)),
                resources: parse_resource_refs(&get(7)),
                summary: get(8),
                size: parse_i64(&get(9)),
                resource_size: parse_i64(&get(10)),
                encrypted: parse_bool(&get(11)),
                deleted: parse_bool(&get(12)),
                local_modified: parse_bool(&get(18)),
                created_at: parse_i64(&get(13)),
                modified_at: parse_i64(&get(14)),
            });
        }
        let resource_sql = r#"
SELECT resourceID, title, mediaType, size, version, entry, remoteUrl
FROM resource
"#;
        let mut resources = BTreeMap::new();
        for row in db.query(resource_sql)? {
            let get = |i: usize| row.get(i).and_then(Clone::clone).unwrap_or_default();
            let entry = nonempty_path(get(5));
            let resource = Resource {
                id: get(0),
                title: get(1),
                media_type: get(2),
                size: parse_i64(&get(3)),
                version: parse_i64(&get(4)),
                available: entry.as_ref().is_some_and(|p| p.is_file()),
                entry,
                remote_url: nonempty(get(6)),
            };
            resources.insert(resource.id.clone(), resource);
        }
        let mut content_index = HashMap::new();
        if let Some(path) = &source.content_database
            && let Ok(content_db) = Connection::open_readonly(path)
            && let Ok(rows) = content_db
                .query("SELECT fileId, title, content FROM contenttable WHERE erased != '1'")
        {
            for row in rows {
                let id = row.first().and_then(Clone::clone).unwrap_or_default();
                let title = row.get(1).and_then(Clone::clone).unwrap_or_default();
                let content = row.get(2).and_then(Clone::clone).unwrap_or_default();
                content_index.insert(id, format!("{title}\n{content}"));
            }
        }
        let raw_paths = index_raw_files(&source.data_root.join("file"))?;
        Ok(Self {
            source,
            items,
            resources,
            content_index,
            raw_paths,
            raw_values: HashMap::new(),
            fidelity_overrides: HashMap::new(),
        })
    }

    pub fn root_id(&self) -> Option<&str> {
        self.items
            .iter()
            .find(|x| x.kind == ItemKind::Root)
            .map(|x| x.id.as_str())
    }

    pub fn item(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|x| x.id.eq_ignore_ascii_case(id))
    }

    pub fn raw_path(&self, item: &Item) -> Option<PathBuf> {
        if let Some(path) = &item.entry_path
            && is_nonempty_file(path)
        {
            return Some(path.clone());
        }
        self.raw_paths
            .get(&item.id)
            .filter(|path| is_nonempty_file(path))
            .cloned()
    }

    pub fn raw_available(&self, item: &Item) -> bool {
        self.raw_values.contains_key(&item.id) || self.raw_path(item).is_some()
    }

    pub fn path_for(&self, id: &str) -> Vec<String> {
        let by_id: HashMap<&str, &Item> = self
            .items
            .iter()
            .map(|item| (item.id.as_str(), item))
            .collect();
        let mut path = Vec::new();
        let mut cursor = by_id.get(id).copied();
        let mut safety = 0;
        while let Some(item) = cursor {
            if item.kind != ItemKind::Root {
                path.push(item.display_title.clone());
            }
            cursor = by_id.get(item.parent_id.as_str()).copied();
            safety += 1;
            if safety > 256 {
                break;
            }
        }
        path.reverse();
        path
    }

    pub fn tree(&self) -> Vec<TreeNode> {
        let mut children: HashMap<&str, Vec<&Item>> = HashMap::new();
        for item in self.items.iter().filter(|x| !x.deleted) {
            children
                .entry(item.parent_id.as_str())
                .or_default()
                .push(item);
        }
        for values in children.values_mut() {
            values.sort_by(|a, b| {
                kind_order(&a.kind)
                    .cmp(&kind_order(&b.kind))
                    .then_with(|| a.display_title.cmp(&b.display_title))
            });
        }
        let root_id = self.root_id().unwrap_or("0");
        let mut tree = self.build_children(root_id, &children, 0);
        let mut reachable = std::collections::HashSet::new();
        collect_tree_ids(&tree, &mut reachable);
        let mut detached = self
            .items
            .iter()
            .filter(|item| {
                !item.deleted
                    && item.kind != ItemKind::Root
                    && !reachable.contains(item.id.as_str())
                    && !self.items.iter().any(|parent| {
                        !parent.deleted
                            && parent.id.eq_ignore_ascii_case(&item.parent_id)
                            && !reachable.contains(parent.id.as_str())
                    })
            })
            .map(|item| TreeNode {
                item: item.clone(),
                raw_available: self.raw_available(item),
                children: self.build_children(&item.id, &children, 0),
            })
            .collect::<Vec<_>>();
        detached.sort_by(|a, b| a.item.display_title.cmp(&b.item.display_title));
        tree.extend(detached);
        tree
    }

    fn build_children(
        &self,
        parent: &str,
        children: &HashMap<&str, Vec<&Item>>,
        depth: usize,
    ) -> Vec<TreeNode> {
        if depth > 256 {
            return Vec::new();
        }
        children
            .get(parent)
            .into_iter()
            .flatten()
            .map(|item| TreeNode {
                item: (*item).clone(),
                raw_available: self.raw_available(item),
                children: self.build_children(&item.id, children, depth + 1),
            })
            .collect()
    }

    pub fn read_note(&self, id: &str) -> Result<RenderedNote> {
        let item = self
            .item(id)
            .ok_or_else(|| anyhow!("No note with id {id}"))?
            .clone();
        if item.kind != ItemKind::Note {
            bail!("{id} is not a note");
        }
        let source = self.note_source(&item)?;
        let remote_urls = source
            .raw
            .as_ref()
            .map(render::resource_urls)
            .unwrap_or_default();
        Ok(render::render_note(
            item,
            self.path_for(id),
            source.raw,
            source.fallback,
            source.fidelity_override.as_deref(),
            |resource_id| {
                if self
                    .resources
                    .get(resource_id)
                    .is_some_and(|resource| resource.available)
                {
                    format!("/api/assets/{resource_id}")
                } else {
                    remote_urls
                        .get(resource_id)
                        .cloned()
                        .unwrap_or_else(|| format!("missing-resource:{resource_id}"))
                }
            },
        ))
    }

    pub fn note_source(&self, item: &Item) -> Result<NoteSource> {
        if let Some(raw) = self.raw_values.get(&item.id) {
            return Ok(NoteSource {
                raw: Some(raw.clone()),
                fidelity_override: self.fidelity_overrides.get(&item.id).cloned(),
                fallback: None,
            });
        }
        if let Some(path) = self.raw_path(item) {
            let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            if let Ok(raw) = serde_json::from_slice::<Value>(&bytes) {
                return Ok(NoteSource {
                    raw: Some(raw),
                    fidelity_override: item
                        .local_modified
                        .then(|| "desktop_unsynced_raw_plus_normalized".to_string())
                        .or_else(|| self.fidelity_overrides.get(&item.id).cloned()),
                    fallback: None,
                });
            }
        }
        if !item.public_key.is_empty() {
            match fetch_public_share(&item.public_key) {
                Ok(raw) => {
                    return Ok(NoteSource {
                        raw: Some(raw),
                        fidelity_override: Some("public_share_raw_plus_normalized".to_string()),
                        fallback: None,
                    });
                }
                Err(error) => {
                    let fallback = self.content_index.get(&item.id).cloned();
                    return Ok(NoteSource {
                        raw: None,
                        fidelity_override: fallback
                            .is_none()
                            .then(|| format!("public_share_fetch_failed:{error}")),
                        fallback,
                    });
                }
            }
        }
        if item.size == 0 {
            return Ok(NoteSource {
                raw: None,
                fidelity_override: Some("confirmed_empty_body".to_string()),
                fallback: None,
            });
        }
        Ok(NoteSource {
            raw: None,
            fidelity_override: None,
            fallback: self.content_index.get(&item.id).cloned(),
        })
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let needle = query.to_lowercase();
        let mut hits = Vec::new();
        for item in self
            .items
            .iter()
            .filter(|x| x.kind == ItemKind::Note && !x.deleted)
        {
            let haystack = self
                .content_index
                .get(&item.id)
                .cloned()
                .unwrap_or_else(|| format!("{}\n{}", item.title, item.summary));
            if let Some(byte_index) = haystack.to_lowercase().find(&needle) {
                let start = nearest_char_boundary(&haystack, byte_index.saturating_sub(80));
                let end = nearest_char_boundary(
                    &haystack,
                    (byte_index + query.len() + 160).min(haystack.len()),
                );
                hits.push(SearchHit {
                    id: item.id.clone(),
                    title: item.display_title.clone(),
                    path: self.path_for(&item.id),
                    snippet: haystack[start..end].replace(['\r', '\n'], " "),
                });
                if hits.len() >= limit {
                    break;
                }
            }
        }
        hits
    }
}

fn collect_tree_ids<'a>(nodes: &'a [TreeNode], output: &mut std::collections::HashSet<&'a str>) {
    for node in nodes {
        output.insert(node.item.id.as_str());
        collect_tree_ids(&node.children, output);
    }
}

fn discover_accounts(base: &Path) -> Result<Vec<(String, PathBuf, PathBuf)>> {
    let mut roots = Vec::new();
    if base.file_name().is_some_and(|x| x == "ynote-data") {
        roots.push(base.to_path_buf());
    } else if base.is_dir() {
        for entry in fs::read_dir(base).with_context(|| format!("read {}", base.display()))? {
            let path = entry?.path().join("ynote-data");
            if path.is_dir() {
                roots.push(path);
            }
        }
    }
    let mut result = Vec::new();
    for root in roots {
        for entry in fs::read_dir(&root)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|x| x.to_str()) else {
                continue;
            };
            if path.extension().and_then(|x| x.to_str()) != Some("db")
                || name.ends_with("-content.db")
                || name.ends_with("-search.db")
            {
                continue;
            }
            let account = name.trim_end_matches(".db").to_string();
            if root.join(format!("{account}-content.db")).exists() {
                result.push((account, root.clone(), path));
            }
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

fn default_app_data() -> PathBuf {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ynote-desktop")
}

fn index_raw_files(file_root: &Path) -> Result<HashMap<String, PathBuf>> {
    let mut result = HashMap::new();
    if !file_root.is_dir() {
        return Ok(result);
    }
    for bucket in fs::read_dir(file_root)? {
        let bucket = bucket?.path();
        if !bucket.is_dir() {
            continue;
        }
        for entry in fs::read_dir(bucket)? {
            let path = entry?.path();
            if path.is_file()
                && let Some(name) = path.file_name().and_then(|x| x.to_str())
            {
                result.insert(name.to_string(), path);
            }
        }
    }
    Ok(result)
}

fn parse_resource_refs(value: &str) -> Vec<ResourceRef> {
    let Ok(Value::Array(values)) = serde_json::from_str::<Value>(value) else {
        return Vec::new();
    };
    values
        .into_iter()
        .filter_map(|value| {
            Some(ResourceRef {
                resource_id: value.get("resourceId")?.as_str()?.to_string(),
                version: value.get("version").and_then(Value::as_i64),
                resource_type: value.get("resourceType").and_then(Value::as_i64),
                resource_sub_type: value.get("resourceSubType").and_then(Value::as_i64),
            })
        })
        .collect()
}

fn parse_i64(value: &str) -> i64 {
    value.parse().unwrap_or_default()
}

fn parse_bool(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn nonempty_path(value: String) -> Option<PathBuf> {
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn display_title(title: &str) -> String {
    for suffix in [".note", ".clip", ".drawio"] {
        if let Some(value) = title.strip_suffix(suffix) {
            return value.to_string();
        }
    }
    title.to_string()
}

fn kind_order(kind: &ItemKind) -> u8 {
    match kind {
        ItemKind::Root => 0,
        ItemKind::Folder => 1,
        ItemKind::Note => 2,
    }
}

fn nearest_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn is_nonempty_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

fn fetch_public_share(public_key: &str) -> Result<Value> {
    if !public_key
        .chars()
        .all(|character| character.is_ascii_alphanumeric())
    {
        bail!("invalid public share key");
    }
    let url = format!(
        "https://note.youdao.com/yws/api/note/{public_key}?sev=j1&editorType=1&editorVersion=new-json-editor"
    );
    let response = crate::winhttp::get(&url).context("fetch public share content")?;
    if response.status != 200 {
        bail!("public share endpoint returned HTTP {}", response.status);
    }
    let envelope: Value =
        serde_json::from_slice(&response.body).context("decode public share envelope")?;
    let content = envelope
        .get("content")
        .and_then(Value::as_str)
        .context("public share response did not contain content")?;
    serde_json::from_str(content).context("decode public share note JSON")
}
