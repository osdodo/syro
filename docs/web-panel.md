# syro 网页端：状态 + 操控

手机浏览器打开一个页面，看每个 worktree 的 agent 状态和当前任务，并能批准
permission、回一句 prompt、删/建 worktree。

用普通 web 技术栈：手写 HTTP（`std::net::TcpListener`）+ 一个静态 HTML，没有
npm、没有构建链、没有 WASM。理由见文末「为什么不用 gpui-kit 的 WASM target」。

M1（看）、M2（操控）、M3（完整终端）都已实现；默认视图仍是纯文本 snapshot，
它不加载任何库，手机流量最小，「完整终端」是按需打开的增强。逐项的进度与证据
见下面「进度」。

## 进度

图例：✅ 已实现并有测试 ／ ⬜ 未做。

| | 状态 | 内容 | 证据 |
|---|---|---|---|
| M1 看 | ✅ | worktree 列表、agent 状态与是否卡住、当前任务、终端文本 snapshot | `web::tests::state_answers_json`、`window_tests::a_web_request_reaches_the_terminal_it_names` |
| M2 操控 | ✅ | 按键名 / 文本 / resize；建、删 worktree；列 presets；在 worktree 里起终端（含 agent） | `web::tests::a_terminal_reads_its_text_and_takes_the_keys`、`a_worktree_is_created_and_removed_by_name_and_id`、`the_agents_are_listed_and_one_can_start_a_terminal`、`window_tests::a_web_request_opens_a_terminal_in_the_worktree_it_names` |
| M3 完整终端 | ✅ | SSE 字节流 + xterm.js + addon-fit + 按手机屏幕 resize | `web::tests::a_stream_paints_the_screen_and_then_sends_the_bytes`、`the_full_terminal_library_is_served_from_the_binary` |
| 背压修复 | ✅ | 丢帧打 `lagged`，SSE 线程从 grid 补发 `paint` | `core::term::tests::a_subscriber_that_falls_behind_is_told_it_is`、`web::tests::a_reader_that_fell_behind_is_repainted_from_the_grid` |
| 鉴权与绑定 | ✅ | token 文件（0600）+ `?token=` → cookie；默认只绑 loopback | `web::tests::the_page_and_the_state_need_the_token`、`a_query_token_answers_once_and_becomes_a_cookie` |
| 主线程桥 | ✅ | accept 线程 → channel → `cx.spawn` 循环 → `Workspace::answer` | `window_tests::the_panel_loop_answers_what_its_channel_carries`、`the_panel_binds_a_port_and_keeps_its_url` |
| 面板内 URL 入口 | ✅ | 设置→通用里显示 URL 并可复制 | `window_tests::the_web_panel_url_is_shown_and_copied` |
| 页面语言 | ✅ | 服务端把 app 的语言写进页面 | `web::tests::the_page_is_served_in_the_apps_language` |
| 状态推送（省电） | ✅ | `/api/state/stream` 变化才发，不再 2s 轮询 | `web::tests::the_state_stream_sends_the_state_and_only_when_it_changes` |
| `[web]` 开关/端口表单 | ⬜ | 设置里改 bind/port/enabled | 未做（有意）：三者都是绑定时读一次，表单里的控件要到重启才生效，不如留在 `settings.toml` |
| 网页里编辑文件 / 看 diff | ⬜ | — | 非目标 |

跑起来只有一个开关和一个地址：

```sh
cargo run -p syro
# 日志里：syro: web panel on http://127.0.0.1:8777/?token=…
```

打开那条 URL 后 token 会变成 cookie，所以书签不带 token 也能用。要手机能连，把
`[web] bind` 改成 Tailscale 地址（见「配置」）。

验证（当前 `web-panel` 分支）：

```sh
cargo test                  # 328 + 119，逐层覆盖面板的全部路径
cargo fmt --all --check
cargo clippy --all-targets  # 面板相关文件 0 条警告
```

说明：328 个窗口测试全速并行时，在负载高的机器上偶发超时（例如
`the_appearance_setting_changes_the_theme`）；这些用例与面板无关，且
`cargo test -- --test-threads=4` 下稳定全绿。

## 非目标

- 不替代桌面端，不做移动 app。
- 不暴露公网，不做多用户/login。
- 不做前端构建链：`crates/app/src/web/index.html` 由 `include_str!` 编进二进制。
- 不做网页上的文件编辑 / diff。

## 架构

```
[手机浏览器] ──HTTP──▶ [web 线程: syro-web]
                            │ async_channel（Action）
                            │ std::sync::mpsc（Payload）
                            ▼
                      [GPUI 主线程: Workspace + AppState]
```

两条不能忘的约束：

