# 有道云笔记 CLI / Codex Skill：本地实时同步、Markdown 导出、SQLite 与 OneDrive 备份

`ynote-local` 是一个使用 Rust 编写的 Windows 有道云笔记 CLI、Codex Skill 和本地 Web 控制台。

它复用用户已经登录的有道云笔记 Windows 客户端，不需要开发者 API Key，把笔记实时转换为未加密的 SQLite、Markdown、结构化 JSON、HTML、图片和附件，方便 AI、Codex、Obsidian、VS Code、Typora 及其他编辑器读取。

关键词：有道云笔记导出、有道云笔记同步、Youdao Note CLI、YNote、Codex Skill、AI 知识库、Markdown、SQLite、OneDrive 备份、Rust。

> 本项目不是网易有道官方项目。云端访问保持只读，不提供未经验证的上传、删除或覆盖接口。

## 主要功能

- 无开发者 Key：复用有道云笔记 Windows 客户端当前登录状态。
- 本地实时：监听客户端 SQLite、WAL、正文和资源文件，默认 800 ms 防抖。
- 云端低频兜底：默认 15 分钟间隔、120 秒随机抖动、失败指数退避。
- 完整结构：保留目录、稳定 ID、待办勾选状态、链接、富文本块、图片和附件。
- AI 友好：同时生成 Markdown、结构化 JSON、HTML 和可查询 SQLite。
- OneDrive 备份：默认创建并使用 `~/OneDrive/notes/YoudaoNote`。
- 高性能：Rust 单进程、增量刷新、版本缓存、原子写入和 SQLite 事务。
- Web 控制台：查看 CPU、内存、数据来源、同步链路、参数、历史、完整 CLI 账本和安全边界。
- 安全外部编辑：编辑 Markdown 后，在下次入站刷新前保存到 outbox；不会自动写回有道云。

## 系统要求

- Windows x64。
- 已安装并登录有道云笔记 Windows 客户端。
- Codex Skill 安装场景需要 Codex。
- 默认 Web 端口 `4768` 未被占用。

运行时不需要 Node.js、Python、Visual Studio 或 Rust。发布包包含 Windows x64 可执行文件及所需 Rust 运行库。

## 安装

### 方法一：从 GitHub 安装 Codex Skill

在 Codex 中运行：

```text
$skill-installer install https://github.com/tingaidehua/ynote-local/tree/main/skills/ynote-local
```

安装后可直接向 Codex 提问：

```text
我的有道笔记有哪些目录？
待办笔记里还有哪些没完成？
搜索所有提到 Rust 的笔记并总结。
先同步最新云端笔记，再回答我的问题。
```

### 方法二：Windows Release ZIP

从 GitHub Releases 下载 `ynote-local-v0.4.1-windows.zip`，解压后运行：

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
powershell -ExecutionPolicy Bypass -File .\setup-ynote.ps1 -Cloud -Start -InstallStartup
```

`setup-ynote.ps1` 会：

1. 自动发现有道云笔记客户端与当前登录账号；
2. 使用 OneDrive 环境路径；若目录尚不存在，则创建 `%USERPROFILE%\OneDrive`；
3. 创建 `%USERPROFILE%\OneDrive\notes\YoudaoNote`；
4. 建立第一份未加密镜像；
5. 可选启动守护进程并配置当前用户登录自启动。

可以用 `-MirrorDirectory` 显式指定其他镜像位置。

## 默认数据目录

```text
%USERPROFILE%\OneDrive\notes\YoudaoNote
├─ <有道文件夹>\<笔记>.md
├─ <有道文件夹>\<笔记>.ynote.json
├─ .ynote-manifest.json
└─ _ynote
   ├─ ynote-mirror.sqlite
   ├─ runtime-config.json
   ├─ raw
   ├─ resources
   └─ cloud
      ├─ raw
      └─ resources
```

所有这些文件均不使用应用层加密：

- `ynote-mirror.sqlite`：标准 SQLite 3 数据库；
- `.md`：UTF-8 Markdown；
- `.ynote.json`：元数据、规范化块、Markdown、HTML 和原始 JSON；
- `raw`：原始正文；
- `resources`：图片和附件；
- `.ynote-manifest.json`：路径、父子关系、版本和 SHA-256。

## 使用 CLI

以下示例假设已经进入安装后的 Skill 目录：

```powershell
$cli = ".\scripts\ynote-cli-0.4.1.exe"
$mirror = "$env:USERPROFILE\OneDrive\notes\YoudaoNote"
$db = "$mirror\_ynote\ynote-mirror.sqlite"

