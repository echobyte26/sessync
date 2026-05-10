# 更新日志

记录 sessync 的所有重要变更。格式参考 [Keep a Changelog](https://keepachangelog.com/)。

## [0.5.0] — 2026-05-10

主题：crypto 路径加速 + 几个 quality-of-life 小功能 + 一个 dev-time bug 修复。

### 新增

- **`sessync auto-push setup / teardown / status`** —— 一条命令搞定 hook + launchd 的安装/卸载/状态查询。新设备初始化少一步。
- **`sessync logs --since 1h --failed`** —— 过滤 outcomes 历史。`--since` 接 `30s`/`5m`/`1h`/`2d`；`--failed` 只显示失败记录。debug hook/launchd 失败时方便。

### 变更（性能）

- **Q1 crypto 路径换 XChaCha20-Poly1305 直接加密** —— 之前 `crypto::encrypt` 用 `age::Encryptor::with_user_passphrase`，age 内部又跑一次 scrypt KDF 派生内容 key —— **跟我们的 argon2id 重复**，浪费 ~200ms / op。现在直接用 argon2id 派生的 32 字节 key 喂 XChaCha20-Poly1305。
  - **格式兼容**：新文件加 `SSC1\0\0\0\0` magic 前缀。`decrypt` 看到 magic 走 xchacha20，没看到走老的 age（v0.1.0–v0.4.0 的所有文件都能解）。
  - **零迁移**：OSS 上的旧 session、`~/.config/sessync/passphrase.enc`、meta cache 第一次解密走老路径透明完成；下次 encrypt 用新格式。
  - **收益**：push/resume 每个 session 省 ~200ms。Resume 27 个 session 大概省 5 秒。
  - age 依赖**保留**给向后兼容路径用。

### 修复

- **Q4 `list_local_sessions` 单目录失败不再拖垮全 list** —— 一个权限拒绝 / 损坏的 project dir 之前会 `?` 直接 abort，所有 session 都看不到。现在 per-dir / per-file `match` + `tracing::warn!` + `continue`，问题目录被跳过、其他都列出来。
- **删掉 dev-time 真发 macOS 通知的测试** —— `notify_does_not_panic_on_macos` 测试真调用 `osascript`，每次 `cargo test` 给开发者 Notification Center 弹一条 "test title"。开发 v0.4.0/v0.5.0 期间累积了几十条。**对最终用户无影响**，但开发者本地能感觉到。

### 升级

```bash
sessync upgrade            # 从 v0.4.x 升上来
sessync push               # 第一次还会全量上传一次（XChaCha20 格式）
                           # 之后稳态恢复 skip 模式
```

无需重 init。配置 / passphrase / OSS 数据全部向后兼容。

### 给 v0.6.0 的笔记

- C3 OSS conditional-put：C-etag 实战经验积累后再决定要不要做
- Q2 SecretString：理论安全 fix，性价比偏低，可考虑
- launchd kickstart 入口可能需要补 doctor 的检查项（"agent is loaded but never executed" 这类）

## [0.4.0] — 2026-05-10

主题：**真正的跨机器撞车检测（C-etag）**，加上一些 v0.3.x 实战暴露的小问题修复。

紧凑版本 —— 5 个功能改动而不是 v0.3.0 那种 15 项一锅端。

### 新增

- **C-etag 跨机器撞车检测**（marquee）—— 按 session 跟踪 OSS ETag。每次 push 成功后把新 ETag 记进 SQLite 队列；下次 push 前 list 远程，对比记录的 vs 当前的：一样 = 是我自己 push 的，不一样 = 别人 push 过 → 真正 stale。**v0.3.2 砍掉的 `--fork-on-conflict` 和 stale-warn 功能正式复活**，可以真正区分自己改的和别人改的，跨机器同时改 session 不再 silent last-writer-wins。
- **`sessync upgrade`** —— 一条命令搞定 brew update + brew upgrade sessync。自建 tap 不会自动同步，之前要记两条命令。`brew` 不在 PATH 时给清晰报错。
- **`StorageObject` 新增 `etag` 字段**（API 变更）—— 三个 backend 都返回。OSS 从 list 响应里直接拿（已经包含），LocalFs/InMemory 从内容 sha256 合成（OSS 的 quoted-hex 格式）。
- **新 `StorageAdapter::head(key)` 方法** —— 拿单个对象的最新 (etag, mtime)。OSS 用 `?objectMeta` query（cheap），其他 backend 同步合成。push 完用它拿新 ETag 写回队列。

### 修复

- **launchd 用 `bootstrap` / `bootout` 替代 legacy `load -w`**（macOS 14+ 推荐）—— `launchctl load -w` 文档列为 legacy，实战里出现"装上去过几天自己变 NOT LOADED"的怪现象。换成现代 API 更稳。idempotent install：`bootstrap` exit code 5 ("already loaded") 当成功处理。
- **launchd plist 用 brew symlink 路径而不是 cellar 路径** —— 之前写的是 `/opt/homebrew/Cellar/sessync/0.3.0/bin/sessync`，每次 `brew upgrade` 后那个版本号目录就没了，plist 失效用户得重跑 `sessync launchd install`。改成 `/opt/homebrew/bin/sessync` symlink 路径，brew 永远维护这个 link。
- **`Q5` OSS 调用加 30 秒 timeout** —— 网络挂死时 hook 不再无限等。`tokio::time::timeout` 包 put/get/list/delete/head 全部 4+1 个方法。
- **`Q3` preview 单行 1 MiB cap** —— `first_user_message_preview` 跳过超大行，避免 50MB 粘贴日志独占内存。

### 升级

```bash
sessync upgrade        # 新命令，一条搞定
                       # 或老办法： brew update && brew upgrade sessync
sessync launchd uninstall && sessync launchd install   # 推荐：让 launchd 切到新 bootstrap API + symlink 路径
sessync status         # 看 Auto-push 区段，launchd 应该 LOADED
sessync push --dry-run # 验证 ETag 路径正常
```

### 给 v0.5.0 的笔记

- **C3** OSS conditional-put（`x-oss-forbid-overwrite`）：C-etag 落地后边际价值降低。撞车窗口现在能检测能 fork，C3 是更严格的"原子写入"防护，v0.5.0 候选
- **Q1** 替换 age 内置 scrypt 为直接 chacha20-poly1305，省 200ms KDF 时间
- **Q2** passphrase / key 用 `secrecy::SecretString`，drop 时 zeroize

## [0.3.2] — 2026-05-08

主题：v0.3.1 没真修好——A5 增量 push 终于真生效了。

### 修复

- **A5 增量 push 仍然失效**（v0.3.1 的容差办法不够）。v0.3.1 给 `is_stale` 加 60 秒容差，本意是让 OSS PUT 时间晚于本地 mtime 几秒的正常情况不再误报。但实际场景里，本地 jsonl 文件上次被写经常是几小时甚至几天前的事，而每次 hook push 又会刷新 OSS mtime 到当下——差距远远超过 60 秒，stale 还是永真，A5 跳过仍然走不到。表现：升级到 v0.3.1 后 `sessync logs` 还是每条 `pushed 27 (skipped 0)`。
- 修法：**直接砍掉 stale 检测**。现在的判断只剩两条：远程 mtime >= 本地 mtime 跳过；本地 newer 上传；远程不存在上传。无 ETag 状态跟踪的前提下，光靠 mtime 本来就分不清"我自己 push 的"和"别人 push 的"——两种情况都是 `remote > local`。v0.3.0 引入的 stale 警告和 fork-on-conflict 在没 ETag 之前注定误报，强行保留只会让 A5 失效。
- **保留** `--no-stale-warn` 和 `--fork-on-conflict` 两个 flag 不破坏 CLI 兼容，但它们现在是 no-op（接受参数、什么都不做）。真正的 race-free 检测留给 v0.4.0 的 C-etag。

### 升级

```bash
brew update && brew upgrade sessync
```

升级后第一次 push 还会上传几个（本地写入比远程新的那些），**之后**稳态就是 `pushed 0 (skipped 27)` 或 `pushed 1 (skipped 26)`，hook 频繁触发也不再无意义全量。

### 给 v0.4.0 的笔记

- C-etag：按 session 跟踪 OSS ETag，`push` 时拿当前远程 ETag 跟自己上次记录的对比，不一样就是别人 push 过 → 真正的 stale 检测可以吃这个信号
- 上面修好后再让 `--fork-on-conflict` 真正起作用，并恢复 stale-warn 的语义

## [0.3.1] — 2026-05-07

主题：修两个 v0.3.0 上手就被发现的 bug —— 增量 push 没生效 + 时间戳显示错时区。

### 修复

- **A5 增量 push 失效（每次都全量）**。`is_stale(remote, local)` 直接比 OSS PUT 收到时间 vs 本地文件 mtime —— 这两个时钟在正常单机 push 后**永远是 remote > local**（OSS 记录的时间晚于本地文件最后写入时间几百毫秒到几秒）。结果：每次 push 都进 C1 的 stale 分支强制覆盖，A5 的 skip 分支永远到不了。表现：`sessync logs` 看到每条都是 `pushed 27 (skipped 0)`。
- 修法：`STALE_TOLERANCE_SECS = 60`，stale 只在远程比本地新 60 秒以上才触发。能正确忽略 PUT 收到时间的几秒误差，同时仍然能抓住真正的跨机器冲突（一般差好几分钟到几小时）。
- 真正 race-free 的冲突检测需要按 session 跟踪 ETag —— 留给 v0.4.0（C3 backlog）。
- **CLI 输出时区错**。`sessync push --dry-run` / `push` 的 tracing info 行用 UTC（`2026-05-07T11:31:54.272362Z`），用户在 +0800 看是错位 8 小时；`sessync logs / ls / resume` 的绝对时间也都是 UTC。
- 修法：tracing 直接 `without_time()`（CLI 用户不需要时间戳，相对时间足够）；`logs` / `ls` / `resume` 的 `[YYYY-MM-DD HH:MM]` 转本地时间；`ls --json` 的 modified_at 保留 UTC RFC3339（机器可读不能歧义）。

### 升级

```bash
brew update && brew upgrade sessync
```

升级完后第一次 hook push 仍会是全量上传（远程 mtime 已经被 v0.3.0 写错了一通），之后稳态会显示 `pushed 0 (skipped 27)` 或 `pushed 1 (skipped 26)`。

## [0.3.0] — 2026-05-06

主题：**push 不再傻全量、不再丢失败、撞车有得救；新命令 doctor / logs / ls；可见性大幅提升**。

合并了原计划的 v0.3.0 + v0.4.0 两轮迭代，15 项一起发。C3 OSS 条件写入推到 v0.4.0（C2 已经覆盖了大部分数据保护场景）。

### 新增

- **`sessync push` 增量上传**（A5）—— 一次 list 远程拿到所有对象的 mtime，本地 session 的 `modified_at <= 远程 last_modified` 直接跳过。输出从 `pushed N` 改成 `pushed N (skipped M unchanged)`。稳态 push 几乎不上传任何字节。
- **`sessync push <session-id>...` 选择性 push**（A6）—— 只推指定 session id。多个并列。未知 id 直接 fail。
- **`sessync push --dry-run` 预览**（S2）—— 跑一遍计算，但不上传、不进队列、不发通知。逐 session 打印 `would push / would skip / would fork`，最后一行汇总。
- **`sessync push --fork-on-conflict` 撞车保留**（C2）—— 远程比本地新（其他设备插队推过）时，本地版本另存为 fork：`{session_id}.fork-{8 hex}.age`。原远程文件不动。fork 用独立 session_id（`{原 id}.fork-{hash}`），所以 `sessync resume` 会同时看到两份并排比较。
- **stale-overwrite 警告**（C1）—— 默认行为：远程比本地新时仍覆盖（last-writer-wins），但 stderr 打警告。`--no-stale-warn` 可静默。
- **持久化 push 队列**（A3）—— SQLite 在 `~/.local/share/sessync/queue.db`。每次 push 失败的 session 进队列，下次 push 自动重试（60 秒冷却避免 hook 抖动）。每次 push 的成功/失败摘要也记进 `push_outcomes`（保留最新 100 条）。
- **macOS 连续失败通知**（A4）—— 队列连续失败计数 == 3 时（不是 ≥3，避免持续报警）调 osascript 弹通知"sessync push failing"。Linux/Windows 静默。
- **`sessync launchd install/uninstall/status`**（A2）—— 装 `~/Library/LaunchAgents/com.sessync.push.plist`，每 30 分钟跑一次 `sessync push --quiet` 兜底。Stop hook 是低延迟主路径，launchd 是"笔记本盖了" / "hook 挂了"的保险网，队列把两边失败的合并起来重试。
- **`sessync doctor`**（D1）—— 体检命令。逐项 ✓/✗ 检查 Config / Storage / Hook / launchd（macOS）/ Queue / Cache / PATH。失败行带 hint（auth 错误就建议轮 key，DNS 错误就建议查网络）。任何 fail 退出码 1，可入 CI/监控。
- **`sessync logs`**（D2）—— 看最近 push 历史。读 `push_outcomes` 表，按时间倒序打印，相对时间 + ✓/✗ marker。`-n 50` 控制条数。`sessync hook install` 后 push 在后台跑，`logs` 是用户能看到失败原因的唯一界面。
- **`sessync ls`**（U4）—— 非交互列出远程 session。按 project 分组 + recency 排序。`--project <key>` 单项目过滤，`--json` 机器可读。复用 resume 的 meta cache，没多余请求。
- **`sessync resume` 自动启动 claude**（U3）—— resume 选完 session 落地后自动 exec `claude --resume <id>`，一条命令到家。不在 PATH 时退回打印命令行。`--no-launch` 保留旧行为。
- **`sessync status` 新增 Auto-push 区段**（D3）—— 显示 hook / launchd / queue pending / last push outcome（含相对时间 + 摘要）。一眼看清自动 push 是不是健康。
- **`sessync completions <shell>`**（L1）—— 输出 zsh / bash / fish / powershell / elvish 的补全脚本。pipe 到 shell 的补全目录即可。
- **`sessync --help` 加 EXAMPLES + CONFIG + DOCS 段**（L5）—— 默认 clap help 太干，新人不知道从哪下手；现在底部有典型工作流和关键路径。

### 变更

- **resume picker mtime 排序保持 + cache 命中跳 GET**（前几版已有，本版无变化但显著影响日常体验）
- **`sessync push` 错误聚合**（A3 副产物）—— 单 session 上传失败不再中断整批，所有错误最后统一 surface，hook 仍能拿到非 0 退出。
- **`sessync uninstall` 不再清 keychain 残留** —— K-new 已经弃用 keychain。

### 文档

- README、ARCHITECTURE、FAQ 已是中文，未动；CHANGELOG 本条新增。
- backlog: `docs/superpowers/v2-backlog.md` 同步标记 15 项 shipped，C3 移到 v0.4.0。

### 推迟到 v0.4.0

- **C3 OSS 条件写入**（atomic put with `x-oss-forbid-overwrite`）—— 需要绕过 aliyun-oss-client SDK 手搓签名 PUT，工作量 + 调试风险偏离这一轮节奏。在 C2 + C1 + 队列 + launchd 兜底之后，纯并发撞车的窄窗口才需要它，性价比降低，留 v0.4.0 单独做。

### 升级（从 v0.2.x）

```bash
brew upgrade sessync
sessync hook install      # 如果还没装；幂等
sessync launchd install   # 新功能；兜底定时 push（macOS）
sessync status            # 看新的 Auto-push 区段
```

不需要重跑 init，passphrase / 配置 / OSS 数据都向后兼容。

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

[0.5.0]: https://github.com/echobyte26/sessync/releases/tag/v0.5.0
[0.4.0]: https://github.com/echobyte26/sessync/releases/tag/v0.4.0
[0.3.2]: https://github.com/echobyte26/sessync/releases/tag/v0.3.2
[0.3.1]: https://github.com/echobyte26/sessync/releases/tag/v0.3.1
[0.3.0]: https://github.com/echobyte26/sessync/releases/tag/v0.3.0
[0.2.3]: https://github.com/echobyte26/sessync/releases/tag/v0.2.3
[0.2.2]: https://github.com/echobyte26/sessync/releases/tag/v0.2.2
[0.2.1]: https://github.com/echobyte26/sessync/releases/tag/v0.2.1
[0.2.0]: https://github.com/echobyte26/sessync/releases/tag/v0.2.0
[0.1.2]: https://github.com/echobyte26/sessync/releases/tag/v0.1.2
[0.1.1]: https://github.com/echobyte26/sessync/releases/tag/v0.1.1
[0.1.0]: https://github.com/echobyte26/sessync/releases/tag/v0.1.0
