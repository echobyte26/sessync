# FAQ — 常见问题与解决办法

来自开发和实际使用中真碰到的问题。如果你撞到这里没列的，开个 issue，我们补进来。

## 安装 / 升级

### `error: A 'brew install sessync' process has already locked ...incomplete`

之前某次 `brew install`（被 Ctrl-C 杀掉、终端关掉）留下了过期的下载锁。清理：

```bash
pkill -9 -f brew 2>/dev/null
rm -f ~/Library/Caches/Homebrew/downloads/*sessync*
brew install sessync
```

### `brew install` 下载卡在某个字节数 30 秒以上

国内常见 —— github.com 的 release CDN 经常很慢。两个绕道：

```bash
# 1. 用代理
export ALL_PROXY=http://127.0.0.1:7890   # 你的代理端口
brew install sessync
unset ALL_PROXY

# 2. 用 ghproxy 镜像手动下载 + 让 brew 复用缓存
URL="https://github.com/echobyte26/sessync/releases/download/v0.2.0/sessync-v0.2.0-macos-universal.tar.gz"
URL_HASH=$(echo -n "$URL" | shasum -a 256 | awk '{print $1}')
CACHE_FILE="$HOME/Library/Caches/Homebrew/downloads/${URL_HASH}--sessync-v0.2.0-macos-universal.tar.gz"
curl -L "https://ghproxy.com/$URL" -o "$CACHE_FILE"
brew install sessync   # 命中缓存，跳过下载
```

### macOS Keychain 每次都弹密码窗（v0.1.x）

v0.2.0 已经彻底解决 —— 不再用 keychain。`brew upgrade sessync` 之后跑一次 `sessync init` 把 passphrase 迁移到新的 machine-bound 文件 `~/.config/sessync/passphrase.enc` 即可。

如果还在 v0.1.x：弹窗是 macOS 自带的反钓鱼机制。点 **始终允许**（不是"允许"），同一 binary 之后就不再弹。每次 `brew upgrade` 后 binary hash 变了，授权失效又会弹一次 —— **唯一的真解就是升级到 v0.2.0**。

### `~/.local/bin/sessync: Permission denied`

你大概在尝试装到 `/usr/local/bin/sessync`（要 sudo）。装到 `~/.local/bin/sessync` 即可（无需 sudo），v0.2.0 的 `sessync install` 自动会做这事。

## Init / 配置

### `sessync resume` 报 `Decryption failed`

最常见原因：**远程 bucket 上的 salt 跟本地 config 记的 salt 对不上**。两种情况：

1. **2026-08 之后第一台设备（v0.1.1+ 已修复）**—— salt 通过 OSS 的 `<prefix>.sessync-salt` 共享。重跑 `sessync init` 用同一个 passphrase 即可，新 init 会自动复用远程 salt。
2. **B1 修复前的双设备配置** —— 需要手动同步：
   ```bash
   # 在能正常工作的设备上：
   grep '^kdf_salt_hex' ~/.config/sessync/config.toml
   # → kdf_salt_hex = "ac5d815c9a..."
   
   # 在出问题的设备上替换：
   sed -i '' 's/^kdf_salt_hex = .*/kdf_salt_hex = "ac5d815c9a..."/' \
       ~/.config/sessync/config.toml
   ```

如果两边 passphrase 真不一样，没法恢复 —— 两边都重 init 用同一个 passphrase，从其中一台重新 push。

### `WARNING: passphrases under 12 characters are weak ...`

软警告，不是错误。输 `y` 继续即可。但还是建议改长 —— passphrase 是你 OSS 里所有 session 与 AK/SK 泄露之间的最后一道防线。

## Resume / push

### `sessync resume` 太慢（>5s）

v0.1.x 的冷启动正常就是 5-15s，因为 SDK 不复用 OSS 连接。v0.2.0 加了加密本地 meta 缓存 —— 升级后第一次仍然 5-15s（缓存为空），之后 ~1s。

如果 v0.2.0 还慢：

- 缓存可能被清掉了（`sessync uninstall` 会删它）。
- OSS 区域到你的网络慢 —— `time curl -I https://<你的bucket>.<endpoint>` 看下基线 RTT。
- 加 trace 看哪一步慢：`RUST_LOG=sessync=debug sessync resume`。

### 打印了 `Run: claude --resume <sid>` 但 `claude` 报 "No conversation found"

resume 把 session jsonl 落到**你跑 `sessync resume` 时所在的 cwd 编码出的目录**。`claude` 又会在**它自己的 cwd** 找对应的 project 目录。如果两条命令之间 `cd` 到了别的地方，就找不到。

定位 jsonl 实际位置：