& $cli doctor --pretty
& $cli mirror refresh --output $mirror --pretty
& $cli mirror status --output $mirror --pretty
& $cli --mirror $db tree --text
& $cli --mirror $db search "关键词" --limit 20 --pretty
& $cli --mirror $db read "<NOTE_ID>" --output-format structured --pretty
& $cli mirror query --output $mirror "SELECT id,title,version FROM items WHERE kind='note'" --pretty
```

启动本地同步和 Web 控制台：

```powershell
& $cli daemon run --output $mirror --interval 900 --jitter 120 --port 4768
```

打开 <http://127.0.0.1:4768/>。

配置当前用户登录自启动，无需管理员权限：

```powershell
& $cli daemon install --output $mirror --interval 900 --jitter 120 --port 4768
& $cli daemon status --pretty
```

## 同步架构

```text
有道云笔记 Windows 客户端
  ├─ 本地 SQLite / WAL / 正文 / 图片 / 附件
  │    └─ Windows 原生文件事件 → 800 ms 防抖 → 本地增量刷新
  └─ 当前登录会话
       └─ 只读云端拉取 → 15 分钟 + 抖动 + 退避 + 版本缓存

两条链路
  → 解析原始 JSON
  → 生成结构化块、Markdown、HTML
  → 原子文件写入 + SQLite 事务
  → CLI / Web / Codex / AI / 编辑器
```

本地事件不会增加云端请求次数。云端最小间隔为 300 秒，手动和计划任务共享限流保护。

## 外部编辑与写回边界

可以使用 Obsidian、VS Code、Typora 等编辑器打开导出的 `.md` 文件。

下一次同步前，CLI 会检测 Markdown SHA-256 的变化，并把完整编辑内容、基础版本和基础哈希保存到 SQLite `outbox`：

```powershell
& $cli writeback outbox --output $mirror --pretty
```

当前没有 `writeback apply`。有道非公开写接口涉及版本前置条件、资源上传和并发冲突；完成可证明的无损测试前，不会把外部编辑自动覆盖到有道云。

不要直接修改：

- `_ynote\ynote-mirror.sqlite`
- `*.ynote.json`
- `_ynote\raw`
- `_ynote\resources`

不要让多台电脑上的守护进程同时写同一个 OneDrive 镜像。

## 隐私与安全

- 不提交、不打包用户笔记、账号 ID、Cookie、缓存数据库或运行日志。
- 打包与 GitHub Actions 会运行 `scripts/check-public-tree.ps1`；发现镜像文件、真实用户路径、账号 ID、笔记 ID、私钥或非白名单 Markdown 时直接失败。
- Cookie 只从本机 `%APPDATA%\ynote-desktop\setting.json` 读取，并仅保存在进程内存。
- 认证请求只允许发送到精确 HTTPS 主机 `note.youdao.com`。
- 不修改有道客户端数据库和缓存。
- 不调用云端 push、upload、update 或 delete。
- Web 只绑定回环地址，拒绝外网暴露。
- Web API 和日志不返回 Cookie。
- SQL 接口只允许单条 `SELECT`、`WITH` 和受限 `PRAGMA`。

## Web 控制台

控制台与 CLI 使用同一套 Rust 能力层，提供：

- 版本、PID、运行时间、CPU、工作集、私有内存和句柄数；
- 笔记、资源、存储、完整性、修订号和同步状态；
- 云端开关、间隔、抖动、本地防抖和页面轮询参数；
- 本地即时刷新、受限云端刷新和只读 SQL；
- 数据源路径、访问方式、完整处理链路和同步历史；
- 全部 CLI 命令与参数映射；
- 回环绑定、Cookie、云端写回和 SQL 安全边界。

## 构建

项目默认配置 `rsproxy.cn` 国内 Rust 镜像，使用 `stable-x86_64-pc-windows-gnullvm`：

```powershell
$env:RUSTUP_DIST_SERVER = "https://rsproxy.cn"
$env:RUSTUP_UPDATE_ROOT = "https://rsproxy.cn/rustup"
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

生成发布包：

```powershell
.\scripts\package-release.ps1 -Version 0.4.1
```

## 仓库结构

```text
ynote-local
├─ src                         Rust CLI、同步器、镜像与 Web API
├─ web                         Web 控制台
├─ skills\ynote-local          可直接安装的 Codex Skill
├─ scripts                     安装、初始化和打包脚本
├─ .github\workflows\release.yml
├─ Cargo.toml
└─ README.md
```

## 许可证

[MIT License](LICENSE)