1. **状态只在运行中的进程里。** agent 状态是 `AppState.statuses`
   （`crates/app/src/state.rs:262`），`state.json` 不存它。所以服务器是 app 进程
   里的一个线程，没有独立 daemon。
2. **终端不是 `Send`。** `TermSession` 挂在 `TerminalView` 上，是 `Rc`
   （`crates/app/src/pane/terminal.rs:137`）。写终端、读终端只能在主线程做，
   web 线程只能发 `WebRequest`。

### 没有终端注册表

请求到达时在主线程扫 `Workspace.panes`
（`crates/app/src/main.rs:267`，`BTreeMap<WorktreeId, BTreeMap<split::Pane, PaneView>>`）
就能找到所有 `TerminalView`（`Workspace::web_terminal_entity`）。不维护
`TerminalId → WeakEntity` 簿记表：少一层状态就少一处失联。

### 主线程桥

- `web::start` 起一个线程 accept，把 `WebRequest { action, reply }` 送进
  `async_channel`，然后阻塞在 `reply.recv_timeout` 上（15s）。
- `Workspace::serve_web` 用一个 `cx.spawn` 的循环 `inbox.recv().await`，每个请求
  交给 `Workspace::answer`，在主线程读状态 / 写终端。
- `answer` 一定回一条（`Result<Payload, String>`）；拿不到回答就是 HTTP 503。

## 接口

只读：

- `GET /` → 内嵌 HTML；app 当前的语言由服务端填进去（`page_for`），所以页面跟
  `settings.language` 一致，而不是看浏览器猜
- `GET /api/state` → `Snapshot` JSON（首次拉取与降级轮询用）
- `GET /api/state/stream` → **SSE**：变化时发 `event: state`（body 是单行 JSON），
  没变化就不发；每 30s 一个 ping。手机不再每 2s 被叫醒问一遍
- `GET /api/agents` → `[{"id":"claude","name":"Claude Code"}]`，页面的启动选择器用
  （只有 id 和名字；command/args/env 不外泄）
- `GET /api/terminal/{id}/snapshot` → `text/plain`，终端的可见文本
- `GET /api/terminal/{id}/stream` → **SSE**：先 `event: paint`（当前屏幕），
  再不断 `event: bytes`，终端结束 `event: end`。payload 是十六进制编码的原始字节。
- `GET /xterm.js`、`GET /xterm-addon-fit.js`、`GET /xterm.css` → 内置的 xterm.js
  （只在页面要完整终端时才去取）

动作（POST，JSON body，全部经主线程）：

- `POST /api/terminal/{id}/input` `{"text":"继续"}` 或 `{"keys":["enter","y"]}`
- `POST /api/terminal/{id}/resize` `{"cols":100,"rows":40}`
- `POST /api/terminals` `{"worktree":"<id>","agent":"claude"?}` —— 在该 worktree
  开一个终端 tab（不给 `agent` 就是普通 shell）；会先切到那个 worktree，
  和会话历史面板 resume 的行为一致
- `POST /api/worktrees` `{"repo":"syro","name":"fix-build","base":"origin/main"?}`
  —— `repo` 可以是 id 或名字
- `POST /api/worktrees/remove` `{"id":"/w/fix-build","force":false?}`

用 `keys` 而不是裸字节：页面不认识 agent 的“同意”是什么，用户按名字、app 发字节。
`api::key_bytes` 认得 `enter`、`esc`、`tab`、`backspace`、`space`、方向键、
`ctrl-c`、`ctrl-d`，以及任意单个可打印字符。有一个认不出就整个请求拒绝，不做半发送。

用 POST + JSON 而不是把 worktree id 放进 URL：worktree id 是绝对路径，
URL path 里放不下，query 又要手写 percent-decode，不值得。

## 数据模型

DTO 在 `crates/core/src/api.rs`，纯 serde + 组装测试，不碰 gpui：

```json
{
  "worktrees": [
    {
      "id": "/Users/me/syro/workspaces/syro/fix-build",
      "repo": "syro",
      "name": "fix-build",
      "branch": "fix-build",
      "status": "working",
      "stalled": false,
      "terminals": [
        {
          "id": "terminal-7",
          "agent": "claude",
          "agent_name": "Claude Code",
          "status": "working",
          "stalled": false,
          "task": "Fix cursor offsets",
          "resume": "claude --dangerously-skip-permissions --resume 0f3a-...",
          "cols": 120,
          "rows": 32
        }
      ]
    }
  ],
  "active": "/Users/me/syro/workspaces/syro/fix-build"
}
```

- `status`：`"working" | "idle" | "permission" | null`（`AgentStatus` 已 `Ord`，
  worktree 上是最紧急的那个）。
- `task`：标题里的任务名（`agent::task_of`）。
- `resume`：`api::resume_command` 拼的完整命令（preset 的 command + args +
  展开的 `resume_args`），给手机复制去 SSH 用。拿不到 id/path 就是 `null`。