```bash
find ~/.claude/projects -name '<session-id>.jsonl' 2>/dev/null
# /Users/.../.claude/projects/-Users-sakuragi-myproject/<session-id>.jsonl
#                              ^^^^^^^^^^^^^^^^^^^^^^^^^
#                              这一段是 cwd 用 `-` 编码的结果
```

把目录名反编码（`-` 换回 `/`），`cd` 过去再跑 `claude --resume`。

### Auto-push hook 跑了但失败我也不知道

Stop hook 在每次对话结束运行 `sessync push --quiet`，但输出被 Claude Code 吞掉了（你看不到）。要看具体错误：

```bash
# 错误不论 --quiet 与否都会写日志
RUST_LOG=sessync=info sessync push --quiet
# 等 D2 (sessync logs) 落地后可以：
sessync logs   # 暂未实现
```

D2 落地前，手动跑一次 `sessync push` 暴露错误。

## 卸载

### 想全部清空

```bash
sessync uninstall --purge-remote   # 同时清空 OSS bucket 前缀
brew uninstall sessync
brew untap echobyte26/sessync
```

不加 `--purge-remote` 的话，OSS 里你加密过的 blob 永远留着 —— 但没 passphrase 它们就是不可读的乱码，**留着安全但 stale**。

### `sessync uninstall` 卡在删自己 binary

如果 `sessync` 正在运行（比如 auto-push hook 此刻正好在执行），自删会失败。等几秒重试，或者：

```bash
sessync hook uninstall   # 先关掉 hook
sessync uninstall
```

## Hook / auto-push

### `sessync hook status` 报 "WARNING: 'sessync' was not found in PATH"

hook 命令在 Claude Code 继承的 PATH 里运行。如果 `~/.local/bin` 或 `/opt/homebrew/bin` 不在那个 PATH，hook 会静默失败。

```bash
# 看 Claude Code 的实际 PATH（在 Claude 里跑 `echo $PATH`）
# 确认 ~/.local/bin 或 /opt/homebrew/bin 在其中
# 如果不在，加到 ~/.zshrc：
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
# 或者 Homebrew：
echo 'export PATH="/opt/homebrew/bin:$PATH"' >> ~/.zshrc
```

然后重启 Claude Code（hook 下次对话才重读 PATH）。

### 临时关掉 auto-push

```bash
sessync hook uninstall   # 把 Stop hook 从 settings.json 移除
# ... 想干啥就干啥 ...
sessync hook install     # 装回来
```

## OSS

### 错误信息只有 `oss error` 没有 detail

v0.1.2 之前的问题 —— 错误格式用的是 `{e}` 而不是 `{e:?}`。升级到 v0.1.2+ 错误链路就完整可见。

### OSS 报 `403 Forbidden`

你的 AccessKey 在这个 bucket 没权限。去阿里云 RAM 控制台：

1. 找到你的子账号（应该叫 `sessync-bot` 之类）。
2. 检查 **权限管理** tab —— 必须包含 `AliyunOSSFullAccess` 或者一个 bucket 范围的策略，至少包含 `oss:GetObject` / `oss:PutObject` / `oss:DeleteObject` / `oss:ListObjects`。
3. 如果你最近重新生成了 AK/SK，重跑 `sessync init` 粘贴新的 AK/SK。

### 跨设备 push 冲突（数据丢失 —— 两台设备都活跃）

当前行为是 **last-writer-wins**：Mac A push 了 session X，Mac B 没先 `sessync resume` 就 push 同一个 X，B 会覆盖 A 在 OSS 上的版本。

v0.3.0 的 C1/C2 落地前的 workaround：在某台设备继续某个 session 之前，**先 `sessync resume` 拉一次最新版**，即使本地有副本。这样能拿到 OSS 上的最新版本，避免 stale 覆盖。

## 跨平台

### Windows / Linux 支持

v0.2.0 暂不支持。代码结构上准备好了（machine_id 已支持 Windows / Linux 路径，热路径无平台特定代码），但 binary 只发 mac 版。tracking 在 v2-backlog 的 V2-9 项。

## 开发

### `cargo test` 在 `keychain::tests::roundtrip` 失败

那个测试默认 `#[ignore]` —— 它会真碰 macOS Keychain。只有想测试 deprecated 的 keychain 路径时才显式跑：

```bash
cargo test --lib keychain::tests::roundtrip -- --ignored --nocapture
```

roundtrip 用的是 test-only service 名（`sessync-test-do-not-use`），不会污染你真的 sessync passphrase。

### 想看 push 会上传什么

`sessync push --dry-run` 在 v0.2.0 backlog（S2）。落地之前，自己看 `~/.claude/projects/` 里你最后一次 push 之后新增/改动过的 jsonl 文件。
