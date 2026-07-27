---
name: ynote-local
description: Read, search, export, synchronize, query, and locally serve Youdao Note (有道云笔记) through a durable unencrypted SQLite/Markdown mirror, using the logged-in Windows desktop session without a developer API key. Use for current cloud notes, real-time desktop-save capture, local hierarchy, rich block JSON, checkboxes, links, images, attachments, mirror health, full-text/SQL queries, safe low-frequency cloud sync, the localhost CLI control console with runtime metrics and data lineage, or captured external edits.
---

# Ynote Local

Use the bundled Rust CLI as the single AI-facing read layer for the current Windows user's Youdao Note data. The default flow pulls current cloud metadata, changed note bodies, and changed resources with the desktop client's existing login session, then transactionally updates an unencrypted SQLite mirror and an atomic Markdown/JSON/resource projection.

The cloud side is read-only. Never modify `%APPDATA%\ynote-desktop`, never print or persist login cookies, and never invent a cloud write command.

## Locate the CLI

The executable is `scripts/ynote-cli-0.4.1.exe` relative to this `SKILL.md`. Keep `std-*.dll` and `libunwind.dll` beside it. Resolve the installed skill directory and invoke the executable with PowerShell's call operator; do not assume it is on `PATH`.

Discover the active mirror before querying it. If the daemon is running, read `GET http://127.0.0.1:4768/api/console` and use `data.config.output`; do not assume an old task-specific path. The Windows setup script uses the configured OneDrive root or creates `%USERPROFILE%\OneDrive`, then stores the mirror under `notes\YoudaoNote`.

## Prefer the durable mirror

When a mirror already exists, use its `_ynote\ynote-mirror.sqlite` for `tree`, `read`, `search`, and `resources`:

```powershell
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' --mirror '<mirror>\_ynote\ynote-mirror.sqlite' doctor --pretty
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' --mirror '<mirror>\_ynote\ynote-mirror.sqlite' tree --pretty
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' --mirror '<mirror>\_ynote\ynote-mirror.sqlite' search '<query>' --limit 50 --pretty
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' --mirror '<mirror>\_ynote\ynote-mirror.sqlite' read '<note-id>' --output-format structured --pretty
```

Use stable IDs, not titles, for follow-up reads. Use Markdown for prose-oriented downstream work, structured JSON for rich blocks and fidelity, and raw JSON only when exact source structures matter.

## Refresh current cloud data

Run a one-shot refresh when freshness matters:

```powershell
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' mirror refresh --output '<mirror>' --pretty
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' mirror status --output '<mirror>' --pretty
```

The refresh reads `%APPDATA%\ynote-desktop\setting.json`, sends the needed cookies only to exact host `https://note.youdao.com`, and never includes cookie values in output or errors. It downloads only missing body/resource versions.

For continuous sync plus live Web:

```powershell
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' daemon run --output '<mirror>' --interval 900 --jitter 120 --port 4768
```

The daemon uses two independent channels. Native Windows filesystem events watch the desktop SQLite/WAL, note bodies, and resources; after an 800 ms debounce they run a local-only refresh and do not contact Youdao. A separate cloud pull keeps the 900-second base interval plus jitter and backoff. Never shorten cloud polling merely to make desktop edits appear faster. Failures automatically back off up to two hours. The Web bind must remain loopback.

The Web UI reloads the tree and current note only when `sync.revision` changes. It also provides a complete CLI control console at `http://127.0.0.1:4768/`: live CPU/memory/uptime, hot parameters, local/cloud refresh actions, storage and integrity, source paths, the full processing pipeline, recent sync history, read-only SQL, security boundaries, and a ledger covering every CLI command and parameter.

Hot parameters use the same Rust validation constants as the CLI and are atomically persisted to `<mirror>\_ynote\runtime-config.json`. They survive restart. An explicit `--local-only` remains authoritative and forces cloud scheduling off even if persisted settings request it. Use `/api/console/metrics` for lightweight polling and `/api/console` for the complete inventory. Mutating API calls require JSON and `X-Ynote-Console: 1`.

To persist through the current user's `HKCU\...\Run` entry without administrator rights:

```powershell
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' daemon install --output '<mirror>'
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' daemon status --pretty
```

## Query SQLite safely

AI can read the SQLite database directly, or use the guarded CLI:

```powershell
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' mirror query --output '<mirror>' "SELECT id,title,version FROM items WHERE kind='note'" --pretty
```

The command accepts one `SELECT`, `WITH`, `integrity_check`, `quick_check`, or `table_info` statement and rejects mutations/stacked SQL.

Tables:

- `items`: stable hierarchy, versions, deletion state, full item JSON;
- `notes`: fidelity, raw JSON, normalized blocks, Markdown, HTML, text and SHA-256;
- `resources`: local paths, versions, SHA-256 and full resource JSON;
- `sync_state` and `sync_runs`: last successful snapshot and history;
- `outbox`: external Markdown edits captured before inbound overwrite.

## Treat external edits as drafts

On refresh, changed exported Markdown is saved into `outbox` with base version/hash before the cloud snapshot replaces the file:

```powershell
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' writeback outbox --output '<mirror>' --pretty
& '<skill-dir>\scripts\ynote-cli-0.4.1.exe' writeback discard --output '<mirror>' '<outbox-id>' --pretty
```

There is intentionally no `writeback apply`. Explain that unofficial Youdao writes remain disabled until resource upload, version preconditions, and concurrent conflict behavior are proven lossless on disposable data.

## Fidelity and integrity

Interpret fidelity literally:

- `cloud_raw_plus_normalized`: authenticated cloud raw JSON plus normalized content;
- `lossless_raw_plus_normalized`: desktop-local raw JSON plus normalized content;
- `desktop_unsynced_raw_plus_normalized`: desktop-local raw JSON is newer than the cloud snapshot and must be preserved;
- `public_share_raw_plus_normalized`: raw JSON/resources recovered from a public-share key;
- `confirmed_empty_body`: metadata explicitly reports a zero-byte body;
- `search_index_fallback`: only indexed plain text exists;
- `metadata_only_content_not_local`: only metadata is available.

Do not claim completeness when `.ynote-manifest.json` has warnings, `mirror status` integrity is not `ok`, Markdown contains `missing-resource:`, or an available resource hash does not match its manifest SHA-256.

Keep stdout as data and stderr as operations/errors. Parse `{ok,data,meta}` or `{ok:false,error}` before acting.

Read [references/cli-reference.md](references/cli-reference.md) for exact commands, output contracts, tables, polling rules, and mirror layout.
