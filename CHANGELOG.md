# 更新日志

记录 sessync 的所有重要变更。格式参考 [Keep a Changelog](https://keepachangelog.com/)。

## [0.2.3] — 2026-05-05

主题：撤回 v0.2.1 引入的 keychain probe，恢复 K-new 的"安装就不弹"承诺。

### 修复

- **`passphrase_is_set()` 不再 probe macOS Keychain**。v0.2.1 加的 keychain check（本意：让 status 在迁移前显示"set"）触发了 `keyring::get_password` 的 ACL 弹窗，让 init 和 uninstall 的 detect 阶段都跟着弹。回退掉这个 probe，改为只看文件存在性。
- 后果：从 v0.1.x 升级且**还没跑过 push/resume** 的用户，status 会短暂显示 "passphrase: missing"。第一次 push/resume 触发 `load_passphrase` 的迁移逻辑后恢复正常。这是为"安装就不弹"做的合理 trade-off。
- v0.2.1 的 keychain → 文件迁移逻辑（在 `load_passphrase` 里）保留不变。那个迁移确实必弹一次（要从 keychain 读 passphrase 必经 ACL）—— 但只在 push/resume 上发生一次。

## [0.2.2] — 2026-05-05

主题：修复 v0.2.x init 在 fresh OSS bucket 上的失败。

### 修复

- **`OssStorage::get` 把 OSS 的 `NoSuchKey` 错误归一化为 `"not found: <key>"`** —— 跟 `LocalFsStorage` / `InMemoryStorage` 错误格式一致。**之前**：在没有 `.sessync-salt` 对象的 bucket 上跑 `sessync init` 会爆 `Service(ServiceXML { code: "NoSuchKey", ... })`，因为 B1 共享 salt 协议的 string-match `msg.contains("not found")` 没匹配上 SDK 原生错误。**现在**：first-init 看到 NoSuchKey → 视为"远程没 salt" → 自动生成 + PUT，正常完成 init。
- M1 的 B1 测试用 InMemoryStorage 没暴露这个 backend 之间错误格式不一致的问题。后续应该把 NotFound 错误升级为 typed variant（v0.3.0+ 候选）。

## [0.2.1] — 2026-05-05

主题：修复 v0.2.0 升级摩擦 —— **从 v0.1.x 升级不再需要重跑 `sessync init`**。

### 修复

- **macOS Keychain → 加密文件 自动迁移** —— `passphrase_store::load_passphrase()` 现在会在 `~/.config/sessync/passphrase.enc` 不存在时尝试读 v0.1.x 留在 macOS Keychain 里的 passphrase。读到就自动写到新文件 + 清掉 keychain 条目，下次进 fast path。`passphrase_is_set()` 也相应认得 keychain 残留，所以 `sessync status` 显示对。
- v0.2.0 升级到 v0.2.1 后，第一次跑 `sessync push / resume / status` 会**透明地完成迁移**，无需任何手工操作。

### 升级（从 v0.2.0 或 v0.1.x）

```bash
brew upgrade sessync
sessync push   # 触发自动迁移；之后一切如常
```

如果你已经在 v0.2.0 上手动跑过 `sessync init` 重新设置 passphrase，升级到 v0.2.1 也无害（fast path 直接命中文件）。

## [0.2.0] — 2026-05-05

主题：**自动 push、resume 提速、不再被 macOS 密码窗骚扰**。

### 新增

- **`sessync hook install / uninstall / status`**（A1）—— 在 `~/.claude/settings.json` 装一个 Stop hook，每次 Claude Code 对话结束自动跑 `sessync push --quiet`。幂等、原子写、保留你已有的其他 hook。
- **`sessync push --quiet`** flag —— 给 hook 用的安静模式。抑制正常输出但错误仍写日志。
- **加密本地 meta 缓存** `~/.cache/sessync/meta-cache.age`（P1）—— `sessync resume` 不再每次都从 OSS 拉所有 meta 文件，只下载远程真变了的那些。稳态 resume 时间从 5-15s 降到 ~1s。list 调用每次仍跑（作为 truth source）。
- **machine-bound 加密 passphrase 文件**（K-new）—— 完全弃用 macOS Keychain。passphrase 加密存在 `~/.config/sessync/passphrase.enc`，加密 key = HMAC-SHA256(machine_id, "sessync-passphrase-v1")。**安装和升级都不再弹密码窗**。Linux/Windows 跨平台 ready（machine_id 来源各 OS 已就位）。
- **彩色输出**（L6）—— 引入 `owo-colors`，提供小型 style 模块（`src/ui/style.rs`）。非 TTY 自动禁用，遵循 `NO_COLOR` 环境变量。
- **`sessync init` UI 重做**（L3）—— 分组标题、OSS endpoint 改成 6 个常用阿里云区域 + Custom 的 Select、保存成功打 ✓ 标记。
- **`sessync status` UI 重做**（L4）—— 分组输出（设备/Sessions/Health）、相对时间格式化、✓/✗ 标记、缓存健康度。
- **`sessync` 不带子命令默认显示 status**（L2），不再是 clap 默认 help。help 仍可用 `sessync help` 或 `--help`。
- **`sessync uninstall --purge-remote` 强制二次确认输 bucket 名**（S1）—— 防误删，类似 1Password 的不可逆操作确认机制。

### 变更

- **resume picker 按 mtime 倒序排列**（U1），不是按字母序的 project_key。最近用过的 session 排第一。
- **session preview 长度** 从 80 字符提到 200 字符（U2）。捕获时（`first_user_message_preview`）和显示时（`truncate(..., 200)`）都改了。
- **`sessync uninstall`** 现在也顺手删除 meta 缓存文件（之前会留 stray artifact）。
- **`StorageObject::last_modified`** 加了 doc 注释说明各 backend 精度差异（缓存按字节比对）。
- 所有命令不再调用 macOS Keychain。`src/keychain.rs` 文件保留（标记 deprecated），可能用于将来 v0.1.x 迁移辅助。

### 修复

- Stop hook 幂等性现在同时匹配尾部 tag **以及** `"sessync push"` 命令前缀，所以用户手动改了 hook（去掉了 tag）后再 install 也不会重复添加。
- 缓存加载时若 `$HOME` 未设置不再让 resume 整个失败 —— 缓存是纯优化，回退到内存空缓存。
- Hook install 在 `$HOME` 未设置时报错退出（之前会把 hook 装到 cwd 下叫 `~` 的目录里）。

### 从 v0.1.x 升级

如果你从 v0.1.x 通过 `brew upgrade sessync` 升级：

1. `~/.config/sessync/config.toml` 旧配置可以直接用。
2. **必须重新设置 passphrase** ——v0.2.0 不再使用 v0.1.x 留在 macOS Keychain 里的条目。重跑 `sessync init` 选择覆盖，**输入相同的 passphrase**（必须一致 —— OSS 上的数据是用 passphrase 派生的 key 加密的）。新文件 `~/.config/sessync/passphrase.enc` 会自动创建。
3. 可选：装自动 push hook —— `sessync hook install`。

## [0.1.2] — 2026-05-05

主题：发版流水线全自动化。

### 新增

- **GitHub Actions release workflow** —— 每次 push `v*` tag 自动 build 跨架构 macOS universal binary 并创建 GitHub Release（含 `.tar.gz` + `.sha256`）。
- **自动更新 brew tap formula** —— release 跑完后自动 push 新版本号 + sha256 到 `echobyte26/homebrew-sessync` 仓库，所以 `brew upgrade sessync` 真正端到端可用，无需任何手工操作。
- **`sessync install`** 子命令 —— AirDrop 之后一条命令搞定部署。复制 binary 到 `~/.local/bin/`、去掉 Gatekeeper 隔离 xattr、ad-hoc codesign、检查 PATH。
- **`sessync uninstall`** 子命令，`--purge-remote` 选项可同时清空 OSS bucket 前缀。

### 修复

- OSS `list` 现在用 `aliyun_oss_client::Error::ParseXml(serde_xml_rs::Error::Custom)` 类型匹配处理空 bucket 情况 —— 之前在第一次 push 前总会因为 `missing field 'Contents'` 失败。
- OSS 错误格式从 `{e}` 改成 `{e:?}`，SDK 错误链路完整可见。

## [0.1.1] — 2026-05-05

### 修复

- Tap 自动 bump workflow 鉴权失败 —— `gh repo clone` 之后的 `git push` 拿不到 `GH_TOKEN`，现在通过 `git remote set-url` 把 token 嵌入 remote URL 解决。

## [0.1.0] — 2026-05-04

首次发布。M1 = 手动 `init` / `push` / `resume` / `status`，已在两台 Mac 之间通过阿里云 OSS 端到端验证。

### 核心功能

- **跨设备同步** Claude Code agent session，使用阿里云 OSS + 客户端 age 加密。
- **`sessync init`** —— 交互式首次设置（OSS endpoint/bucket/AK/SK + passphrase）。
- **`sessync push`** —— 加密所有本地 Claude Code session jsonl 及 SessionMeta 元数据，上传到 OSS。
- **`sessync resume`** —— 交互式选单（项目 → session → 落到当前 cwd），打印 `claude --resume <id>` 提示。
- **`sessync status`** —— 本地+远程 session 数量、最后上传时间、passphrase 状态、OSS bucket 摘要。
- **`sessync init --mock`** —— 本地文件系统 backend，烟测用（不需要 OSS 账号）。

### 架构

- 双 trait 适配器：`ToolAdapter`（v1 仅 `ClaudeCodeAdapter`）+ `StorageAdapter`（`OssStorage` + `InMemoryStorage` + `LocalFsStorage`）。
- 加密：argon2id（m=64MiB, t=3, p=4）→ 32 字节 key → age passphrase 模式。
- 跨设备共享 salt 通过 OSS 对象 `<prefix>.sessync-salt` 实现（M1 烟测期间发现 B1 设计 bug 并修复）—— 现在跨设备只需要保证 passphrase 一致。
- 跨路径 resume 验证通过：在 Mac A 的 `/Users/A/foo` 录的 session，可以在 Mac B 的 `/Users/B/bar` 成功 `claude --resume`。

[0.2.3]: https://github.com/echobyte26/sessync/releases/tag/v0.2.3
[0.2.2]: https://github.com/echobyte26/sessync/releases/tag/v0.2.2
[0.2.1]: https://github.com/echobyte26/sessync/releases/tag/v0.2.1
[0.2.0]: https://github.com/echobyte26/sessync/releases/tag/v0.2.0
[0.1.2]: https://github.com/echobyte26/sessync/releases/tag/v0.1.2
[0.1.1]: https://github.com/echobyte26/sessync/releases/tag/v0.1.1
[0.1.0]: https://github.com/echobyte26/sessync/releases/tag/v0.1.0
