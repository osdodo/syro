<div align="center">
  <img src="assets/logo.png" width="110" alt="Syro logo">
  <h1>Syro</h1>
  <p><strong>A native desktop workspace for running CLI coding agents across git worktrees.</strong></p>
  <p>
    <a href="https://github.com/osdodo/syro/releases/latest"><img src="https://img.shields.io/github/v/release/osdodo/syro?label=release" alt="Latest release"></a>
    <img src="https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey" alt="Platform: macOS 15+">
    <img src="https://img.shields.io/badge/built%20with-Rust-orange" alt="Built with Rust">
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="License: MIT"></a>
  </p>
  <p align="center">English | <a href="README.zh-CN.md">简体中文</a></p>
</div>

![Syro demo](assets/syro-demo.gif)

Syro gives every git worktree its own agent, terminal, files, diff and history, side
by side in one window. Run Claude Code, Codex, Gemini and other CLI agents in
parallel on isolated branches, and see at a glance which one is working, idle or
waiting for you.

Built in Rust on [GPUI](https://gpui-kit.com). No Electron, Chromium or Node.

## Features

| | |
|---|---|
| **Worktree-first** | Create and remove worktrees for any repo from the sidebar. Each one is an isolated workspace for one agent. |
| **Agent status** | Built-in, configurable presets for Claude Code, Codex, Gemini, OpenCode, Qoder and pi report whether each agent is working, idle or waiting for input. |
| **Tabs and split panes** | Mix terminals, a syntax-highlighted editor, diffs and images in any layout. Tabs are restored on the next launch. |
| **Source control** | Changes panel, git graph, and AI-written commit messages through your own agent command. |
| **Session history** | Browse and resume the agent sessions that belong to a worktree. |
| **Resource and usage tracking** | CPU and memory per app and per worktree, plus a daily token-usage and cost chart by model. |
| **Hooks** | Run scripts on worktree events, globally or per repo. |
| **Web panel** | Check agent status, send keys, or open a full terminal from your phone. Terminals and worktrees can be started or removed from the panel too. |
| **Quick open** | Jump to any file or worktree. |
| **Keep awake** | Optionally keep the Mac awake while agents are running. |

## Installation

### Homebrew

```sh
brew install --cask osdodo/syro/syro
```

### Direct download

Download the `.dmg` for your Mac (`arm64` for Apple Silicon, `x86_64` for Intel)
from the [latest release](https://github.com/osdodo/syro/releases/latest) and drag
**syro** into **Applications**.

> [!NOTE]
> Syro is not notarized yet. If macOS refuses to open it on first launch, run:
>
> ```sh
> xattr -dr com.apple.quarantine /Applications/syro.app
> ```

### Requirements

- macOS 15 (Sequoia) or later
- `git`
- The CLI agents you want to run (`claude`, `codex`, `gemini`, ...) on your `PATH`

## Configuration

Settings and state are stored in `~/.config/syro/`.

### Hooks

Run a script whenever a worktree is created or removed by adding a `syro.yaml`
to the root of a repo:

```yaml
scripts:
  setup: |
    pnpm install
  archive: |
    rm -rf node_modules
```

`setup` runs on `worktree_created` and `archive` on `worktree_removed`, inside the
worktree directory. Hooks for any event can also be defined globally in
`~/.config/syro/settings.toml`:

```toml
[[hook]]
on = "agent_status_changed"
run = "echo \"$SYRO_WORKSPACE_NAME: $SYRO_AGENT_ID\" >> ~/syro.log"
```

Hooks receive `SYRO_WORKTREE_PATH`, `SYRO_WORKSPACE_NAME`, and, depending on the
event, `SYRO_AGENT_ID` or `SYRO_ROOT_PATH`.

## Building from source

Requires a recent stable [Rust](https://rustup.rs) toolchain (edition 2024).

```sh
git clone https://github.com/osdodo/syro.git
cd syro
cargo run -p syro     # run the app
cargo test            # run the tests
```

To build the macOS `.app` and `.dmg`:

```sh
cargo install cargo-bundle
./bundle.sh           # output: target/release/bundle/osx/
```

## License

[MIT](LICENSE)
