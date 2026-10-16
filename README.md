# syro

Repos, worktrees, one CLI agent per worktree, files, diffs, session history and
source control — in Rust + [GPUI](https://gpui-kit.com), without
Electron/Chromium/Node.

Everything for a worktree lives in one window: a terminal for its agent, the file
tree beside it, the diff, the git graph, and the agent session it belongs to.

- Worktrees for every repo, created and removed from the sidebar.
- Tabs and split panes holding a terminal, editor, diff or image.
- Built-in agent presets, with each worktree showing whether its agent is
  working, idle, or waiting for you.
- File tree, session history, source control panel and git graph.
- Quick open for files and worktrees.
- English and Chinese UI.

## Usage costs

The usage panel keeps recorded costs. Missing/zero GPT costs in Pi and Codex
transcripts are estimated at OpenAI Standard USD rates for `gpt-6.1-sol`,
`gpt-6-sol`, and `gpt-5.6-sol` (snapshot: 2026-10-16; the latter uses the current
promotional rate). Input, output, cache reads/writes and the >272K input
long-context premium are counted per request; reasoning is not counted twice.
`~` marks estimates, `Unknown` means no known price, and `+` marks a subtotal
that excludes unpriced usage. These are not subscription charges or relay bills.
Fast/Ultrafast, regional premiums and tool fees are excluded. Prices are a local
snapshot, not automatically refreshed. Restart the updated app to recalculate
history; transcript files are never modified. The existing 14-day window and
250-transcript scan limit still apply.

Sources: [OpenAI rates and long-context rules](https://help.openai.com/en/articles/20001415-chatgpt-rate-card-enterprise-token-based-pricing),
[API prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching).

## Run

```sh
cargo run -p syro     # the app
cargo test            # tests
./bundle.sh           # the macOS .app and DMG
```
