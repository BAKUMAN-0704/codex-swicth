# codex-switch

在 Codex 的 **ChatGPT 账号登录** 与 **API Key / 第三方供应商** 之间一键切换，并且切换后**历史对话照常可见**。

```text
 codex-switch 0.2.1   relay  ·  仅 API Key（sk-…）                               ● Codex 未运行
────────────────────────────────────────────────────────────────────────────────────────────────
╭ profiles ──────────────────────╮╭ 详情 ──────────────────────────────────────────────────────╮
│  ○ official     ChatGPT  需登录││ id          relay                                          │
│▌ ● relay        API Key  key ✓ ││ 认证        API Key                                        │
│                                ││ base_url    https://relay.example.com/v1                   │
│                                ││ 历史标记    relay                                          │
╰────────────────────────────────╯╰────────────────────────────────────────────────────────────╯
╭ 日志 ────────────────────────────────────────────────────────────────────────────────────────╮
│ ✓ config.toml 已更新                                                                         │
╰──────────────────────────────────────────────────────────────────────────────────────────────╯
 ↑↓ 选择   Enter 切换   e 设 Key   a 新增   h 迁移历史   v 校验   b 备份   ? 帮助   q 退出
```

## 它解决什么问题

Codex 按 `model_provider` 给会话分桶。你从中转站切回官方登录后，之前所有会话都"消失"了 —— 它们没被删除，只是被过滤掉了。而且列表并非只读文件：

| 位置 | 作用 |
| --- | --- |
| `sessions/**/rollout-*.jsonl` 首行 `session_meta.model_provider` | 会话本体标记 |
| `state_*.sqlite` → `threads.model_provider` | 会话列表真正的数据源 |
| `thread_history_*.sqlite` → `thread_turns.rollout_byte_offset` | 每一轮对话在文件里的字节偏移 |

只改第一处（网上多数脚本的做法）在较新版本上不够用；而改动首行会改变文件长度 —— `relay` → `openai` 多 1 字节，后面所有偏移就全部错位，历史内容会读成乱码。

这个工具把三处一起改：改写首行时**按实际字节差精确平移偏移量**，所以反复切换不会累积误差。

## 安装

