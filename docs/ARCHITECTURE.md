# 架构

sessync 怎么搭起来的。阅读顺序：自顶向下 —— 用户视角 → 适配器 → 加密 → 缓存 → 发布流水线。

## 总览

```
┌──────────────────────────────────────────────────────────────────────────┐
│  用户 CLI                                                                 │
│   sessync init / push / resume / status / hook / install / uninstall      │
└────────────────────────┬─────────────────────────────────────────────────┘
                         │
            ┌────────────┴────────────┐
            ▼                         ▼
┌───────────────────┐       ┌──────────────────────┐
│ ToolAdapter       │       │ StorageAdapter       │
│ (本地 session     │       │ (远程加密 blob 存哪) │
│  在哪)            │       │                      │
├───────────────────┤       ├──────────────────────┤
│ ClaudeCodeAdapter │       │ OssStorage           │
│ (~/.claude/...)   │       │ LocalFsStorage       │
│                   │       │ InMemoryStorage      │
│ [V2: Codex,       │       │ [V2: R2, MinIO, S3]  │
│      Cursor]      │       └──────────────────────┘
└───────────────────┘
            │                         │
            └────────────┬────────────┘
                         ▼
              ┌──────────────────────┐
              │ Crypto (age + KDF)    │
              ├──────────────────────┤
              │ argon2id(passphrase, │
              │   shared salt) →     │
              │   32-byte key →      │
              │   age encrypt        │
              └──────────────────────┘
                         │
                         ▼
              ┌──────────────────────┐
              │ 本地 meta 缓存        │
              │ (只读优化层)          │
              └──────────────────────┘
```

两个可插拔边界（Tool + Storage），一层加密，一层可选缓存。其他都是粘合代码。

---

## 模块布局

```
src/
├── main.rs              CLI 入口、clap derive、分发
├── lib.rs               给集成测试用的 re-exports
├── error.rs             SessyncError + Result<T>
├── types.rs             SessionId、ProjectKey、SessionMeta（wire format）
│
├── crypto.rs            argon2id KDF + age 加密/解密
├── config.rs            Config（TOML）、OssConfig、LocalFsConfig
├── passphrase_store.rs  machine-bound passphrase 文件（取代 Keychain）
├── keychain.rs          已废弃 —— 保留供 v0.1.x 迁移辅助
├── cache.rs             加密本地 meta 缓存
│
├── adapter/
│   ├── tool.rs          ToolAdapter trait + LocalSession
│   ├── claude_code.rs   ClaudeCodeAdapter（~/.claude/projects 布局）
│   ├── path_codec.rs    cwd ↔ 编码目录名 + 项目 key 哈希
│   ├── storage.rs       StorageAdapter trait + StorageObject
│   ├── oss.rs           阿里云 OSS 实现
│   ├── local_fs.rs      文件系统当 bucket（烟测用）
│   └── memory.rs        InMemoryStorage（单元/集成测试）
│
├── ui/
│   ├── mod.rs
│   └── style.rs         owo-colors 样式表（识别 TTY + NO_COLOR）
│
└── commands/
    ├── init.rs          交互式首次设置
    ├── install.rs       cp self + codesign + PATH 检查
    ├── uninstall.rs     install 的反向（含可选 --purge-remote）
    ├── hook.rs          Stop hook install/uninstall/status
    ├── push.rs          读本地 session → 加密 → 上传
    ├── resume.rs        list 远程 → 缓存感知 fetch → 解密 → 写本地
    └── status.rs        只读摘要
```

---

## ToolAdapter trait —— "本地 session 在哪"

```rust
trait ToolAdapter {
    fn name(&self) -> &'static str;
    async fn list_local_sessions(&self) -> Result<Vec<LocalSession>>;
    async fn read_session(&self, session_id: &SessionId) -> Result<Vec<u8>>;
    async fn write_session(&self, session_id: &SessionId, target_cwd: &str, raw: &[u8]) -> Result<PathBuf>;
    fn project_key_for(&self, cwd: &str) -> ProjectKey;
}
```

