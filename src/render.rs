use crate::model::{Item, NormalizedBlock, RenderedNote, Span};
use serde_json::{Map, Value};
use std::collections::HashMap;

pub fn render_note<F>(
    item: Item,
    path: Vec<String>,
    raw: Option<Value>,
    fallback: Option<String>,
    fidelity_override: Option<&str>,
    asset_url: F,
) -> RenderedNote
where
    F: Fn(&str) -> String,
{
    let (blocks, mut fidelity, raw_format) = if let Some(value) = &raw {
        (
            normalize_document(value),
            "lossless_raw_plus_normalized".to_string(),
            detect_raw_format(value).to_string(),
        )
    } else if let Some(text) = fallback {
        (
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| NormalizedBlock {
                    kind: "paragraph".into(),
                    text: line.to_string(),
                    spans: vec![Span {
                        text: line.to_string(),
                        href: None,
                        styles: Vec::new(),
                    }],
                    checked: None,
                    heading_level: None,
                    list_kind: None,
                    resource_id: None,
                    source_url: None,
                })
                .collect(),
            "search_index_fallback".to_string(),
            "plain_text".to_string(),
        )
    } else {
        (
            Vec::new(),
            "metadata_only_content_not_local".to_string(),
            "missing".to_string(),
        )
    };

    if let Some(override_value) = fidelity_override {
        fidelity = override_value.to_string();
    }
    let markdown = render_markdown(&item, &blocks, &asset_url, &fidelity);
    let html = render_html(&item, &blocks, &asset_url, &fidelity);
    RenderedNote {
        item,
        path,
        fidelity,
        raw_format,
        blocks,
        markdown,
        html,
        raw,
    }
}

pub fn resource_urls(value: &Value) -> HashMap<String, String> {
    let mut result = HashMap::new();
    collect_resource_urls(value, &mut result);
    result
}

fn collect_resource_urls(value: &Value, result: &mut HashMap<String, String>) {
    match value {
        Value::Object(object) => {
            for child in object.values() {
                if let Some(url) = child.as_str()
                    && let Some(id) = resource_id_from_url(url)
                    && url.starts_with("https://note.youdao.com/")
                {
                    result.entry(id).or_insert_with(|| url.to_string());
                }
                collect_resource_urls(child, result);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_resource_urls(child, result);
            }
        }
        _ => {}
    }
}

pub fn normalize_document(value: &Value) -> Vec<NormalizedBlock> {
    let Some(blocks) = value.get("5").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks.iter().map(normalize_block).collect()
}

