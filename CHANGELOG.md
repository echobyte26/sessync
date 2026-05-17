# 更新日志

记录 sessync 的所有重要变更。格式参考 [Keep a Changelog](https://keepachangelog.com/)。

## [0.7.4] — 2026-05-18

主题：**带宽暴增紧急修复**——v0.7.0/v0.7.1 引入的 2 分钟自动 pull + 每次 list 全 prefix 导致用户 OSS 每月 20+ GB 外网流量，账号触发 UserDisable。

### 修复

- **新命令 `sessync sync`** —— 内部依次跑 push + pull，共享 list 响应缓存。launchd plist 改为直接调 `sessync sync --quiet`，**同一 cycle 内一次 list 调用同时给 push 和 pull 用**。比之前 `sh -c "push && pull"` 各自 list 节省 50% list 流量。
- **launchd 默认 interval 改回 1800 秒（30 分钟）** —— v0.7.1 的 120 秒太激进，配合 list 流量放大导致月流量 20+ GB。30 分钟 + sync 共享 cache 后预期月流量 < 1 GB。
- **SIGPIPE 优雅处理** —— `sessync push --dry-run | head -20` 之前会 panic（BrokenPipe），原因是 Rust 默认 SIGPIPE 处理是 abort。main.rs 启动时设 `SIG_DFL`，pipe 关闭就正常退出。

### 升级（**必须重装 launchd**）

```bash
sessync upgrade

# 关键：旧 plist 还是 sh -c "push && pull" 不走新 sync 命令
# 必须重装才能享受新 cache + 30 分钟间隔
sessync auto-push teardown && sessync auto-push setup
```

不重装的话还是老的 4 list / cycle × 2 分钟 / cycle，照样月流量 20+ GB。

### 预期效果

```
v0.7.1 (2 分钟 + 4 list/cycle):  ~40 GB/月外网流量
v0.7.4 (30 分钟 + 2 list/cycle): ~500 MB/月

OSS 标准价节省: ¥20/月 → ¥0.25/月
```

不再担心账号被流量费触发禁用。

## [0.7.3] — 2026-05-16

主题：**零配置插件过滤的根本性修法**——拿掉硬编码路径黑名单，换成通用启发式 + 从 jsonl 读真实 cwd。

### 背景：v0.7.2 黑名单为啥失效

`v0.7.2` 加了硬编码黑名单（`/.claude/plugins/`, `/.claude-mem/`, `/.codex/plugins/`），但用户实际 1478 个 claude-mem session 没被拦住。根因：

1. **Claude Code 路径编码**把 `/` 和 `.` 都换成 `-`：`/Users/X/.claude-mem/foo` → `-Users-X--claude-mem-foo`
2. sessync 之前从**目录名反解** source_cwd，反解把 `-` 都变回 `/`，**信息丢失**：`-Users-X--claude-mem-foo` → `/Users/X//claude/mem/foo`（点没了，原始 `-` 也变 `/`）
3. 黑名单匹配的是 `/.claude-mem/`，但实际 source_cwd 是 `/claude/mem/`，**对不上**

### 修复（两层）

**1. `ClaudeCodeAdapter` 改为从 jsonl 内容读真实 cwd**

不再用目录名反解。打开 jsonl 扫前 50 行，找第一个带 `"cwd"` 字段的 event（`attachment` / `assistant` / `user` / `system`），用那个原始路径作为 source_cwd。原始路径**保留所有点和短横**。

扫不到 cwd 时（防御性）回退到老的目录名反解 + `tracing::warn!`。

**2. 通用 dotfile 启发式替代硬编码路径**

新 helper `is_plugin_cwd_under_home(cwd, home)`：判断 cwd 是否是 `$HOME/.<任何东西>/...` 形式。

```
/Users/X/.claude-mem/observer    → true  (plugin)
/Users/X/.claude/plugins/foo     → true  (plugin)
/Users/X/.codex/plugins/bar      → true  (plugin)
/Users/X/.future-plugin-v2/baz   → true  ✓ 自动防御任何未来插件
/Users/X/Project/azoth           → false (user)
/Users/X/code/.git/internal      → false ✓ git 子目录不误伤
```

`ExcludeConfig::matches()` 现在是两层：
- Layer 1（默认开启）：上述 dotfile 启发式
- Layer 2（向后兼容）：v0.7.2 加的 `[exclude] project_path_contains` 用户自定义子串

**Layer 1 拿掉了 `HARDCODED_PLUGIN_PATHS`** —— 启发式覆盖所有该排除的，**且不需要维护具体路径列表**。装任何 plugin（claude-mem1、foo-helper-v3、whatever）只要它走 `$HOME/.X/` 规范，自动被识别。

### 行为变化

- **新 push 上去的 session：** source_cwd 是原始正确路径，启发式精准过滤。
- **v0.7.2 之前 push 的老数据：** source_cwd 是反解后的 garbled 形式（`/Users/X//claude/mem/...`），启发式可能漏过。但 v0.7.2 已经允许用 `[exclude] project_path_contains` 加 `"claude/mem"` 这种子串补充过滤。
- **正常用户路径完全不受影响：** `~/Project/`, `~/code/`, `~/Documents/` 都不以 `.` 开头，启发式不会误伤。

### 升级

```bash
sessync upgrade
sessync auto-push setup    # 重新打开自动 push（如果之前 teardown 关了）
```

不需要改 config。`[exclude]` config 仍然有效作为补充手段（向后兼容）。

### 不再需要维护

- ❌ HARDCODED_PLUGIN_PATHS 常量
- ❌ 每出新 plugin 加一条
- ❌ 用户为不同 plugin 配 `[exclude]`（启发式覆盖了）

## [0.7.2] — 2026-05-15

主题：**插件污染防御 + OSS 分页 + 远程清理**——真实场景驱动的紧急 fix。

用户装了 [claude-mem](https://github.com/thedotmack/claude-mem) plugin，它的 observer 在 `~/.claude-mem/observer-sessions/` 跑了 1500+ 个 Claude subprocess sessions。sessync 全推 → 爆 OSS list 的 1000 对象首页上限 → 老 session 看不到。

### 新增

- **零配置硬黑名单**——默认就过滤这 3 个路径下的 session，用户啥都不用配：
  - `/.claude/plugins/` （所有 Claude Code marketplace plugin）
  - `/.claude-mem/` （claude-mem 整个数据目录）
  - `/.codex/plugins/` （所有 Codex plugin）
  
  装上 sessync 就生效，新装这些 plugin 自动忽略它们的 subprocess session。不可关。

- **`[exclude]` config**：跟硬黑名单**叠加**的用户补充。`~/.config/sessync/config.toml` 新增 `[exclude]` 段，按 `source_cwd` 子串过滤。`push` / `pull` / `ls` 都生效。
  ```toml
  [exclude]
  project_path_contains = ["claude-mem", "plugins/marketplaces"]
  ```
  匹配的 session 不上传、不显示、不下载。push 时多打一行 `excluded N sessions matching [...]`。
- **`sessync purge --pattern <substring>`** 新命令——按 `source_cwd` 子串清远程 OSS 数据。
  ```bash
  sessync purge --pattern claude-mem --dry-run    # 预览
  sessync purge --pattern claude-mem               # 提示输入 "delete" 确认后批量删
  sessync purge --pattern claude-mem -y            # 跳过确认（脚本用）
  ```
  并行删（buffered 8），返回删除的 .age + .meta.json 对数。

### 修复

- **OSS list 现在分页**——之前只读首页 1000 对象，>500 session 老的会"消失"。改成循环 `next_token` 累加全部页，cap 50 页（50,000 对象）防死循环。

### 升级 + 紧急清理流程

```bash
# 1. 升级
sessync upgrade

# 2. 加 exclude config 防止再污染
cat >> ~/.config/sessync/config.toml <<'TOML'

[exclude]
project_path_contains = ["claude-mem", "plugins/marketplaces", "observer-sessions"]
TOML

# 3. 看远程有多少脏数据要清
sessync purge --pattern claude-mem --dry-run
# 看清楚输出再继续

# 4. 清理（会要求输 "delete" 确认）
sessync purge --pattern claude-mem
sessync purge --pattern observer-sessions

# 5. 重启自动 push
sessync auto-push setup
```

清完之后 `sessync ls --tool claude-code` 应该恢复显示真正的项目 session。

### 为什么 case-sensitive 子串而不是 glob/regex

少加依赖、配置简单。99% 的 plugin 污染都是路径里有特定关键字（claude-mem、plugins、marketplaces 之类），子串够用。

## [0.7.1] — 2026-05-12

主题：launchd 默认间隔从 30 分钟改到 **2 分钟**，加 `--interval` flag 可调。

### 变更

- **launchd `StartInterval` 默认 1800 → 120 秒**。push+pull 现在每 2 分钟跑一次，跨机器同步收敛快得多。代价：每天多几百次 OSS list 调用（每次稳态都是 skip，不传字节）+ 笔记本上几百次短暂唤醒。
- **`sessync launchd install --interval <secs>`** 新 flag，自定义间隔：
  ```bash
  sessync launchd install --interval 60     # 1 分钟
  sessync launchd install --interval 300    # 5 分钟
  sessync launchd install --interval 1800   # 老的 30 分钟
  ```
- **install 输出更友好**——根据 interval 秒数自动选 "2 minutes" / "30 seconds" 等显示。

### 升级（从 v0.7.0）

```bash
sessync upgrade
sessync auto-push teardown && sessync auto-push setup   # 重装 plist 切到 120s
sessync launchd status                                   # LOADED
```

`auto-push setup` 用新默认 120s。要别的间隔走 `sessync launchd install --interval N` 单独装。

## [0.7.0] — 2026-05-12

主题：**双向自动同步闭环 + resume 分层选择器**。

之前只有 push 是自动的，receive 端必须手动 `sessync resume` 一次拉一个。v0.7.0 加上 `sessync pull` 命令 + launchd 自动 pull，跨机器同步终于"啥都不用做"。同时 resume picker 从混排改成 3 级 drill-down，多工具时找 session 不再翻一长串。

### 新增

- **`sessync pull` 命令**（marquee）—— 跟 push 完全对称：list 远程 ETag → 跟本地 mtime + 记录的 ETag 比 → 增量下载 + 解密 + 写本地。
  - `sessync pull` 推所有工具的 session
  - `sessync pull --tool codex` 单工具
  - `sessync pull --dry-run` 预览
- **launchd 自动 push + pull** —— plist 改为 `/bin/sh -c "sessync push --quiet && sessync pull --quiet"`，每 30 分钟跑一次。push 失败时 pull 也跳过（下次再补，幂等）。**Stop hook 不变**——你刚结束对话就拉别人的更新不合直觉，pull 完全 launchd 驱动。
- **`sessync resume` 3 级分层选择器**：
  ```
  Step 1: 选 agent      → Claude Code / Codex
  Step 2: 选项目        → 该工具下的 project（按 mtime 排）
  Step 3: 选 session    → 该项目下的 session
  ```
  `--tool X` 跳过 Step 1；`--project Y` 跳过 Step 2；都给就直接到 Step 3。
- **`sessync resume --restart-app`**（Codex 专用）—— write_session 成功后 `killall Codex && open Codex.app`，强制 Codex.app 重新加载 SQLite 看到新 session。仅 Codex 生效，Claude Code 无副作用。
- **`sessync hook install --tool codex`** 安装完会**额外打印一行提示**：`NOTE: 去 Codex.app 点 "hook needs review" 批准这个 hook，否则不会生效`。`sessync auto-push setup` 也加同样提示。

### 给 v0.7.0+ 的注意

- **Codex 自动 pull 的 cwd 限制**：pull 写入时用 `meta.source_cwd`（源机器的路径）作为 target_cwd。对 Claude Code 没问题；对 Codex 的话，**Codex.app 侧边栏可能看不到**（按精确 cwd 分组，源机器路径在本机不存在）。`codex resume <uuid>` 还能用，但走 Codex.app UI 体验差。要在 Codex.app 看到，目前需要**交互式** `sessync resume`（在项目目录里手动跑）。
- 真正的修法是 **path mapping table**：config.toml 里配 `[path_map] "/Users/mini-user" = "/Users/pro-user"`，pull 时自动映射。v0.7.1+ 候选。

### 升级（从 v0.6.x）

```bash
sessync upgrade
sessync auto-push teardown && sessync auto-push setup   # 重装 launchd 切到新 push+pull 模式
sessync hook install --tool codex                       # 没装的话装上（按提示批准）
sessync pull                                            # 立即拉一次远程更新
```

老版本数据无需迁移。launchd 老 plist 还是只跑 push，**必须重装才能用 pull**。

## [0.6.2] — 2026-05-12

主题：修 v0.6.1 后 Codex resume 报 "Model provider `unknown` not found"。

### 修复

- **Codex `model_provider` INSERT 不能写 'unknown'** —— v0.6.0 起 `CodexAdapter::write_session` 把 model_provider 硬编码成 `'unknown'`，Codex resume 时直接报错 "failed to load configuration: Model provider \`unknown\` not found"，session 在 sidebar 看得见但打不开。修法：从 rollout jsonl 第一行的 `session_meta.payload.model_provider` 读真实值（一般是 `"openai"`），读不到 fallback 到 `"openai"`。ON CONFLICT 路径也加进 model_provider 的 update，避免老行残留。
- 副带影响：之前 v0.6.0 / v0.6.1 期间同步到本地的 session 数据库行**已经写了 'unknown'**，**升级 v0.6.2 后这些老行不会自动修复**。要么删了重 resume，要么手动 `sqlite3` UPDATE。见下方迁移说明。

### 升级

```bash
sessync upgrade
```

**已经同步过的 Codex session 修复**（如果你装了 v0.6.0 或 v0.6.1）：

```bash
# 方案 A：批量改 SQLite（最快）
sqlite3 ~/.codex/state_5.sqlite "UPDATE threads SET model_provider = 'openai' WHERE model_provider = 'unknown';"
killall Codex && open /Applications/Codex.app

# 方案 B：删了重新 resume（更彻底）
ROLLOUT=$(sqlite3 ~/.codex/state_5.sqlite "SELECT rollout_path FROM threads WHERE id LIKE '<uuid>%';")
sqlite3 ~/.codex/state_5.sqlite "DELETE FROM threads WHERE id LIKE '<uuid>%';"
rm "$ROLLOUT"
cd <项目目录>
sessync resume --tool codex
```

## [0.6.1] — 2026-05-12

主题：修 v0.6.0 上手就被 macOS Tahoe + Codex 跨机器场景暴露的 3 个 bug。

### 修复

- **`launchctl bootstrap` exit code 5 不该当成"已加载"** —— v0.4.0 切到 `bootstrap` API 时，错把 exit 5 视为"already loaded → 成功"。但 exit 5 在 `bootstrap` 子命令里其实是 **Input/output error**（通用 I/O 失败）。结果：`auto-push setup` 跑完显示 OK，但 launchd 那边根本没装上，`doctor` 跟着报 NOT loaded，用户一头雾水。现在 exit 5 正确 surface 为失败，错误信息带 `launchctl` 的 stderr。
- **macOS 15+ / Tahoe Login Items 失效引导提示** —— 在 macOS Tahoe (26.x) 和 Sequoia (15.x)，每次 `brew upgrade sessync` 换 binary 的 ad-hoc 签名，**Login Items 里的旧批准状态被自动撤销**，但 UI 上开关还显示打开。`launchctl bootstrap` 这时报模糊的 "Input/output error"（连 `sudo` 都不给详细信息）。现在 install 失败 + doctor 检测时都会打印**具体操作指引**："去 系统设置 → 通用 → 登录项与扩展 → 后台 → 找 sessync → 关掉再打开 重新批准"。
- **Codex 跨机器 resume 的 cwd 漂移** —— Codex 的 jsonl rollout 文件**第一行 embed 了原始 cwd**：`{"type":"session_meta","payload":{"cwd":"/Users/mini-user/...","..."}}`。Codex.app/CLI 启动会读 rollout reconcile SQLite，把我们 INSERT 的本地 cwd **覆盖回源机器路径**。结果：mini 上的 session 同步到 pro 后，Codex.app 找不到（按 cwd 分组项目，源 cwd 在 pro 上不存在）。修法：`CodexAdapter::write_session` 写入 jsonl **之前**改写第一行的 cwd 字段为 target_cwd。未知格式的 rollout 不动（保持 robustness）。

### 顺手 polish

- doctor 的 `launchd_loaded` 检查改用 `launchctl print`（新 API），代替 legacy `launchctl list | grep`（后者偶尔误报）。

### 升级

```bash
sessync upgrade
sessync auto-push teardown && sessync auto-push setup   # 让 launchd 切到新错误路径
```

如果 install 还是失败，照新提示**去系统设置 → 登录项与扩展 → 后台 → toggle sessync OFF→ON**。

### 给 v0.7.0 的笔记

- **`sessync pull` 命令 + 自动 pull**（marquee 候选）—— 当前 push 是自动的（hook + launchd），pull 只能手动 `sessync resume` 一次一个。双向自动同步才是真闭环。`sessync pull` 设计跟 push 对称：list 远程 ETag → 跟本地 mtime 比 → 增量下载 + decrypt + rewrite cwd + 写本地。launchd 兜底也可以跑 pull。

## [0.6.0] — 2026-05-11

主题：**Codex 支持**——sessync 不再只为 Claude Code 服务。

第二个 `ToolAdapter` 落地：OpenAI Codex CLI 的 session 现在能跟 Claude Code 一起同步。底层架构改造为多工具 dispatch，所有命令支持 `--tool` 过滤。

### 新增

- **`CodexAdapter`** —— 读取 `~/.codex/state_*.sqlite`（按版本号 glob，自动适配 state_5/state_6/...）+ `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` 的双层存储。`write_session` 安全往 SQLite 插行（写前自动备份、保留最近 3 份、未知 schema 直接拒绝不破坏）。复用 `path_codec::project_key_for_cwd`——同 cwd 的 Claude session 和 Codex session 会得到同样的 project_key。
- **多工具 dispatch**：
  - `sessync push` 默认推所有工具的 session
  - `sessync push --tool claude-code` / `--tool codex` 过滤
  - `sessync resume` picker 跨工具混排按 mtime DESC 排序
  - `sessync ls` 按工具分组（单工具时省略 header）
  - `sessync status` 多工具时显示 per-tool 计数
- **`sessync hook install --tool codex`** —— 写 `~/.codex/config.toml`（TOML 不是 JSON）+ 自动开 `[features] codex_hooks = true`。Codex 的 hook 必须开这个特性开关才会触发。
- **`sessync auto-push setup`** —— 现在会装所有工具的 hook，每个工具独立 ✓/✗ 报告。
- **doctor 新增 Codex 区段** —— 检测 `~/.codex/` 存在、`state_*.sqlite` 存在、`codex` binary 可达。Codex 未装时全用 Info 不是 Fail（不打扰非 Codex 用户）。
- **resume 选完后自动调用对应工具的 launch 命令** —— Claude 用 `claude --resume <id>`，Codex 用 `codex resume <uuid>`。`ToolAdapter` trait 新增 `launch_resume` 和 `launch_binary_on_path` 两个方法。
- **`sessync push --tool X`** 当目标工具没本地 session 时打印 "no local sessions to push" 不再是误导性的 `pushed 0 (skipped 0)`。
- **`--help` EXAMPLES 大幅扩充**：覆盖两个工具的典型工作流 + 各 tool 的关键路径（CONFIG / CLAUDE CODE / CODEX 三段）。

### 变更（API breaking）

- **`ToolAdapter` trait 新增 2 方法**：`launch_resume` 和 `launch_binary_on_path`。下游 impl 必须实现（M1 之外没有第三方 impl，影响仅内部代码）。
- **`sessync ls --json` 输出结构变了**：从 `{"projects": [...]}` 改为 `{"tools": [{"name": "...", "projects": [...]}]}`。加 Codex 后旧格式没法表达多工具。脚本依赖 JSON 的需要更新。
- **OSS key 布局**：`codex/<project_key>/<uuid>.age` 跟 `claude-code/...` 并存。同一 bucket、同一 passphrase，靠 prefix 分。

### 修复 / 加固

- **Codex SQLite 写入安全**：每次 `write_session` 前先备份 state_N.sqlite，保留最近 3 份。schema mismatch 时返回空 vec + warn，**绝不破坏用户的 Codex 数据**。
- **doctor hook 检查现在按工具分** —— 之前只检 Claude Code 的 settings.json；现在也检 Codex 的 config.toml。

### 升级（从 v0.5.x）

```bash
sessync upgrade
sessync hook install --tool codex     # 装 Codex 的 hook（自动开 codex_hooks）
sessync auto-push setup               # 或者一条命令搞定两个工具
sessync ls                            # 应该看到按工具分组的列表
sessync doctor                        # Codex 区段确认安装状态
```

如果你不用 Codex，**啥都不用动**——sessync push 不会推 Codex 的东西（因为没本地 session），doctor 显示 Info 不是 Fail。

### 给 v0.7.0 的笔记

- **Windows 支持**——Task Scheduler 替代 launchd，Windows toast 替代 osascript，path codec 处理 backslash
- **launchd 与 Codex 兼容**——目前 launchd 的 plist 只推 Claude 的 session（因为 plist 里 `sessync push --quiet` 没 `--tool`），无意中也会跑 Codex push 但只有一个 OSS 调用。需要决定：plist 是不是写两个 entry / 或者保持单 entry 推所有工具
- **Codex hook 真机验证**——本批的 TOML schema 是从 `codex-rs/hooks/src/schema.rs` 推的，需要在真 Codex 里跑一次确认 hook 真触发

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

[0.7.4]: https://github.com/echobyte26/sessync/releases/tag/v0.7.4
[0.7.3]: https://github.com/echobyte26/sessync/releases/tag/v0.7.3
[0.7.2]: https://github.com/echobyte26/sessync/releases/tag/v0.7.2
[0.7.1]: https://github.com/echobyte26/sessync/releases/tag/v0.7.1
[0.7.0]: https://github.com/echobyte26/sessync/releases/tag/v0.7.0
[0.6.2]: https://github.com/echobyte26/sessync/releases/tag/v0.6.2
[0.6.1]: https://github.com/echobyte26/sessync/releases/tag/v0.6.1
[0.6.0]: https://github.com/echobyte26/sessync/releases/tag/v0.6.0
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
