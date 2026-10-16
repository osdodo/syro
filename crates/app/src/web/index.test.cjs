// Run with: node --test crates/app/src/web/index.test.cjs
const { test } = require("node:test");
const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { runInNewContext } = require("node:vm");
const tick = () => new Promise((resolve) => setImmediate(resolve));
const html = readFileSync(`${__dirname}/index.html`, "utf8");

function panel(locale = "en", options = {}) {
  const pendingScripts = [], streams = [], instances = [], listeners = {};
  class Element {
    constructor(tag = "div") {
      this.tag = tag;
      this.children = [];
      this.dataset = {};
      this.style = {};
      this.value = "";
      this.isConnected = true;
      const classes = new Set();
      this.classList = { add: (name) => classes.add(name), remove: (name) => classes.delete(name), contains: (name) => classes.has(name) };
    }
    set textContent(text) { this.children = [String(text)]; this.value = ""; }
    get textContent() { return this.children.map((child) => typeof child === "string" ? child : child.textContent).join(""); }
    append(...children) {
      this.children.push(...children);
      for (const child of children) if (child.tag === "script") {
        if (options.deferScripts) pendingScripts.push(child);
        else queueMicrotask(() => child.onload());
      }
    }
    setAttribute(name, value) { this[name] = value; }
    addEventListener(name, callback) { this[name] = callback; }
    focus() {}
    showModal() { this.open = true; }
    close() { this.open = false; }
  }
  const elements = Object.fromEntries([...html.matchAll(/<[^>]+\bid="([^"]+)"[^>]*>/g)].map(([tag, id]) => {
    const element = new Element();
    element.hidden = /\bhidden\b/.test(tag);
    return [id, element];
  }));
  class EventSource {
    constructor(path) { this.path = path; streams.push(this); }
    addEventListener(name, callback) { this[name] = callback; }
    close() { this.closed = true; }
  }
  class Terminal {
    constructor() { this.cols = 80; this.rows = 24; instances.push(this); }
    loadAddon() {}
    open() {}
    onData() {}
    focus() {}
    dispose() { this.disposed = true; }
  }
  const context = {
    document: {
      documentElement: {}, head: new Element(), body: new Element(),
      getElementById: (id) => elements[id] || null,
      createElement: (tag) => new Element(tag),
    },
    window: {
      EventSource, Terminal, FitAddon: class { fit() {} },
      addEventListener(name, callback) { listeners[name] = callback; },
    }, EventSource,
    history: { state: null, pushState(state) { this.state = state; }, back() { this.state = null; listeners.popstate({ state: null }); } },
    navigator: { language: locale },
    fetch: options.fetch || (async () => ({ ok: true, text: async () => "[]" })),
    setInterval() {}, setTimeout() {},
  };
  const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
  runInNewContext(script.replace('"__syro_locale__"', JSON.stringify(locale)), context);
  return { ...context, elements, stream: streams[0], streams, instances, pendingScripts, listeners };
}

const terminal = (id, status, stalled = false) => ({ id, status, stalled, agent_name: "Claude Code", task: "Fix login" });
const worktree = (id, terminals = []) => ({ id, repo: "syro", name: id, branch: id, terminals });

test("overview counts running and attention terminals without double counting stalled approvals", () => {
  const { applyState, elements } = panel();
  applyState({ worktrees: [worktree("one", [terminal("a", "working"), terminal("b", "permission", true)]), worktree("two", [terminal("c", "working", true), terminal("d", "idle")])] });
  assert.equal(elements["stat-worktrees"].textContent, "2");
  assert.equal(elements["stat-working"].textContent, "1");
  assert.equal(elements["stat-attention"].textContent, "2");
  assert.match(elements.list.textContent, /May be stalled/);
  assert.match(elements.list.textContent, /Idle/);
});

