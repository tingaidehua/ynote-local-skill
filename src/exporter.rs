use crate::model::*;
use crate::render;
use crate::repository::Repository;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn export(repo: &Repository, output: &Path) -> Result<ExportManifest> {
    fs::create_dir_all(output).with_context(|| format!("create {}", output.display()))?;
    let internal = output.join("_ynote");
    let raw_root = internal.join("raw");
    let resource_root = internal.join("resources");
    fs::create_dir_all(&raw_root)?;
    fs::create_dir_all(&resource_root)?;

    let path_map = build_path_map(repo);
    let mut exported_resources = BTreeMap::new();
    let mut resource_paths = HashMap::new();
    for resource in repo.resources.values() {
        let file_name = resource_file_name(resource);
        let destination = resource_root.join(&file_name);
        let relative = destination
            .strip_prefix(output)
            .unwrap_or(&destination)
            .to_string_lossy()
            .replace('\\', "/");
        let (sha256, available, relative_path) =
            if let Some(source) = resource.entry.as_ref().filter(|path| path.is_file()) {
                crate::atomic::write(
                    &destination,
                    &fs::read(source).with_context(|| format!("read {}", source.display()))?,
                )?;
                (Some(hash_file(&destination)?), true, Some(relative.clone()))
            } else {
                (None, false, None)
            };
        if available {
            resource_paths.insert(resource.id.clone(), destination);
        }
        exported_resources.insert(
            resource.id.clone(),
            ExportedResource {
                title: resource.title.clone(),
                media_type: resource.media_type.clone(),
                relative_path,
                size: resource.size,
                sha256,
                available,
            },
        );
    }

    let mut exported_items = BTreeMap::new();
    let mut warnings = Vec::new();
    for item in repo.items.iter().filter(|item| !item.deleted) {
        if item.kind == ItemKind::Root {
            continue;
        }
        let relative = path_map.get(&item.id).cloned().unwrap_or_default();
        let destination = output.join(&relative);
        if item.kind == ItemKind::Folder {
            fs::create_dir_all(&destination)?;
            exported_items.insert(
                item.id.clone(),
                ExportedItem {
                    title: item.title.clone(),
                    kind: item.kind.clone(),
                    parent_id: item.parent_id.clone(),
                    relative_path: normalize_path(&relative),
                    raw_relative_path: None,
                    fidelity: None,
                    text_sha256: None,
                },
            );
            continue;
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw_path = repo.raw_path(item);
        let source = repo.note_source(item)?;
        let raw_value = source.raw;
        let remote_urls = raw_value
            .as_ref()
            .map(render::resource_urls)
            .unwrap_or_default();
        for (resource_id, url) in &remote_urls {
            if resource_paths.contains_key(resource_id) {
                continue;
            }
            if let Some(destination) = find_public_resource(&resource_root, resource_id) {
                let extension = destination
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("bin");
                let relative =
                    normalize_path(destination.strip_prefix(output).unwrap_or(&destination));
                let size = fs::metadata(&destination)?.len() as i64;
                resource_paths.insert(resource_id.clone(), destination.clone());
                exported_resources.insert(
                    resource_id.clone(),
                    ExportedResource {
                        title: format!("public-resource.{extension}"),
                        media_type: mime_for_extension(extension).to_string(),
                        relative_path: Some(relative),
                        size,
                        sha256: Some(hash_file(&destination)?),
                        available: true,
                    },
                );
                continue;
            }
            match crate::winhttp::get(url) {
                Ok(response) if response.status == 200 && !response.body.is_empty() => {
                    let extension = detect_extension(&response.body);
                    let file_name = format!("{resource_id}--public-resource.{extension}");
                    let destination = resource_root.join(file_name);
                    crate::atomic::write(&destination, &response.body)?;
                    let relative =
                        normalize_path(destination.strip_prefix(output).unwrap_or(&destination));
                    let sha256 = format!("{:x}", Sha256::digest(&response.body));
                    resource_paths.insert(resource_id.clone(), destination);
                    exported_resources.insert(
                        resource_id.clone(),
                        ExportedResource {
                            title: format!("public-resource.{extension}"),
                            media_type: mime_for_extension(extension).to_string(),
                            relative_path: Some(relative),
                            size: response.body.len() as i64,
                            sha256: Some(sha256),
                            available: true,
                        },
                    );
                }
                Ok(response) => warnings.push(format!(
                    "public resource {resource_id} returned HTTP {} or an empty body",
                    response.status
                )),
                Err(error) => warnings.push(format!(
                    "public resource {resource_id} could not be fetched: {error}"
                )),
            }
        }
        let note_parent = destination.parent().unwrap_or(output).to_path_buf();
        let rendered = render::render_note(
            item.clone(),
            repo.path_for(&item.id),
            raw_value.clone(),
            source.fallback,
            source.fidelity_override.as_deref(),
            |resource_id| {
                resource_paths
                    .get(resource_id)
                    .and_then(|asset| pathdiff::diff_paths(asset, &note_parent))
                    .map(|path| normalize_path(&path))
                    .unwrap_or_else(|| format!("missing-resource:{resource_id}"))
            },
        );
        crate::atomic::write(&destination, rendered.markdown.as_bytes())
            .with_context(|| format!("write {}", destination.display()))?;
        let text_sha256 = format!("{:x}", Sha256::digest(rendered.markdown.as_bytes()));

        let structured_path = destination.with_extension("ynote.json");
        crate::atomic::write(&structured_path, &serde_json::to_vec_pretty(&rendered)?)?;

        let raw_relative_path = if let Some(source) = raw_path {
            let extension = if raw_value.is_some() { "json" } else { "bin" };
            let destination = raw_root.join(format!("{}.{}", item.id, extension));
            crate::atomic::write(&destination, &fs::read(&source)?)?;
            Some(normalize_path(
                destination.strip_prefix(output).unwrap_or(&destination),
            ))
        } else if let Some(raw) = raw_value {
            let destination = raw_root.join(format!("{}.public.json", item.id));
            crate::atomic::write(&destination, &serde_json::to_vec(&raw)?)?;
            Some(normalize_path(
                destination.strip_prefix(output).unwrap_or(&destination),
            ))
        } else if rendered.fidelity == "confirmed_empty_body" {
            None
        } else {
            warnings.push(format!(
                "{} ({}) has metadata but its body is not currently cached locally",
                item.display_title, item.id
            ));
            None
        };
        exported_items.insert(
            item.id.clone(),
            ExportedItem {
                title: item.title.clone(),
                kind: item.kind.clone(),
                parent_id: item.parent_id.clone(),
                relative_path: normalize_path(&relative),
                raw_relative_path,
                fidelity: Some(rendered.fidelity),
                text_sha256: Some(text_sha256),
            },
        );
    }

    let manifest = ExportManifest {
        format_version: 2,
        generated_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        source: repo.source.clone(),
        item_count: repo.items.iter().filter(|x| !x.deleted).count(),
        note_count: repo
            .items
            .iter()
            .filter(|x| x.kind == ItemKind::Note && !x.deleted)
            .count(),
        folder_count: repo
            .items
            .iter()
            .filter(|x| x.kind == ItemKind::Folder && !x.deleted)
            .count(),
        resource_count: exported_resources.len(),
        items: exported_items,
        resources: exported_resources,
        warnings,
    };
    crate::atomic::write(
        &output.join(".ynote-manifest.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

fn find_public_resource(root: &Path, id: &str) -> Option<PathBuf> {
    let prefix = format!("{id}--public-resource.");
    fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let path = entry.path();
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
                .then_some(path)
        })
}

fn build_path_map(repo: &Repository) -> HashMap<String, PathBuf> {
    let mut by_parent: HashMap<&str, Vec<&Item>> = HashMap::new();
    for item in repo.items.iter().filter(|x| !x.deleted) {
        by_parent
            .entry(item.parent_id.as_str())
            .or_default()
            .push(item);
    }
    let mut output = HashMap::new();
    let root_id = repo.root_id().unwrap_or("0");
    assign_paths(root_id, Path::new(""), &by_parent, &mut output, 0);
    // Shared/imported notes can legitimately have special parents such as "-2".
    // Keep them visible without pretending that they belong to the user's root.
    for item in repo
        .items
        .iter()
        .filter(|item| !item.deleted && item.kind != ItemKind::Root)
    {
        if output.contains_key(&item.id) {
            continue;
        }
        let prefix: String = item.id.chars().take(8).collect();
        let name = if item.kind == ItemKind::Folder {
            format!("{}--{}", sanitize_name(&item.display_title), prefix)
        } else {
            format!("{}--{}.md", sanitize_name(&item.display_title), prefix)
        };
        output.insert(item.id.clone(), Path::new("_unfiled").join(name));
    }
    output
}

fn assign_paths(
    parent_id: &str,
    parent_path: &Path,
    children: &HashMap<&str, Vec<&Item>>,
    output: &mut HashMap<String, PathBuf>,
    depth: usize,
) {
    if depth > 256 {
        return;
    }
    let mut used = HashSet::new();
    let mut values = children.get(parent_id).cloned().unwrap_or_default();
    values.sort_by(|a, b| a.display_title.cmp(&b.display_title));
    for item in values {
        let base = sanitize_name(&item.display_title);
        let mut name = if item.kind == ItemKind::Folder {
            base
        } else {
            format!("{base}.md")
        };
        let folded = name.to_lowercase();
        if !used.insert(folded) {
            let prefix: String = item.id.chars().take(8).collect();
            name = if item.kind == ItemKind::Folder {
                format!("{}--{}", sanitize_name(&item.display_title), prefix)
            } else {
                format!("{}--{}.md", sanitize_name(&item.display_title), prefix)
            };
            used.insert(name.to_lowercase());
        }
        let path = parent_path.join(name);
        output.insert(item.id.clone(), path.clone());
        if item.kind == ItemKind::Folder {
            assign_paths(&item.id, &path, children, output, depth + 1);
        }
    }
}

pub fn sanitize_name(value: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| match character {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '＿',
            control if control.is_control() => ' ',
            other => other,
        })
        .collect::<String>()
        .trim()
        .trim_end_matches(['.', ' '])
        .to_string();
    if result.is_empty() {
        result = "未命名".to_string();
    }
    let upper = result.to_uppercase();
    if matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        result.push('＿');
    }
    result
}

fn resource_file_name(resource: &Resource) -> String {
    let title = sanitize_name(&resource.title);
    let title = if title == "未命名" {
        let ext = mime_guess::get_mime_extensions_str(&resource.media_type)
            .and_then(|values| values.first().copied())
            .unwrap_or("bin");
        format!("resource.{ext}")
    } else {
        title
    };
    format!("{}--{}", resource.id, title)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
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
    } else if bytes.starts_with(b"<svg") || bytes.windows(4).any(|value| value == b"<svg") {
        "svg"
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
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_name;

    #[test]
    fn sanitizes_windows_names() {
        assert_eq!(sanitize_name("a:b?c"), "a＿b＿c");
        assert_eq!(sanitize_name("CON"), "CON＿");
        assert_eq!(sanitize_name("x. "), "x");
    }
}