fn normalize_block(value: &Value) -> NormalizedBlock {
    let object = value.as_object();
    let tag = object
        .and_then(|x| x.get("6"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let attrs = object.and_then(|x| x.get("4")).and_then(Value::as_object);
    let spans = collect_spans(value, None);
    let text = spans.iter().map(|span| span.text.as_str()).collect();
    let source_url = attrs
        .and_then(|x| string_attr(x, &["u", "src", "hf"]))
        .map(str::to_string);
    let resource_id = source_url.as_deref().and_then(resource_id_from_url);
    let (kind, checked, heading_level, list_kind) = match tag {
        "td" => (
            "todo",
            attrs.and_then(|x| x.get("c")).and_then(Value::as_bool),
            None,
            None,
        ),
        "h" => (
            "heading",
            None,
            attrs
                .and_then(|x| x.get("l"))
                .and_then(Value::as_str)
                .and_then(|level| level.trim_start_matches('h').parse::<u8>().ok()),
            None,
        ),
        "l" => (
            "list_item",
            None,
            None,
            attrs
                .and_then(|x| x.get("lt"))
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        "im" => ("image", None, None, None),
        "hr" => ("horizontal_rule", None, None, None),
        "pre" | "code" => ("code", None, None, None),
        "" => ("paragraph", None, None, None),
        other => (other, None, None, None),
    };
    NormalizedBlock {
        kind: kind.to_string(),
        text,
        spans,
        checked,
        heading_level,
        list_kind,
        resource_id,
        source_url,
    }
}

fn collect_spans(value: &Value, inherited_href: Option<&str>) -> Vec<Span> {
    match value {
        Value::Array(values) => values
            .iter()
            .flat_map(|value| collect_spans(value, inherited_href))
            .collect(),
        Value::Object(object) => {
            let own_href = object
                .get("4")
                .and_then(Value::as_object)
                .and_then(|attrs| string_attr(attrs, &["hf"]))
                .or(inherited_href);
            if let Some(text) = object.get("8").and_then(Value::as_str) {
                return vec![Span {
                    text: text.to_string(),
                    href: own_href.map(str::to_string),
                    styles: extract_styles(object.get("9")),
                }];
            }
            object
                .get("7")
                .or_else(|| object.get("5"))
                .map(|children| collect_spans(children, own_href))
                .unwrap_or_default()
        }
        Value::String(text) => vec![Span {
            text: text.clone(),
            href: inherited_href.map(str::to_string),
            styles: Vec::new(),
        }],
        _ => Vec::new(),
    }
}

fn extract_styles(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|style| {
            let object = style.as_object()?;
            let kind = object.get("2")?.as_str()?;
            match kind {
                "b" => Some("bold".to_string()),
                "i" => Some("italic".to_string()),
                "u" => Some("underline".to_string()),
                "s" => Some("strikethrough".to_string()),
                "c" => object
                    .get("0")
                    .and_then(Value::as_str)
                    .map(|value| format!("color:{value}")),
                "fs" => object.get("0").map(|value| format!("font-size:{value}")),
                "ff" => object
                    .get("0")
                    .and_then(Value::as_str)
                    .map(|value| format!("font-family:{value}")),
                other => Some(other.to_string()),
            }
        })
        .collect()
}

fn render_markdown<F>(
    item: &Item,
    blocks: &[NormalizedBlock],
    asset_url: &F,
    fidelity: &str,
) -> String
where
    F: Fn(&str) -> String,
{
    let mut output = format!(
        "<!-- ynote id={} original-title={:?} fidelity={} -->\n\n# {}\n\n",
        item.id, item.title, fidelity, item.display_title
    );
    for block in blocks {
        let inline = render_markdown_spans(&block.spans);
        match block.kind.as_str() {
            "todo" => {
                output.push_str(if block.checked.unwrap_or(false) {
                    "- [x] "
                } else {
                    "- [ ] "
                });
                output.push_str(&inline);
                output.push('\n');
            }
            "heading" => {
                let level = block.heading_level.unwrap_or(2).clamp(1, 6) as usize;
                output.push_str(&"#".repeat(level));
                output.push(' ');
                output.push_str(&inline);
                output.push_str("\n\n");
            }
            "list_item" => {
                if block.list_kind.as_deref() == Some("ordered") {
                    output.push_str("1. ");
                } else {
                    output.push_str("- ");
                }
                output.push_str(&inline);
                output.push('\n');
            }
            "image" => {
                if let Some(id) = &block.resource_id {
                    output.push_str(&format!(
                        "![{}]({})\n\n",
                        markdown_escape(&block.text),
                        asset_url(id)
                    ));
                } else if let Some(url) = &block.source_url {
                    output.push_str(&format!("![{}]({url})\n\n", markdown_escape(&block.text)));
                }
            }
            "horizontal_rule" => output.push_str("---\n\n"),
            "code" => output.push_str(&format!("```\n{}\n```\n\n", block.text)),
            _ => {
                if !inline.trim().is_empty() {
                    output.push_str(&inline);
                    output.push_str("\n\n");
                }
            }
        }
    }
    output
}

fn render_markdown_spans(spans: &[Span]) -> String {
    spans
        .iter()
        .map(|span| {
            let mut text = markdown_escape(&span.text);
            if span.styles.iter().any(|x| x == "bold") {
                text = format!("**{text}**");
            }
            if span.styles.iter().any(|x| x == "italic") {
                text = format!("*{text}*");
            }
            if span.styles.iter().any(|x| x == "strikethrough") {
                text = format!("~~{text}~~");
            }
            if let Some(href) = &span.href {
                text = format!("[{text}]({href})");
            }
            text
        })
        .collect()
}

fn render_html<F>(item: &Item, blocks: &[NormalizedBlock], asset_url: &F, fidelity: &str) -> String
where
    F: Fn(&str) -> String,
{
    let mut output = format!(
        "<article data-ynote-id=\"{}\" data-fidelity=\"{}\"><h1>{}</h1>",
        html_escape(&item.id),
        html_escape(fidelity),
        html_escape(&item.display_title)
    );
    let mut list_kind: Option<&str> = None;
    for block in blocks {
        let wanted_list = if block.kind == "list_item" {
            Some(if block.list_kind.as_deref() == Some("ordered") {
                "ol"
            } else {
                "ul"
            })
        } else {
            None
        };
        if list_kind != wanted_list {
            if let Some(tag) = list_kind {
                output.push_str(&format!("</{tag}>"));
            }
            if let Some(tag) = wanted_list {
                output.push_str(&format!("<{tag}>"));
            }
            list_kind = wanted_list;
        }
        let inline = render_html_spans(&block.spans);
        match block.kind.as_str() {
            "todo" => output.push_str(&format!(
                "<label class=\"todo\"><input type=\"checkbox\" disabled {}><span>{}</span></label>",
                if block.checked.unwrap_or(false) {
                    "checked"
                } else {
                    ""
                },
                inline
            )),
            "heading" => {
                let level = block.heading_level.unwrap_or(2).clamp(1, 6);
                output.push_str(&format!("<h{level}>{inline}</h{level}>"));
            }
            "list_item" => output.push_str(&format!("<li>{inline}</li>")),
            "image" => {
                let url = block
                    .resource_id
                    .as_deref()
                    .map(asset_url)
                    .or_else(|| block.source_url.clone())
                    .unwrap_or_default();
                output.push_str(&format!(
                    "<figure><img loading=\"lazy\" src=\"{}\" alt=\"{}\"></figure>",
                    html_escape(&url),
                    html_escape(&block.text)
                ));
            }
            "horizontal_rule" => output.push_str("<hr>"),
            "code" => output.push_str(&format!("<pre><code>{}</code></pre>", html_escape(&block.text))),
            _ => {
                if !inline.trim().is_empty() {
                    output.push_str(&format!("<p>{inline}</p>"));
                }
            }
        }
    }
    if let Some(tag) = list_kind {
        output.push_str(&format!("</{tag}>"));
    }
    if blocks.is_empty() {
        output.push_str(if fidelity == "confirmed_empty_body" {
            "<p class=\"missing\">此笔记的正文为空。</p>"
        } else {
            "<p class=\"missing\">笔记正文尚未同步到本地；当前仅有元数据。</p>"
        });
    }
    output.push_str("</article>");
    output
}

fn render_html_spans(spans: &[Span]) -> String {
    spans
        .iter()
        .map(|span| {
            let mut text = html_escape(&span.text);
            let style = span
                .styles
                .iter()
                .filter(|style| style.starts_with("color:") || style.starts_with("font-"))
                .cloned()
                .collect::<Vec<_>>()
                .join(";");
            if !style.is_empty() {
                text = format!("<span style=\"{}\">{text}</span>", html_escape(&style));
            }
            if span.styles.iter().any(|x| x == "bold") {
                text = format!("<strong>{text}</strong>");
            }
            if span.styles.iter().any(|x| x == "italic") {
                text = format!("<em>{text}</em>");
            }
            if span.styles.iter().any(|x| x == "underline") {
                text = format!("<u>{text}</u>");
            }
            if span.styles.iter().any(|x| x == "strikethrough") {
                text = format!("<s>{text}</s>");
            }
            if let Some(href) = &span.href {
                text = format!(
                    "<a href=\"{}\" target=\"_blank\" rel=\"noreferrer\">{text}</a>",
                    html_escape(href)
                );
            }
            text
        })
        .collect()
}

fn detect_raw_format(value: &Value) -> &'static str {
    if value
        .get("__compress__")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "ynote_compressed_json"
    } else {
        "json"
    }
}

