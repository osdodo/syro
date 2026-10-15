# Packaging

## Build

```
packaging/bundle.sh
open target/release/bundle/osx/syro.dmg
```

One command from a clean checkout: the script checks the two copies of the
bundle identifier against each other, runs `cargo bundle --release` (which
builds the release binary if it is not built yet), checks the `Info.plist`
macOS will actually read, and then builds the drag-to-install DMG. Re-run from a
clean checkout on 2026-10-12: the guard passed, the bundle was built, and the
plist read back `com.syro.app` — the same string `main.rs` hands to
`set_app_identity`.

## The DMG

The DMG is `hdiutil` and `sips`, both in the base system, plus one run of
Finder: the window layout (background picture, icon size and icon positions) is
stored in `.DS_Store`, and only Finder writes that file. So the build mounts a
read-write image at `/Volumes/syro`, drives Finder over AppleScript, waits for
Finder to flush, and only then compresses to `UDZO`. Two things that cost time
to find out:

- Finder refuses a *hidden file* as the window background (`-1700`), so the
  picture lives at `.background/background.png` — a normal file inside a
  dot-named folder. It still has to be set by absolute POSIX path; a bare
  `file ".background.png"` resolves against Finder's working directory.
- The volume has to land on plain `/Volumes/syro`, because Finder addresses it
  by name. With a second `syro` mounted the script stops rather than write the
  layout to the wrong disk.

The background is `crates/app/assets/logo.png` scaled to the window width and
cropped on **transparency**, so the mark sits on Finder's own window. In dark mode a
near-black mark on a dark window is close to invisible; the window is otherwise
empty, so the two icons still read.

## The bundle identifier has to match, twice

| where | what | who reads it |
| --- | --- | --- |
| `crates/app/src/main.rs`, `const BUNDLE_ID` | handed to `cx.set_app_identity` | gpui, to tag notifications |
| `crates/app/Cargo.toml`, `[package.metadata.bundle] identifier` | written into `Contents/Info.plist` | macOS, `NSBundle.mainBundle.bundleIdentifier` |

They are not the same field, and macOS decides whether to deliver a system
notification by looking up the second one. If they disagree the app runs fine
and every notification is dropped without a word. `bundle.sh` therefore refuses
to build when they drift, and re-checks the built plist.

The plist is necessary but not sufficient. `cargo-bundle` leaves the app
linker-signed: the signature has no bound `Info.plist`, and its identifier comes
from the executable (`syro-<hash>`), not the bundle. `usernotificationsd`
identifies the process by that signature and rejects the authorization request:

```
usernotificationsd  [syro-ab44add7a4f6578e] Entitlement 'com.apple.private.usernotifications.bundle-identifiers'
                                             required to request user notifications
                    requestAuthorization not allowed: com.syro.app
```

No prompt, no notification — the app runs, the plist says `com.syro.app`, and
nothing is delivered. `bundle.sh` re-signs ad-hoc (`codesign --force --sign -`)
after `cargo bundle`; with the bundle in place, codesign binds the plist and takes
`com.syro.app` as the identifier, and the script re-reads the signature to
confirm it. macOS is unforgiving here: the same binary, only the signature
differing, is accepted or rejected.

Evidence — the **same binary** (byte-identical, `cmp` clean), launched twice,
only the path differing:

```
bare  target/release/syro          RUST_LOG=info  →  "system notifications disabled:
                                                           not running from an app bundle"
in    syro.app/Contents/MacOS/     RUST_LOG=info  →  (no such line; 0 lines of log)
```

`gpui-pre-macos` guards its `UNUserNotificationCenter` on exactly that lookup,
because the framework aborts the process when there is no bundle proxy. So the
code path P3 built is dead in every `cargo run` and live in the bundle. What
that does *not* prove is delivery: the first real notification raises the macOS
permission prompt, and no agent was driven to a permission state here (that
needs the UI). The bundled binary was also run with a repo in `state.json`: the
window came up, spawned one `/bin/zsh` child, and left no orphan when signalled.

## Measurements

**These numbers predate the removal of the plugin system.** They were taken when
the plugin system (and the QuickJS runtime under it) was still linked in, so the
binary size and the RSS rows below are an upper bound; the plugin-specific scene
and sections that used to be here are gone with the feature. Re-run
`packaging/measure.sh` to replace them.

2026-10-12, Apple M1 Pro, 32 GB, macOS 27.0.1, 10 cores, release build
(`target/release/syro`, 56.0 MB). Procedure unchanged:
`packaging/measure.sh <binary> <label>` — two scenes, each in its own throwaway
`HOME` so none of this touches a real config directory, 12 s settle. RSS is
`ps -o rss=` in KB and the MB below is KB/1024; footprint is `footprint -p`,
the number the memory target is stated in.