v1 **只实现 `ClaudeCodeAdapter`**。它的职责：

- **发现 session**：遍历 `~/.claude/projects/<encoded-cwd>/<sid>.jsonl`，构造 `SessionMeta`（id、project_key、source_cwd、hostname、mtime、byte_size、preview）。
- **cwd ↔ 目录名编解码**：Claude Code 的约定是 `/` → `-`。如果路径分量本身含 `-` 是 lossy 的。逻辑放在 `path_codec.rs`，方便其他 tool adapter 共用（或替换）。
- **项目 key**：cwd 的 SHA-256 取前 8 字节 hex（16 字符）。同一路径在不同设备上稳定相同。不同路径产生不同 key —— 用户在 resume 时自己挑要拉哪个。
- **原子写**：`tmp + rename`，崩溃中途不会留下半截 jsonl 让 Claude resume 失败。

**新增一个 tool adapter**（比如 V2-1 的 Codex）只要：实现 trait、放进 `adapter/`、加 CLI flag 或自动检测。crypto / storage / cache / commands 全都不用动。

---

## StorageAdapter trait —— "远程加密 blob 存哪"

```rust
trait StorageAdapter {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Vec<u8>>;
    async fn list(&self, prefix: &str) -> Result<Vec<StorageObject>>;
    async fn delete(&self, key: &str) -> Result<()>;
}
```

三个实现：

- **`OssStorage`** —— 生产环境用，阿里云 OSS via `aliyun-oss-client = 0.13`。每个方法转调 SDK；SDK 每次调用都新建一个 `reqwest::Client`（每次都要 TLS handshake —— 已知性能瓶颈，见 v2-backlog 的 P3）。
- **`LocalFsStorage`** —— 目录当 bucket，被 `--mock` 用来在没 OSS 账号情况下做烟测。
- **`InMemoryStorage`** —— `Mutex<HashMap<String, Vec<u8>>>`，集成测试用。

**对象 key 布局**：

```
<bucket>/<configured prefix>/
├── .sessync-salt                         (16 字节明文，跨设备共享)
└── claude-code/                          (tool 名)
    └── <project_key>/                    (sha256(cwd) 前 8 字节 hex)
        ├── <session_id>.age              (加密的 session jsonl)
        └── <session_id>.age.meta.json    (加密的 SessionMeta sidecar)
```

**meta sidecar**（小，加密后 100-500 字节）携带 resume 选单需要展示的信息（项目路径、preview），让我们不用下载整个 session 内容就能渲染选单。

---

## 加密链路

```
用户 passphrase（字符串）
        +
.sessync-salt（OSS 上的 16 字节随机字节，第一次 init 时生成，跨设备共享）
        ↓
argon2id (m=64 MiB, t=3, p=4)
        ↓
32 字节对称 key（只在内存里，永不持久化）
        ↓
age::Encryptor::with_user_passphrase(hex(key))
        ↓
密文（上传到 OSS）
```

**为什么 salt 要共享**：M1 烟测期间发现的 B1 设计 bug —— 之前每台设备各自生成 salt，所以两台 passphrase 一样的设备派生出的 key 不同，Mac B 解不开 Mac A 推上去的内容。修复：第一次 `sessync init` 时 GET `<prefix>.sessync-salt`，没有就生成 + PUT，有就直接用。

**为什么 argon2id + age 串了两次 KDF**：argon2id 是慢哈希，专门用于 password 强化。age 内部又对 hex 编码的 32 字节 key 跑了一遍 scrypt（当成 passphrase）—— 这是第二次 KDF。浪费（每次加密多 ~200ms）但无害。在 backlog 里跟踪为 Q1：用 chacha20-poly1305 直接替换 `age::Encryptor::with_user_passphrase`。

**这把 key 保护什么**：
- session jsonl 内容（完整对话）—— 上传前加密。
- `SessionMeta` sidecar（cwd、hostname、preview）—— 上传前加密。
- 本地 meta 缓存（整个文件）—— 静态加密。