从 [Releases](https://github.com/BAKUMAN-0704/codex-swicth/releases) 下载 `codex-switch.exe`（单文件，无运行时依赖），或自行编译：

```bash
cargo build --release
# target/release/codex-switch(.exe)
```

## 使用

直接运行（不带参数）打开 TUI：

```bash
codex-switch
```

| 按键 | 作用 |
| --- | --- |
| `↑` `↓` / `j` `k` | 选择 profile |
| `g` / `G` | 跳到列表首 / 尾 |
| `Enter` | 切换（会先列出将要写入的每一项） |
| `e` | 设置选中 profile 的 API Key |
| `a` | 新增 profile |
| `h` | 迁移历史会话到选中 profile 的 provider |
| `v` | 校验会话偏移是否仍然对齐 |
| `b` | 手动备份 |
| `u` | 回滚上一次历史迁移 |
| `R` | 从最近备份还原 config/auth/sqlite |
| `r` | 重新读取配置与进程状态 |
| `?` | 打开 / 关闭按键说明 |
| `q` / `Esc` | 退出 |

也完全可以用命令行（适合脚本和 CI）：

```bash
codex-switch list                 # 列出所有 profile
codex-switch status               # 当前 provider / 凭据 / 进程状态
codex-switch use official         # 切到 ChatGPT 官方登录
codex-switch use relay            # 切回中转
codex-switch use official -n      # 只演练，不写任何文件
codex-switch history relay        # 只迁移历史会话，不动凭据
codex-switch verify               # 校验偏移
codex-switch rollback             # 撤销上一次历史迁移（字节级还原）
codex-switch add myrelay --base-url https://api.example.com/v1 --model gpt-5
codex-switch doctor               # 环境自检
```

没有终端（或被管道调用）时 TUI 不会启动，可用 `codex-switch menu` 走纯文本菜单。

## 字体

"终端字体"在 Windows 上其实是两回事，改的地方也不同：

| 你在哪看到这个界面 | 字体归谁管 | 怎么改 |
| --- | --- | --- |
| Windows Terminal 的标签页 | Windows Terminal | `codex-switch font --face "<名字>"`（改它的 `settings.json`） |
| **双击 exe 打开的控制台窗口** | 我们自己的进程 | `codex-switch font --console --face "<名字>"`（记到 `window.toml`，启动时调 `SetCurrentConsoleFontEx`） |

```bash
codex-switch font                               # 标签页字体 + 配置文件位置
codex-switch font --list                        # 系统已安装的字体
codex-switch font --face "Cascadia Mono"        # 改 Windows Terminal 标签页
codex-switch font --console                     # 看本窗口现在用什么字体
codex-switch font --console --face "Sarasa Term SC Nerd" --size 18   # 改本 exe 的窗口
codex-switch font --restore                     # 还原上一次对 settings.json 的修改
```

`--console` 改的是**当前这个控制台窗口**，同时写进 `~/.codex-switch/window.toml`，所以下次双击 exe 还是这个字体。它会读回校验：如果窗口其实由宿主程序绘制（Windows Terminal、ConPTY），conhost 会接受请求但不生效，这时命令会直接告诉你"窗口没跟着变"，不会假装成功。

`settings.json` 是带注释的 JSONC。整份解析再序列化会把注释和缩进全删掉，所以这里只定位并替换 `profiles.defaults.font.face` 的那一个字符串字面量，其余字节原样保留；写前备份到 `~/.codex-switch/backups/wt-settings-*.json`，写完还会回读校验，对不上就不落盘。

**字体要挑等宽的**：TUI 按固定字符网格排版，比例字体（比如 HarmonyOS Sans SC）会让表格列和边框对不齐，列表右侧的状态标记会飘。只有中文等宽的需求，装 `Sarasa Term SC`（更纱黑体，一个 `.ttc` 装完全部字重）或 `Maple Mono NF CN`；不想装就直接用系统自带的 `Cascadia Mono`，中文回退到 Noto Sans SC / 微软雅黑，仍然占满两格，网格不会散。

**名字要对**：`.ttc` 在注册表里只登记本地化名（例如 `更纱终端书呆黑体-简`），而终端要的是字体内部的英文族名 `Sarasa Term SC Nerd`。所以"是否已安装"不能只查注册表 —— 注册表没有时还会打开字体文件读 `name` 表，两种名字都认得；`--list` 打出来的是注册表名，可以对照着用。

## 每次切换具体改了什么

| 文件 | 改动 |
| --- | --- |
| `config.toml` | 只改 `model_provider` / `preferred_auth_method` / `model` / `disable_response_storage` 和目标 provider 表；注释、`[projects]`、`[mcp_servers]` 与其他 provider 表原样保留 |
| `auth.json` | 切官方时把 `OPENAI_API_KEY` 置 null（否则可能继续按 key 计费）；切 API Key 时写入 key |
| `~/.codex-switch/stash/` | 覆盖前把当前登录态另存一份，下次切回官方直接复用，不用重新走浏览器 |
| 会话文件 | 首行 `model_provider` 改写，其余字节原样 |

第三方 key 写进 `[model_providers.<id>].experimental_bearer_token` 并令 `requires_openai_auth = false`，**不会**污染 auth.json 里的官方登录缓存。

## 安全设计

- **写前快照**：每次切换把 `auth.json` / `config.toml` / 两个 sqlite 复制一份（只保留最近 3 份，迁移记录不受限制）
- **原子写入**：临时文件 + rename，失败不会留下半截文件
- **可回滚**：迁移记录里保存每个文件**原始首行字节**，回滚是字节级还原，不是重新序列化
- **幂等**：已经在目标 provider 下的会话会被跳过，重复执行无副作用
- **进程检查**：检测到 Codex 主进程在跑会拒绝写入（配置只在启动时读一次，运行中写会被覆盖）

## 须知

- 切换后要**完全退出并重新打开 Codex**，配置只在启动时读取。
- 切到官方登录后如果 `auth.json` 里没有 OAuth 凭据，需要自己跑一次 `codex login`。
- `threads` 表里还有 `creator_account_id`。在**同一台机器登录不同的 ChatGPT 账号**时，那是 Codex 自己的第二层隔离，本工具不处理。
- 只处理已经入库的会话；不在索引里的 `.zst` 归档会原样跳过。
- 该工具直接读写 Codex 的本地状态，属于非官方手段。升级 Codex 后若 schema 变化可能失效 —— 先用 `codex-switch use <id> -n` 演练，确认无误再执行。

## 开发

```bash
cargo test --release        # TUI 用 TestBackend 做离屏渲染测试，不需要真终端
cargo fmt
bash scripts/build-release.sh   # 产出的 exe 不含构建机绝对路径
```

代码结构：`switch.rs` 是唯一执行写入的路径（CLI 与 TUI 共用），`history.rs` 负责会话迁移与偏移修正，`config_edit.rs` 用 `toml_edit` 做保序改写。

## 致谢

profile 与凭据分离存储的思路参考了 [CC Switch](https://github.com/farion1231/cc-switch)。

## License

MIT
