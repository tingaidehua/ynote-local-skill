mod atomic;
mod cloud;
mod console;
mod daemon;
mod exporter;
mod mirror;
mod model;
mod render;
mod repository;
mod sqlite;
mod syncer;
mod web;
mod winhttp;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use model::{ItemKind, TreeNode};
use repository::Repository;
use serde::Serialize;
use serde_json::json;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "ynote-cli",
    version,
    about = "Local-first mirror and AI-readable interface for Youdao Note",
    long_about = "Uses the logged-in Windows desktop session without a developer key. Pulls cloud changes read-only, maintains an unencrypted SQLite mirror, exports Markdown/JSON/assets, and serves a localhost web UI."
)]
struct Cli {
    /// ynote-desktop app-data root or a concrete ynote-data directory
    #[arg(long, global = true)]
    data_root: Option<PathBuf>,

    /// Local account directory/database basename
    #[arg(long, global = true)]
    account: Option<String>,

    /// Read tree/note/search commands from a ynote-mirror.sqlite file
    #[arg(long, global = true)]
    mirror: Option<PathBuf>,

    /// Pretty-print JSON output
    #[arg(long, global = true)]
    pretty: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Diagnose client login, local data, mirror and capabilities
    Doctor,
    /// Print the complete notebook/folder/note tree
    Tree {
        #[arg(long)]
        text: bool,
    },
    /// List direct children of a folder (defaults to root)
    List {
        #[arg(long)]
        parent: Option<String>,
    },
    /// Read one note as structured JSON, Markdown, HTML or raw JSON
    Read {
        id: String,
        #[arg(long, value_enum, default_value_t = ReadFormat::Structured)]
        output_format: ReadFormat,
    },
    /// Search normalized note content
    Search {
        query: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// List resource metadata
    Resources {
        #[arg(long)]
        note: Option<String>,
    },
    /// Export the currently selected local/mirror repository
    Export {
        #[arg(long)]
        output: PathBuf,
    },
    /// Refresh cloud/local data into SQLite + Markdown; optionally keep polling
    Sync {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = syncer::DEFAULT_INTERVAL_SECONDS)]
        interval: u64,
        #[arg(long, default_value_t = syncer::DEFAULT_JITTER_SECONDS)]
        jitter: u64,
        /// Do not contact the cloud; mirror only the desktop client's current cache
        #[arg(long)]
        local_only: bool,
    },
    /// Inspect or refresh the durable SQLite mirror
    Mirror {
        #[command(subcommand)]
        command: MirrorCommand,
    },
    /// Run/install the low-frequency synchronizer and live web UI
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Inspect captured external Markdown edits; cloud apply is intentionally disabled
    Writeback {
        #[command(subcommand)]
        command: WritebackCommand,
    },
    /// Serve a local tree/search/note web interface
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 4768)]
        port: u16,
        #[arg(long)]
        open: bool,
    },
}

#[derive(Subcommand)]
enum MirrorCommand {
    /// Pull current cloud state and rebuild the AI-readable mirror
    Refresh {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        local_only: bool,
    },
    /// Show integrity, last successful sync and row counts
    Status {
        #[arg(long)]
        output: PathBuf,
    },
    /// Run one explicitly read-only SQL query
    Query {
        #[arg(long)]
        output: PathBuf,
        sql: String,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Run sync + live localhost web in the foreground
    Run {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = syncer::DEFAULT_INTERVAL_SECONDS)]
        interval: u64,
        #[arg(long, default_value_t = syncer::DEFAULT_JITTER_SECONDS)]
        jitter: u64,
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 4768)]
        port: u16,
        #[arg(long)]
        local_only: bool,
    },
    /// Install a no-admin current-user Windows logon startup
    Install {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = syncer::DEFAULT_INTERVAL_SECONDS)]
        interval: u64,
        #[arg(long, default_value_t = syncer::DEFAULT_JITTER_SECONDS)]
        jitter: u64,
        #[arg(long, default_value_t = 4768)]
        port: u16,
    },
    /// Query the Windows scheduled task
    Status,
    /// Remove the Windows scheduled task
    Uninstall,
}