| scene | launched from | RSS | footprint | cold start | processes |
| --- | --- | --- | --- | --- | --- |
| empty window | the bare binary | 79.6 MB | 60 MB | – | 1 |
| empty window | inside the bundle | 104.9 MB | 62 MB | – | 1 |
| 1 repo, 5 worktrees, 1 shell | the bare binary | 83.4 MB | 63 MB | 306 ms | 2 |
| 1 repo, 5 worktrees, 1 shell | inside the bundle | 108.8 MB | 65 MB | 303 ms | 2 |

- **cold start** is timed to the first child process — the shell the window
  starts when it comes up. It is a *proxy* for "interactive", not a measurement
  of it; there is no cheaper external one. 303 ms against the 500 ms target.
- **processes** is the GUI plus one shell per active worktree: no helper, no
  renderer, no daemon, as section 1 asks.
- **the worktree scene minus the empty one is +3.9 MB RSS / +3 MB footprint**,
  and that difference contains the repo tree, five rows and one live shell. An
  external measurement cannot separate a terminal from the tree beside it (see
  below).

### The same bytes measure ~25 MB apart, depending on where they are launched

Both contexts were measured twice; the gap is reproducible (79.6 and 79.5 MB
bare against 105.3 and 105.1 MB bundled, empty window). Footprint moves by only ~2 MB in
the same comparison, and the footprint category tables are the same to within a
megabyte — IOSurface 26 MB, Malloc Small 14 vs 15 MB, IOAccelerator 3.9 MB in
both. So the extra RSS is clean, file-backed pages: mappings the bundle context
pulls in because a bundled app is a real app to macOS (LaunchServices, the
notification registration the `set_app_identity` path is now allowed to make,
more framework and `__TEXT` regions). RSS counts clean shared pages; footprint
counts dirty and compressed ones. The gap and its shape are measured; that
reading of *why* is the likely one, not a measured fact.

### Against the memory targets

| target | measured | |
| --- | --- | --- |
| empty window < 80 MB | footprint 60 MB bare, 62 MB bundled | met |
| | RSS 79.6 MB bare, 104.9 MB bundled | **not met in the bundle** — and only 0.4 MB under it bare |
| 5 worktrees, 10 terminals, 5 editor tabs < 300 MB | still not measurable from outside | see below |
| cold start < 500 ms | 303–306 ms (proxy) | met |
| 1 process + shells | 1 GUI + 1 shell per active worktree | met |

The RSS row is the one to read carefully: the plan's number is footprint and
that is met in both contexts, but RSS is over the 80 MB target in the bundle and
0.4 MB under it bare. Picking footprint as the answer would be picking the
flattering one; both are in the table for that reason, and the divergence is
explained above rather than argued away.

### What was not measured, and why

**The 10-terminal / 5-editor-tab scene.** The app opens a terminal for the
active worktree and one per worktree you switch to, and there is no way to
switch from outside: this machine has no automation permission (no Accessibility
grant, and `screencapture` cannot create an image from the display). A true
per-terminal figure needs a headless harness that opens ten terminals in one
window, which the app's own tests can do and this script cannot. When that
harness exists its numbers belong in this file, from the harness, not from
here.

**Notification delivery**, as above: the authorization request now reaches
`usernotificationsd` (the linker-signed bundle was rejected outright), but the
framework here is lazy — it asks for authorization only when the app posts its
first notification — and no agent has been driven to a permission state from the
UI. Whether the banner is delivered and the click handled is still unverified.

**The config every number ran on.** Each scene above
used a fresh `HOME`, so no usage source was signed in and nothing was fetched. One smaller gap from the last
round is now closed: the app restores tabs, so a saved layout with terminals in
it is reachable — but still only through the UI.


- Ad-hoc code signing (so notifications work), but no Developer ID and no
  notarization: the bundle runs on this machine and would be refused by
  Gatekeeper anywhere else. `ponytail:` a release needs a Developer ID and
  notarization, and that is a decision about an account, not about this script.
- One mark, two files, both transparent: `crates/app/assets/logo.png` is the
  1024pt canvas with the mark where it always sat and the panel it used to sit on
  keyed out to alpha; `app-icon.icns` is that logo with the mark trimmed and
  scaled to 590px of the 1024 canvas (macOS puts it on its own plate, where the
  original 394px read small), through `magick` + `iconutil`:
  `magick logo.png -trim +repage -resize x590 -background none -gravity center -extent 1024x1024 master.png`,
  then the ten iconset sizes from `master.png`. It is
  committed and wired in through `icon` in `[package.metadata.bundle]`, which is
  what puts it in `Contents/Resources` and names it in `CFBundleIconFile`.
  `bundle.sh` fails when the bundle ends up without it — cargo-bundle drops a
  missing icon silently. Nothing regenerates the icns, so if you edit `logo.png`
  you re-run the iconset by hand. The mark is `#222222`: on a dark Dock it is
  faint, and that is the cost of shipping it without a backing shape.
- `cargo-bundle` writes `LSRequiresCarbon = true`, a Carbon-era key that modern
  macOS ignores. Left alone rather than patched out of the plist by hand.
- No codesigning on the DMG, and no CI.