test("removing the last worktree clears counters, repository suggestions and terminal picker", () => {
  const { applyState, elements } = panel("zh-CN");
  applyState({ worktrees: [worktree("one")] });
  elements["term-worktree"].value = "one";
  applyState({ worktrees: [] });
  assert.equal(elements["stat-worktrees"].textContent, "0");
  assert.equal(elements["stat-attention"].textContent, "0");
  assert.equal(elements.repos.textContent, "");
  assert.equal(elements["term-worktree"].value, "");
  assert.match(elements.list.textContent, /还没有工作树/);
  assert.equal(elements["page-title"].textContent, "工作区总览");
});

test("state updates preserve the worktree choice and restore connection status", () => {
  const { applyState, elements, stream } = panel();
  const state = { worktrees: [worktree("one"), worktree("two")] };
  applyState(state);
  elements["term-worktree"].value = "two";
  stream.onerror();
  assert.equal(elements.hint.dataset.state, "offline");
  stream.state({ data: JSON.stringify(state) });
  assert.equal(elements.hint.dataset.state, "live");
  assert.equal(elements["term-worktree"].value, "two");
  assert.match(elements.list.textContent, /No terminals/);
});

test("opening a session starts the interactive terminal; browser Back disposes it", async () => {
  const p = panel();
  const t = terminal("a", "working");
  p.applyState({ worktrees: [worktree("one", [t])] });
  p.openTerminalView(t);
  await tick();
  assert.equal(p.elements.terminal.open, true);
  assert.equal(p.document.body.classList.contains("session-open"), true);
  assert.equal(p.instances.length, 1);
  assert.equal(p.streams[1].path, "/api/terminal/a/stream");
  assert.equal(p.elements["t-name"].textContent, "Claude Code · 01");
  p.listeners.popstate({ state: null });
  assert.equal(p.elements.terminal.open, false);
  assert.equal(p.document.body.classList.contains("session-open"), false);
  assert.equal(p.instances[0].disposed, true);
  assert.equal(p.streams[1].closed, true);
});

test("late library loads only attach to the latest open session", async () => {
  const p = panel("en", { deferScripts: true });
  const a = terminal("a", "idle"), b = terminal("b", "working");
  p.applyState({ worktrees: [worktree("one", [a, b])] });
  p.openTerminalView(a);
  p.requestCloseTerminalView();
  p.openTerminalView(b);
  p.pendingScripts[0].onload();
  await tick();
  p.pendingScripts[1].onload();
  await tick();
  assert.equal(p.instances.length, 1);
  assert.equal(p.streams[1].path, "/api/terminal/b/stream");
  assert.equal(p.elements["t-name"].textContent, "Claude Code · 02");
  p.applyState({ worktrees: [] });
  assert.equal(p.elements.terminal.open, false);
  assert.equal(p.instances[0].disposed, true);
});

test("a library load failure falls back to the full-screen text view", async () => {
  const p = panel("en", { deferScripts: true });
  const t = terminal("a", "working");
  p.applyState({ worktrees: [worktree("one", [t])] });
  p.openTerminalView(t);
  p.pendingScripts[0].onerror();
  await tick();
  assert.equal(p.elements.terminal.open, true);
  assert.equal(p.elements["t-full"].hidden, true);
  assert.equal(p.elements["t-simple"].hidden, false);
  assert.match(p.elements["t-error"].textContent, /could not load/);
  assert.equal(p.elements["t-text"].textContent, "[]");
});

test("a failed text send keeps the input and displays its error in the session", async () => {
  const p = panel("en", { fetch: async (_path, options) => ({ ok: !options?.method, text: async () => options?.method ? "PTY unavailable" : "[]" }) });
  const t = terminal("a", "working");
  p.applyState({ worktrees: [worktree("one", [t])] });
  p.openTerminalView(t);
  await tick();
  p.elements["t-input"].value = "keep this text";
  await p.elements["t-send-form"].onsubmit({ preventDefault() {} });
  assert.equal(p.elements["t-input"].value, "keep this text");
  assert.equal(p.elements["t-send"].disabled, false);
  assert.match(p.elements["t-error"].textContent, /PTY unavailable/);
  assert.equal(p.elements["t-error"].hidden, false);
});