#[derive(Subcommand)]
enum WritebackCommand {
    /// List Markdown changes captured before the next inbound refresh
    Outbox {
        #[arg(long)]
        output: PathBuf,
    },
    /// Discard one captured local edit without touching Youdao
    Discard {
        #[arg(long)]
        output: PathBuf,
        id: i64,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ReadFormat {
    Structured,
    Markdown,
    Html,
    Raw,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let envelope = json!({
                "ok": false,
                "error": {
                    "type": "ynote_local_error",
                    "message": format!("{error:#}"),
                    "hint": error_hint(&error.to_string())
                }
            });
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| error.to_string())
            );
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let Cli {
        data_root,
        account,
        mirror: mirror_path,
        pretty,
        command,
    } = Cli::parse();
    match command {
        Command::Mirror { command } => match command {
            MirrorCommand::Refresh { output, local_only } => {
                let outcome = syncer::refresh_once(data_root, account, &output, local_only)?;
                print_envelope(outcome.summary, pretty)?;
            }
            MirrorCommand::Status { output } => {
                print_envelope(mirror::status(&output)?, pretty)?;
            }
            MirrorCommand::Query { output, sql } => {
                print_envelope(
                    json!({"columns": "positional", "rows": mirror::query(&output, &sql)?}),
                    pretty,
                )?;
            }
        },
        Command::Daemon { command } => match command {
            DaemonCommand::Run {
                output,
                interval,
                jitter,
                bind,
                port,
                local_only,
            } => {
                daemon::run(daemon::RunOptions {
                    data_root,
                    account,
                    output,
                    interval,
                    jitter,
                    bind,
                    port,
                    local_only,
                })
                .await?;
            }
            DaemonCommand::Install {
                output,
                interval,
                jitter,
                port,
            } => {
                print_envelope(
                    json!({
                        "task": daemon::TASK_NAME,
                        "message": daemon::install(&output, interval, jitter, port)?,
                        "starts": "at the next user logon; run `daemon run` to start now"
                    }),
                    pretty,
                )?;
            }
            DaemonCommand::Status => {
                print_envelope(
                    json!({"task": daemon::TASK_NAME, "details": daemon::task_status()?}),
                    pretty,
                )?;
            }
            DaemonCommand::Uninstall => {
                print_envelope(
                    json!({"task": daemon::TASK_NAME, "message": daemon::uninstall()?}),
                    pretty,
                )?;
            }
        },
        Command::Writeback { command } => match command {
            WritebackCommand::Outbox { output } => {
                print_envelope(
                    json!({
                        "applyEnabled": false,
                        "reason": "unofficial cloud writes are disabled until version-precondition and conflict tests pass on a disposable note",
                        "columns": ["id","noteId","baseVersion","operation","status","createdAt","error"],
                        "rows": mirror::list_outbox(&output)?
                    }),
                    pretty,
                )?;
            }
            WritebackCommand::Discard { output, id } => {
                print_envelope(
                    json!({
                        "discarded": mirror::discard_outbox(&output, id)?,
                        "cloudChanged": false
                    }),
                    pretty,
                )?;
            }
        },
        Command::Sync {
            output,
            watch,
            interval,
            jitter,
            local_only,
        } => {
            syncer::validate_interval(interval)?;
            let mut failures = 0u32;
            loop {
                match syncer::refresh_once(data_root.clone(), account.clone(), &output, local_only)
                {
                    Ok(outcome) => {
                        failures = 0;
                        print_envelope(
                            json!({"event": "sync_complete", "summary": outcome.summary}),
                            pretty,
                        )?;
                    }
                    Err(error) if watch => {
                        failures = failures.saturating_add(1);
                        eprintln!("ynote-cli sync failed; keeping last complete mirror: {error:#}");
                    }
                    Err(error) => return Err(error),
                }
                if !watch {
                    break;
                }
                let delay = syncer::next_delay(interval, jitter, failures);
                eprintln!(
                    "ynote-cli sync: next attempt in {} seconds (failures: {})",
                    delay.as_secs(),
                    failures
                );
                tokio::time::sleep(delay).await;
            }
        }
        other => {
            let repo = load_repo(mirror_path.as_deref(), data_root, account)?;
            run_repository_command(repo, other, pretty).await?;
        }
    }
    Ok(())
}

fn load_repo(
    mirror_path: Option<&Path>,
    data_root: Option<PathBuf>,
    account: Option<String>,
) -> Result<Repository> {
    if let Some(path) = mirror_path {
        mirror::load(path)
    } else {
        Repository::discover(data_root, account)
    }
}