## 配置

`~/.config/syro/settings.toml`：

```toml
[web]
enabled = true      # 默认 true
bind = "127.0.0.1"  # 默认回环
port = 8777
```

要手机能连：`bind` 改成 Tailscale 地址（`100.x.y.z`），然后用
`http://100.x.y.z:8777/?token=...`。app 启动时会把完整 URL 打进日志。

## 安全

这一层能执行任意命令，等同 SSH 私钥。

- **默认只绑 `127.0.0.1`。** 绝不 `0.0.0.0`。
- **token**：`~/.config/syro/web-token`（0600），没有就生成。第一次访问带
  `?token=...`，服务端回 `Set-Cookie: syro_token=...; HttpOnly; SameSite=Strict`，
  之后靠 cookie。也接受 `Authorization: Bearer`。
- 错误的 token 一律 401，且**不**把错 token 当 cookie 发回去。
- 响应带 `Cache-Control: no-store` 和 `X-Content-Type-Options: nosniff`。
- 传输加密交给 Tailscale。明文 HTTP 只跑在 loopback。

## 文件清单

- `crates/core/src/term.rs`：`TermSession::subscribe()`，reader 线程把同一次读
  也转发给订阅者（`forward`）。
- `crates/core/src/api.rs`（新）：DTO（`Snapshot`、`AgentOption`）、`resume_command`、
  `key_bytes`、`keys_to_bytes`。
- `crates/app/src/web.rs`（新）：accept 线程、路由、鉴权、token、内嵌页面、
  SSE 流（`serve_stream`）、hex。
- `crates/app/src/web/index.html`（新）：一页，2s 轮询 `/api/state`；打开某个终端后
  1.2s 轮询它的 snapshot；按键按钮 + 文本框 + 复制 resume 命令；建/删 worktree；
  选 worktree + agent 启一个终端；「完整终端」按需加载 xterm.js。
- `crates/app/src/web/{xterm.js,xterm.css,xterm-addon-fit.js}`（新，vendored）。
- `crates/app/src/main.rs`：`Workspace::start_web` / `serve_web` / `answer` /
  `run_web` / `web_snapshot` / `web_terminal_entity` / `web_repo_id`，以及 `main()`
  里的启动调用。
- `crates/app/src/pane/terminal.rs`：`TerminalView` 的只读/写入接口
  （`snapshot_text`、`write_bytes`、`term_size`、`resize_term`、`agent_status`、
  `is_stalled`、`task`、`resume_command`）。
- `crates/core/src/config.rs`：`WebSettings`。
- `crates/app/Cargo.toml`：`async-channel`（gpui 已在依赖树里，不新增下载）、
  `serde_json`。

## 测试

- `crates/core/src/term.rs`：订阅者拿到和 grid 一样的字节；丢掉的订阅者不被等待；
  队列满时被标记 `lagged`、且标记只会被取走一次（不会重复重绘）。
- `crates/core/src/api.rs`：DTO JSON 形状、resume 命令拼装、按键映射。
- `crates/app/src/web.rs`：真 `TcpListener` + 真 socket 打请求（token 的对/错、
  query→cookie、路由、坏 body、404、agents 列表、启终端、静态资源、hex 往返），
  accept 循环里用一个假主线程应答；两个流测试用**真 PTY**：屏幕 → 字节事件，
  以及落后后的补绘。
- `crates/app/src/main.rs`（`window_tests`）：
  - `the_panel_binds_a_port_and_keeps_its_url`：`port = 0` 绑真端口、URL 里的 token
    与 token 文件一致、`enabled = false` 不绑。
  - `the_panel_loop_answers_what_its_channel_carries`：`cx.spawn` 那个循环确实被
    poll 并回答。
  - `a_web_request_reaches_the_terminal_it_names`：请求经桥进真终端，shell 回显；
    未知 terminal id 被拒；`Action::Stream` 给出的屏幕就是刚敲的那行。
  - `a_web_request_opens_a_terminal_in_the_worktree_it_names`：启终端真的多一个 tab；
    未知 worktree / agent 被拒且不留 tab。

不写「真 socket + 真线程」的 gpui 测试：gpui 的 test scheduler 会把外部线程的
wake 判为 non-determinism 而 panic。socket 层在 `web::tests`（无 gpui）覆盖，
gpui 侧只驱动 channel。

## 风险 / 已知边界

- **同一终端两端同时写。** 桌面端和网页端都能发按键，没有任何锁，谁最后写谁赢。
  手机和桌面同时敲同一个终端会交错 —— 和 tmux 挂两个 client 一样，是自己干的。