fn string_attr<'a>(attrs: &'a Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| attrs.get(*name).and_then(Value::as_str))
}

pub fn resource_id_from_url(url: &str) -> Option<String> {
    url.rsplit('/')
        .find(|part| part.starts_with("WEBRESOURCE"))
        .map(|part| part.split(['?', '#']).next().unwrap_or(part).to_string())
}

fn markdown_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

pub fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemKind;
    use std::path::PathBuf;

    fn item() -> Item {
        Item {
            id: "WEB-test".into(),
            version: 1,
            title: "待办.note".into(),
            display_title: "待办".into(),
            parent_id: "root".into(),
            public_key: String::new(),
            public_link: String::new(),
            kind: ItemKind::Note,
            note_type: 0,
            editor_type: 1,
            entry_path: Some(PathBuf::from("fixture")),
            resources: Vec::new(),
            summary: String::new(),
            size: 0,
            resource_size: 0,
            encrypted: false,
            deleted: false,
            local_modified: false,
            created_at: 0,
            modified_at: 0,
        }
    }

    #[test]
    fn renders_todos_links_and_images() {
        let raw: Value = serde_json::from_str(
            r#"{"5":[
              {"6":"td","4":{"c":false},"5":[{"7":[{"8":"第一项"}]}]},
              {"6":"td","4":{"c":true},"5":[{"7":[{"8":"完成项"}]}]},
              {"5":[{"2":"3","6":"li","4":{"hf":"https://example.com"},"5":[{"7":[{"8":"链接"}]}]}]},
              {"6":"im","4":{"u":"https://note.youdao.com/yws/res/1/WEBRESOURCEabc"}}
            ],"__compress__":true}"#,
        )
        .unwrap();
        let note = render_note(item(), vec!["待办".into()], Some(raw), None, None, |id| {
            format!("assets/{id}.png")
        });
        assert_eq!(note.blocks.len(), 4);
        assert!(note.markdown.contains("- [ ] 第一项"));
        assert!(note.markdown.contains("- [x] 完成项"));
        assert!(note.markdown.contains("[链接](https://example.com)"));
        assert!(note.markdown.contains("assets/WEBRESOURCEabc.png"));
        assert_eq!(note.fidelity, "lossless_raw_plus_normalized");
    }
}