async fn run_repository_command(repo: Repository, command: Command, pretty: bool) -> Result<()> {
    match command {
        Command::Doctor => {
            let notes = repo
                .items
                .iter()
                .filter(|x| x.kind == ItemKind::Note && !x.deleted)
                .count();
            let folders = repo
                .items
                .iter()
                .filter(|x| x.kind == ItemKind::Folder && !x.deleted)
                .count();
            let raw_notes = repo
                .items
                .iter()
                .filter(|x| x.kind == ItemKind::Note && repo.raw_available(x))
                .count();
            let available_resources = repo.resources.values().filter(|x| x.available).count();
            let server = SocketAddr::from(([127, 0, 0, 1], 3334));
            let local_server_running =
                TcpStream::connect_timeout(&server, Duration::from_millis(250)).is_ok();
            print_envelope(
                json!({
                    "readOnlyCloudAccess": true,
                    "developerKeyRequired": false,
                    "source": repo.source,
                    "clientExecutableExists": repo.source.client_executable.is_file(),
                    "clientLocalServer": {"address": "127.0.0.1:3334", "reachable": local_server_running},
                    "counts": {
                        "items": repo.items.iter().filter(|x| !x.deleted).count(),
                        "folders": folders,
                        "notes": notes,
                        "rawBodiesAvailable": raw_notes,
                        "resources": repo.resources.len(),
                        "resourcesAvailable": available_resources,
                        "contentIndexRows": repo.content_index.len()
                    },
                    "capabilities": {
                        "cloudPullFromDesktopLogin": true,
                        "sqliteMirror": true,
                        "tree": true,
                        "rawJson": true,
                        "normalizedBlocks": true,
                        "markdown": true,
                        "html": true,
                        "assets": true,
                        "search": true,
                        "folderSync": true,
                        "liveWeb": true,
                        "externalEditOutbox": true,
                        "cloudWriteBack": false
                    }
                }),
                pretty,
            )?;
        }
        Command::Tree { text } => {
            let tree = repo.tree();
            if text {
                let mut output = String::new();
                render_text_tree(&tree, "", &mut output);
                print!("{output}");
            } else {
                print_envelope(tree, pretty)?;
            }
        }
        Command::List { parent } => {
            let parent = parent
                .or_else(|| repo.root_id().map(str::to_string))
                .context("root item not found")?;
            let items = repo
                .items
                .iter()
                .filter(|item| item.parent_id == parent && !item.deleted)
                .cloned()
                .collect::<Vec<_>>();
            print_envelope(items, pretty)?;
        }
        Command::Read { id, output_format } => {
            let note = repo.read_note(&id)?;
            match output_format {
                ReadFormat::Structured => print_envelope(note, pretty)?,
                ReadFormat::Markdown => print!("{}", note.markdown),
                ReadFormat::Html => print!("{}", note.html),
                ReadFormat::Raw => {
                    let raw = note
                        .raw
                        .context("raw body is unavailable in the selected source")?;
                    println!(
                        "{}",
                        if pretty {
                            serde_json::to_string_pretty(&raw)?
                        } else {
                            serde_json::to_string(&raw)?
                        }
                    );
                }
            }
        }
        Command::Search { query, limit } => {
            print_envelope(repo.search(&query, limit.min(500)), pretty)?;
        }
        Command::Resources { note } => {
            let values = if let Some(id) = note {
                let item = repo.item(&id).context("note not found")?;
                item.resources
                    .iter()
                    .filter_map(|reference| repo.resources.get(&reference.resource_id))
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                repo.resources.values().cloned().collect::<Vec<_>>()
            };
            print_envelope(values, pretty)?;
        }
        Command::Export { output } => {
            let manifest = exporter::export(&repo, &output)?;
            print_envelope(json!({"output": output, "manifest": manifest}), pretty)?;
        }
        Command::Serve { bind, port, open } => {
            daemon::validate_loopback(&bind)?;
            if open {
                let url = format!("http://{bind}:{port}");
                let _ = std::process::Command::new("cmd")
                    .args(["/C", "start", "", &url])
                    .spawn();
            }
            web::serve(repo, &bind, port).await?;
        }
        _ => unreachable!("command was handled before repository loading"),
    }
    Ok(())
}

fn print_envelope<T: Serialize>(data: T, pretty: bool) -> Result<()> {
    let envelope = json!({
        "ok": true,
        "data": data,
        "meta": {
            "tool": "ynote-cli",
            "version": env!("CARGO_PKG_VERSION"),
            "cloudAccess": "read_only",
            "writeBack": "outbox_only"
        }
    });
    println!(
        "{}",
        if pretty {
            serde_json::to_string_pretty(&envelope)?
        } else {
            serde_json::to_string(&envelope)?
        }
    );
    Ok(())
}

fn render_text_tree(nodes: &[TreeNode], prefix: &str, output: &mut String) {
    for (index, node) in nodes.iter().enumerate() {
        let last = index + 1 == nodes.len();
        let marker = if last { "└─" } else { "├─" };
        let kind = match node.item.kind {
            ItemKind::Folder => "📁",
            ItemKind::Note => "📄",
            ItemKind::Root => "◉",
        };
        output.push_str(&format!(
            "{prefix}{marker} {kind} {} [{}]{}\n",
            node.item.display_title,
            node.item.id,
            if node.item.kind == ItemKind::Note && !node.raw_available {
                " (正文不可用)"
            } else {
                ""
            }
        ));
        let next = format!("{prefix}{}", if last { "   " } else { "│  " });
        render_text_tree(&node.children, &next, output);
    }
}

fn error_hint(message: &str) -> &'static str {
    if message.contains("No local Youdao Note account") {
        "Start the desktop client and sign in once, or pass --data-root."
    } else if message.contains("not logged in")
        || message.contains("YNOTE_CSTK")
        || message.contains("YNOTE-PC")
    {
        "Sign in to the Windows Youdao Note desktop client, then retry."
    } else if message.contains("too frequent") {
        "Use --interval 300 or higher; the recommended default is 900 seconds."
    } else if message.contains("another ynote-cli") {
        "Wait for the current refresh to finish; overlapping syncs are deliberately blocked."
    } else {
        "Run `ynote-cli doctor --pretty` or `ynote-cli mirror status --output <folder> --pretty`."
    }
}