**不被这把 key 保护**的：
- 共享 salt 本身（公开值；安全性来自 passphrase 熵）。
- 对象 key（如 `claude-code/<project_key>/<session_id>.age`）—— OSS 访问日志和有 bucket 读权限的人能看到。session_id 是 Claude 生成的 UUID，project_key 是 `sha256(cwd)[..8]`，都不泄露可读信息但确实没加密。

---

## passphrase 存储（K-new）

passphrase 需要在多次命令之间持久化，否则用户每次跑命令都得重输一次。v0.1.x 用 macOS Keychain 存，每次 read 都弹用户确认窗（每次 `brew upgrade` 后也都要重新弹一次，因为 Keychain 信任是 binary hash 级的）。

v0.2.0 用 **machine-bound 加密文件**取代 Keychain：

```
~/.config/sessync/passphrase.enc   (Unix 上 chmod 0600)

内容 = age::encrypt(passphrase_bytes, file_key)

file_key = HMAC-SHA256(machine_id, b"sessync-passphrase-v1")

machine_id（按 OS 取）：
  macOS:    ioreg → IOPlatformUUID
  Linux:    /etc/machine-id
  Windows:  HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid
```

**特性**：
- ✅ 安装时不弹（文件 IO 不触发任何系统弹窗）。
- ✅ 升级时不弹（加密 key 跟 binary 无关）。
- ✅ 跨平台（machine_id 各 OS 来源由 `machine-uid` crate 抽象）。
- ✅ 文件被偷复制到别的机器没用 —— 装到别的 box 直接解密失败。
- ⚠️ 比 Keychain 在某些场景安全性略低 —— 比如同机器 root 用户提取文件（root 既能读 0600 又能绕开 Keychain，等于洗漱）。

对个人单用户的 macOS / Linux / Windows 使用场景，这个 trade-off 可接受。多用户共享的 Mac 名义上 Keychain 略好，但 sessync 本来也不是为这种场景设计的。

---

## 本地 meta 缓存（P1）

没缓存的话，每次 `sessync resume`：
- 1 次 `storage.list(prefix)`（在国内 + 代理走阿里云 OSS 大约 700ms）
- N 次 `storage.get(meta)` 拉项目列表（每个项目一次，并发 8）
- M 次 `storage.get(meta)` 拉选定项目下的 session 列表（并发）
- 1 次 `storage.get(content)` 拉选中的 session 内容

每次 OSS 调用都付完整 TLS handshake 钱（SDK 不复用连接）。每次 ~250ms，27 个 session 的 bucket 即使 8 路并发也要 5-15s 总耗时。

**缓存**夹在双循环和 storage 之间：

```
~/.cache/sessync/meta-cache.age   (chmod 0600，age 加密)

内容 = MetaCache {
    schema_version: u32,
    tool: String,
    entries: HashMap<String, CachedEntry>  // key → (解密后的 SessionMeta、远程 mtime、远程 size)
}
```

**带缓存的 resume 流程**：

```
1. storage.list(prefix)                     ← 始终运行（truth source）
2. 从磁盘加载缓存（best-effort）
3. 对 list 返回的每个对象：
     - 缓存里有 + remote_mtime == cached_mtime + remote_size == cached_size
       → 用缓存里的 SessionMeta（跳过 GET）
     - 否则 → 加进 "待 fetch 列表"
4. buffered(8) GET + 解密待 fetch 的，写进缓存
5. 从缓存构造 dialoguer 选单（现在缓存里有所有需要的东西）
6. resume 完成后把缓存写回磁盘
```

**稳态**（远程没变化）：list 700ms + 0 次 fetch + ~50ms 从缓存解析 = **~1s**。
**增量**（其他设备新增了 K 个 session）：list 700ms + K × 200ms fetch = **典型 K 下 1-3s**。
**冷/失效**（passphrase 变了 / schema bump 等）：完整 re-fetch = **5-15s**。

**失效**自动且永远不会服务陈旧数据：
- list 是 truth source —— 远程对象没了，缓存条目就被删（`retain_only`）。
- mtime + size 不一致 → re-fetch。
- passphrase 变了 → 缓存解密成乱码 → load 返回 empty → 冷 rebuild。
- schema_version 不匹配 → load 返回 empty。