- **写是同步的。** `TermSession::write` 在主线程上写 PTY；子进程不读、PTY 缓冲满时
  会堵住 UI。桌面端的键入/粘贴本来就是这样，面板把这个面放大了（body 上限 64KB）。
- **丢帧。** 订阅队列满（64 帧）就丢，但丢帧会被记录，SSE 线程从 grid 补发一个
  `paint` 把画面拉回来（至多 10 秒内，即下一次 ping）。
- **一个进程一个面板。** 只有 `main()` 建的第一个窗口会 `start_web`；再开窗不会
  绑第二个端口，所以第二个窗口的状态不在面板里。
- **面板挂了不影响 app。** 服务器线程或任务结束，请求变 503，界面照常。
- **批准 permission 不猜按键。** 只暴露“发送我打的字 / 按这个键名”，不做一键 approve
  的魔法按钮；真要做得按 `AgentSpec` 配键位。
- **SSE 断了会自己重连**（`EventSource` 的行为），重连后重新 paint + 重订阅，
  所以手机切网络不用管。只有 `event: end` 会主动关掉它。

## 完整终端（M3）

页面上的「完整终端」按钮把它切成 xterm.js 视图：颜色、TUI、滚动、鼠标都有；键盘是
xterm 自己的隐藏 input（手机上就是系统输入法）。默认仍是纯文本 snapshot，
xterm.js 只在按下那个按钮时才下载。

**为什么用 SSE 而不是 WebSocket。** 上行已经是 POST（`/api/terminal/{id}/input`），
只有下行是流，而浏览器自带 `EventSource`。SSE 不需要握手、不需要帧格式、不需要
新依赖；`tungstenite` 那一步省了。

**为什么 payload 是十六进制。** SSE 是按行文本，不能带裸 `\n`；而终端输出是
UTF-8 字节流，一个读块可能把一个字符切两半。十六进制是唯一“不用在这一层操心
UTF-8、页面能直接交给 xterm.js 当字节用”的编码（xterm.js 自己缓冲 UTF-8）。
开销是 2 倍，换来不引 base64 依赖、不破坏 CJK。

**屏幕与流的拼接。** `TerminalView::stream()` 先 `subscribe()` 再读 snapshot，
所以订阅里已经攒下的，正是 snapshot 已经包含的——服务端先丢掉它们再发 paint，
不会用旧字节覆盖新屏幕。两者之间的一瞬间（最多一帧）会丢，TUI 会重画。

**背压与修复。** 每个订阅者一个 64 帧的 `sync_channel`；满了就丢帧（不会卡住 PTY
读线程），断开就自动从列表里移除（`try_send` 返回 `Disconnected`）。丢帧的订阅者会
被标上 `lagged`：它错过了字节、不可能知道是哪一段，屏幕从那一刻起就是错的。
SSE 线程发现这个标记后，向主线程要一份新的 snapshot 并**补发一个 `paint`**——
页面本来就把 `paint` 当重绘处理器，所以不用多余的协议：错屏最多存在到下一次
重绘（至多 10 秒，也就是下一个 ping 之前）。

**两端一致的字节。** `TermSession::subscribe()` 拿到的就是喂给 alacritty grid 的
同一份字节（reader 线程 `forward` 转发），所以手机上的终端和桌面上的 grid 不会对
子进程说了什么产生分歧。

**vendored 的三方文件**（`crates/app/src/web/`，`include_str!` 进二进制）：

| 文件 | 大小 | 来源 |
|---|---|---|
| `xterm.js` | 291 KiB | `@xterm/xterm@5.5.0/lib/xterm.js` |
| `xterm-addon-fit.js` | 1.5 KiB | `@xterm/addon-fit@0.10.0/lib/addon-fit.js` |
| `xterm.css` | 5.5 KiB | `@xterm/xterm@5.5.0/css/xterm.css` |

MIT；完整 license 文本在 `xterm.js` 和 `xterm.css` 的头注释里。升级就是重新下载
这几个 URL（版本号改掉），不需要构建链。

## 为什么不用 gpui-kit 的 WASM target
`gpui-kit` 有 `wasm32-unknown-unknown` target（`gpui_kit::web`），但 gpui-kit 自己
把它标为 `showcase-only`，并警告未验证「完整应用」的 web 分发：只能一个 canvas /
一个顶层窗口、无字体库（CJK 要自己打 subset）、键盘走隐藏 input 且对触屏设备
禁用以避免弹键盘、WASM 里没有 PTY/git/fs。

对「手机看状态 + 操控」这个需求，用 WASM 会**既付 WASM 的代价（体积、首屏、
字体、单窗口、nightly 工具链），又照样写 native RPC**，换来一个没被验证过的
手机输入。所以用普通 HTML。只读仪表盘、且几乎不打字时，WASM 才值得考虑。
