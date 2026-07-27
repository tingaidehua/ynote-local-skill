use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Root,
    Folder,
    Note,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    #[serde(default)]
    pub version: i64,
    pub title: String,
    pub display_title: String,
    pub parent_id: String,
    pub public_key: String,
    pub public_link: String,
    pub kind: ItemKind,
    pub note_type: i64,
    pub editor_type: i64,
    pub entry_path: Option<PathBuf>,
    pub resources: Vec<ResourceRef>,
    pub summary: String,
    pub size: i64,
    pub resource_size: i64,
    pub encrypted: bool,
    pub deleted: bool,
    #[serde(default)]
    pub local_modified: bool,
    pub created_at: i64,
    pub modified_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRef {
    pub resource_id: String,
    pub version: Option<i64>,
    pub resource_type: Option<i64>,
    pub resource_sub_type: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub id: String,
    pub title: String,
    pub media_type: String,
    pub size: i64,
    pub version: i64,
    pub entry: Option<PathBuf>,
    pub remote_url: Option<String>,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub account: String,
    pub data_root: PathBuf,
    pub database: PathBuf,
    pub content_database: Option<PathBuf>,
    pub client_executable: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeNode {
    #[serde(flatten)]
    pub item: Item,
    pub raw_available: bool,
    pub children: Vec<TreeNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub styles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedBlock {
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading_level: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedNote {
    pub item: Item,
    pub path: Vec<String>,
    pub fidelity: String,
    pub raw_format: String,
    pub blocks: Vec<NormalizedBlock>,
    pub markdown: String,
    pub html: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    pub path: Vec<String>,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportManifest {
    pub format_version: u32,
    pub generated_at_unix: u64,
    pub source: SourceInfo,
    pub item_count: usize,
    pub note_count: usize,
    pub folder_count: usize,
    pub resource_count: usize,
    pub items: BTreeMap<String, ExportedItem>,
    pub resources: BTreeMap<String, ExportedResource>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedItem {
    pub title: String,
    pub kind: ItemKind,
    pub parent_id: String,
    pub relative_path: String,
    pub raw_relative_path: Option<String>,
    pub fidelity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedResource {
    pub title: String,
    pub media_type: String,
    pub relative_path: Option<String>,
    pub size: i64,
    pub sha256: Option<String>,
    pub available: bool,
}