---

## 自动 push（Stop hook 集成，A1）

Claude Code 从 `~/.claude/settings.json` 读 hook。形状：

```json
{
  "hooks": {
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          {"type": "command", "command": "sessync push --quiet # sessync-auto-push"}
        ]
      }
    ]
  }
}
```

**`sessync hook install`** 改这个文件：
- 原子写（tmp + rename）。
- 幂等：同时匹配尾部 tag（`sessync-auto-push`）**和** `sessync push` 命令前缀，所以重复 install 不会重复，uninstall 也总能找到我们的条目。
- 保留用户其他 hook（其他 Stop 条目、其他事件类型如 PreToolUse）。

**`sessync hook status`** 报告是否已装，**且**警告 `sessync` 是否在 Claude Code 继承的 PATH 里（一个常见的静默失败模式）。

**`sessync push --quiet`** 抑制正常的 "pushed N sessions" 输出，但错误和 tracing 日志仍然写。

---

## 发布流水线

```
你: git tag v0.X.Y && git push --tags
        ↓
GitHub Actions release.yml:
   ├─ macOS-latest runner
   ├─ build x86_64 + aarch64
   ├─ lipo 合并 → universal binary
   ├─ codesign --force --sign - --identifier com.echobyte26.sessync
   ├─ tar czf + sha256
   ├─ 创建 GitHub Release + 上传 tar.gz + .sha256
   └─ Tap auto-bump:
       ├─ git clone echobyte26/homebrew-sessync (用 TAP_REPO_TOKEN PAT)
       ├─ sed Formula/sessync.rb (version + sha256)
       └─ git push (commit author "sessync-release-bot")
        ↓
任何人跑 `brew upgrade sessync` 立即拿到 v0.X.Y。
```

**涉及的两个 repo**：
- `echobyte26/sessync` —— 源码、release artifact、Actions workflow。
- `echobyte26/homebrew-sessync` —— 单 formula 的 tap 仓库。`brew tap echobyte26/sessync` 克隆的就是它。

**`echobyte26/sessync` repo 的 secrets**：
- `TAP_REPO_TOKEN` —— fine-grained PAT，权限范围限定 `echobyte26/homebrew-sessync` 的 Contents:Read+Write。给 auto-bump 步骤用。

日常发版操作详见 [`docs/RELEASING.md`](RELEASING.md)。

---

## Wire format 兼容性

`SessionMeta` 带 `schema_version: u32` 字段（`#[serde(default = "1")]`）—— 设计上允许结构体前向兼容地演进而不影响现有 OSS 对象。新加字段用 `#[serde(default)]`；不向后兼容的结构变化要 bump 版本号。

本地缓存自带它**自己的** `schema_version`（跟 `SessionMeta` 的版本号独立），所以缓存结构变化能自动失效而不动远程对象。

---

## 已知架构债（在 v2-backlog 里追踪）

- **OSS SDK 不复用连接** —— 每次 API 调用都付完整 TLS handshake。真正修复只能换/fork SDK（P3）；P1 缓存是绕道。
- **加密路径双 KDF** —— argon2id 然后 age 内部 scrypt 又来一遍。Q1 会切到 chacha20-poly1305 直接。
- **没有 CRDT 风格 merge** —— 当两台设备非串行地推同一个 session，last-writer-wins。v0.3.0 加检测（C1）和 fork-on-conflict UI（C2），但真正的 merge 故意 out of scope（Claude 的 parentUuid jsonl 模型本来就不是行级可 merge 的）。
- **OSS list 是单页**（最多 1000 个对象）。每个活跃项目 ~25 个 session 的话，要 >40 个活跃项目才会撞到。已 track 但未排期。
- **Stop hook 依赖 PATH** —— hook 命令 `sessync push --quiet` 需要 `sessync` 在 Claude Code 继承的 PATH 里。`sessync hook status` 不在时会警告，但没自动修。
