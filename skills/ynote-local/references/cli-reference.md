# ynote-cli reference

## Global options

```text
--data-root <path>   ynote-desktop app-data root or concrete ynote-data directory
--account <id>       local account directory/database basename
--mirror <path>      read repository commands from ynote-mirror.sqlite
--pretty             pretty-print JSON envelopes
```

## Commands

```text
doctor
tree [--text]
list [--parent <folder-id>]
read <note-id> [--output-format structured|markdown|html|raw]
search <query> [--limit <1..500>]
resources [--note <note-id>]
export --output <directory>
sync --output <directory> [--watch] [--interval 900] [--jitter 120] [--local-only]

mirror refresh --output <directory> [--local-only]
mirror status --output <directory>
mirror query --output <directory> <read-only-sql>

daemon run --output <directory> [--interval 900] [--jitter 120] [--bind 127.0.0.1] [--port 4768] [--local-only]
daemon install --output <directory> [--interval 900] [--jitter 120] [--port 4768]  # current-user HKCU Run startup; no admin
daemon status
daemon uninstall

writeback outbox --output <directory>
writeback discard --output <directory> <id>

serve [--bind 127.0.0.1] [--port 4768] [--open]
```

`sync` and `daemon` reject cloud intervals below 300 seconds. Delay is interval plus jitter; consecutive failures use exponential backoff capped at two hours.

`daemon` also installs a native Windows filesystem watcher on the selected account's SQLite/WAL files, `file`, and `resource` trees. Relevant events are coalesced with 800 ms quiet debounce and a 5-second maximum batch. This path always calls `refresh_once(..., local_only=true)` and therefore never increases cloud traffic. Cloud and local refreshes share a single in-process gate plus the cross-process mirror lock.

`/api/health` exposes `sync.revision`, `sync.localWatch`, `sync.localRefreshCount`, `sync.lastLocalSuccess`, and `sync.lastCloudSuccess`. The Web UI checks this small endpoint and reloads note data only when the revision changes.

## Web control console

The daemon serves the note browser and complete control plane from `http://127.0.0.1:4768/`.

```text
GET  /api/console          full configuration, command ledger, sources, pipeline, metrics, history
GET  /api/console/metrics  lightweight process and revision metrics
POST /api/console/config   persist and broadcast validated hot configuration
POST /api/console/sync     queue one local or cloud refresh
POST /api/console/sql      run one guarded read-only SQL statement
```

POST requests require `Content-Type: application/json` and `X-Ynote-Console: 1`. The Web page and CLI handlers call the same Rust capability layer.

Hot settings and ranges:

```text
cloudEnabled                    true|false
cloudIntervalSeconds            >=300
cloudJitterSeconds              0..3600
localDebounceMilliseconds       100..5000
webStatusPollMilliseconds       500..60000
```

Accepted values are atomically stored in `<mirror>\_ynote\runtime-config.json` and survive restart. Secrets are never persisted there. An explicit `--local-only` overrides a stored `cloudEnabled: true`.

Bind, port, output directory, executable, startup installation, and the 5-second maximum local-event batch are displayed as fixed/restart settings. Lifecycle and destructive commands stay terminal-only and remain visible in the command ledger with their reason.

## Output contract

Structured success:

```json
{
  "ok": true,
  "data": {},
  "meta": {
    "tool": "ynote-cli",
    "version": "0.4.1",
    "cloudAccess": "read_only",
    "writeBack": "outbox_only"
  }
}
```

Errors go to stderr:

```json
{
  "ok": false,
  "error": {
    "type": "ynote_local_error",
    "message": "actionable context chain",
    "hint": "suggested recovery"
  }
}
```

`read --output-format markdown|html|raw` writes the body directly.

## Mirror layout

```text
<mirror>\
  .ynote-manifest.json
  <folder>\<note>.md
  <folder>\<note>.ynote.json
  _unfiled\...
  _ynote\
    ynote-mirror.sqlite
    runtime-config.json
    raw\...
    resources\...
    cloud\
      raw\<note-id>--v<version>.json
      resources\<resource-id>--v<version>.<ext>
```

Windows-invalid characters use full-width replacements. Sibling collisions receive stable ID suffixes. Detached/shared items are kept under `_unfiled`.

The manifest is written last and records text/resource SHA-256. Unchanged exported bytes are not rewritten; changed files use same-volume atomic replacement. SQLite updates use a single `BEGIN IMMEDIATE` transaction, WAL, `synchronous=FULL`, and a post-write integrity check.

## SQLite schema

```text
items(id,parent_id,kind,title,version,modified_at,deleted,item_json)
notes(item_id,fidelity,raw_format,raw_json,blocks_json,markdown,markdown_sha256,html,content_text)
resources(id,version,relative_path,sha256,resource_json)
sync_state(key,value)
sync_runs(id,started_at,finished_at,backend,success,message,stats_json)
outbox(id,note_id,base_version,base_checksum,operation,payload,content_sha256,status,created_at,error)
```

`mirror query` is read-only by construction. Direct readers should open SQLite in read-only mode.

## Cloud request boundary

Authenticated requests are limited to exact HTTPS host `note.youdao.com`:

- metadata: `method=pull`;
- note body: `method=download`;
- resource bytes: `method=getResource`.

Cookies come from the logged-in desktop client's `setting.json`, remain in memory, and must never be logged or copied into the mirror. The CLI does not call push/upload/update/delete methods.

## Normalized blocks and fidelity

Structured output preserves ordered blocks, todo checked state, headings, lists, links, inline styles, image resource IDs and source URLs. The raw JSON remains authoritative for unsupported structures.

```text
cloud_raw_plus_normalized            authenticated cloud raw plus normalized output
lossless_raw_plus_normalized         desktop-local raw plus normalized output
desktop_unsynced_raw_plus_normalized desktop-local raw newer than cloud; preserve it
public_share_raw_plus_normalized     public-share raw plus normalized output
confirmed_empty_body                 metadata explicitly records zero bytes
search_index_fallback                local indexed text only
metadata_only_content_not_local      metadata only
```
