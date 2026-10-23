<div align="center">
  <img src="assets/logo.png" width="110" alt="Syro logo">
  <h1>Syro</h1>
  <p>A native desktop workspace for running CLI coding agents across git worktrees.</p>
</div>

Syro gives every worktree its own agent, terminal, files, diff and history in one
window. Built in Rust on [GPUI](https://gpui-kit.com) — no Electron, Chromium or Node.

## Features

- **Worktree-first** — create and remove worktrees for any repo from the sidebar;
  each one is an isolated workspace for one agent.
- **Agent status at a glance** — built-in presets for Claude Code, Codex, Gemini,
  OpenCode, Qoder and pi show whether each agent is working, idle or waiting for you.
  Presets are configurable.
- **Tabs and split panes** — mix terminals, a syntax-highlighted editor, diffs and
  images in any layout; tabs are restored on the next launch.
- **Source control** — changes panel, git graph, and AI-written commit messages via
  your own agent command.
- **Session history** — browse and resume the agent sessions belonging to a worktree.
- **Resource and usage tracking** — CPU/memory per app and per worktree, plus a
  daily token-usage chart by model.
- **Hooks** — run scripts on worktree events, configured globally or per repo via
  `syro.yaml` (`scripts.setup` / `scripts.archive`).
- **Quick open** for files and worktrees, optional keep-awake while agents run, and
  English / Chinese UI.

## Build

```sh
cargo run -p syro     # run the app
cargo test            # run tests
./bundle.sh           # build the macOS .app and DMG
```
