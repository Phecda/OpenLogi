# OpenLogi — Agent Guide

OpenLogi is a native, local-first alternative to Logitech Options+ written in Rust:
button remapping, DPI, SmartShift, and per-app profiles for Logitech HID++ devices
(Bolt/Unifying receiver, Bluetooth-direct, wired) — no account, no telemetry, plain-TOML
config. macOS and Linux are first-class; Windows is a young but shipping port.
Dual-licensed MIT/Apache-2.0; the `design/` brand assets are proprietary.

The developer handbook (toolchain, packaging, release pipeline) is
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md). This file is the agent-facing contract;
subsystem deep-rules are indexed at the bottom.

## Architecture

Three tiers ship in one install: the **GUI** is a pure IPC client, the **agent** is a
background server owning the input hook and ALL device I/O, and shared orchestration
sits beneath both.

| Crate | Role |
|---|---|
| `openlogi` (root package, `src/`) | The CLI binary — thin wrapper over `openlogi-cli` |
| `crates/openlogi-core` | Pure types: TOML config, device model, action catalog. No I/O, no async |
| `crates/openlogi-hidpp` | Vendored fork of the `hidpp` protocol crate (**lib name `hidpp`**, 0BSD) |
| `crates/openlogi-hid` | Device discovery + HID++ writes over `async-hid` |
| `crates/openlogi-assets` | Device-render registry + cached fetch from OpenLogi asset mirrors |
| `crates/openlogi-cli` | `clap` command tree: `list`, `assets`, `diag` |
| `crates/openlogi-hook` | OS input capture: CGEventTap / evdev+uinput / WH_MOUSE_LL |
| `crates/openlogi-inject` | OS input synthesis: CGEvent / uinput+MPRIS / SendInput |
| `crates/openlogi-agent-core` | Shared orchestration + the tarpc IPC contract (`src/ipc.rs`) |
| `crates/openlogi-agent` | The `openlogi-agent` binary — hook + device I/O server |
| `crates/openlogi-gui` | GPUI + gpui-component desktop app — polls the agent, no device I/O |
| `xtask` | `cargo xtask` maintenance: bundling, packaging, release manifest |

- GUI ↔ agent speak tarpc/bincode over an `interprocess` local socket. The wire format
  is versioned and **append-only** — read `.claude/rules/ipc-protocol.md` before touching it.
- Platform code is cfg-gated per crate (`[target.'cfg(target_os = …)'.dependencies]`).
  The workspace's ObjC FFI is centralized in `crates/openlogi-gui/src/platform/` — read
  that directory's `AGENTS.md` before editing it.

## Build, run, verify

Use the locally installed Rust toolchain and command-line tools directly. **Do not invoke commands through Nix, devenv, direnv, or similar environment wrappers.** Run `cargo`, `rustc`, `git`, and other tools directly from the repo root. If a required tool is missing from `PATH`, stop and report the missing prerequisite instead of activating or installing a Nix environment.

```sh
cargo clippy --workspace --all-targets -- -D warnings
git commit …
```

- Full local gate (same as CI): run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` directly. All three must pass before every commit.
- prek hooks (`prek.toml`): `cargo fmt` at commit; full-workspace clippy at push
  (rust-scoped, so non-Rust pushes skip it).
- The macOS GUI build needs full Xcode for GPUI's Metal shaders. If shader compilation fails, verify the local Xcode installation and selected developer directory; do not switch to Nix or direnv as a workaround.
- Dev-run the app with `cargo run -p openlogi-gui` — a cargo runner wraps it into
  `target/dev/OpenLogi.app`. `cargo build` does NOT refresh that bundle, and a second
  instance exits on the singleton lock: quit the old instance and re-`run` before
  judging a UI change "not applied".
- macOS-green proves nothing about cfg-gated code. CI's linux/windows jobs are the
  authoritative check (`RUSTFLAGS=-D warnings` globally, so plain warnings fail too). Cross-lint locally by running `cargo clippy --target x86_64-pc-windows-gnu -p openlogi-core -p openlogi-hidpp -p openlogi-hid -p openlogi-hook -p openlogi-agent -p openlogi-agent-core --all-targets -- -D warnings` directly. Don't claim cross-platform success without CI.

## Rust standards

Edition 2024, MSRV 1.96. Workspace lints (root `Cargo.toml`): `unsafe_code = "deny"`
(opt out per item with `#[expect(unsafe_code, reason = "…")]` plus a `// SAFETY:`
comment), `clippy::pedantic` at warn, `unwrap_used`/`expect_used` at warn.
`openlogi-hidpp` deliberately does not inherit workspace lints (vendored code). Any
lint suppression carries a `reason`.

Encode invariants in the type system instead of checking them at runtime:

- Wire/firmware values get typed wrappers: `num_enum` for discriminants, `bitflags`
  (`from_bits_retain` when unknown bits are legal) for flag sets. Unknown wire values
  surface as **errors** (`UnsupportedResponse`-style), never as silent fallbacks.
- Replace long parameter lists with Change/Params structs; make illegal combinations
  unrepresentable rather than validated.
- Ownership models resources (`Retained<T>` in the ObjC FFI) and thread affinity is
  proven by types (`MainThreadMarker`, `!Send` handles), not by runtime checks.
- Libraries return `thiserror` types; binaries may use `anyhow`.

House style:

- **Root-cause fixes only.** Never layer compatibility shims over a broken abstraction —
  refactor it. Never change product code to work around a dev-environment quirk; debug
  the environment (or a release build) instead.
- **Prefer mature crates over hand-rolled logic** (retry/backoff, hashing, paths, …).
  Check `cargo tree | grep <candidate>` before adding a dependency and use `cargo add`
  so versions come from the registry. After ANY dependency change, verify the
  `gpui`/`gpui-component` git pins in `Cargo.lock` didn't move (they are held only by
  the lock; restore with `cargo update -p gpui --precise <rev>`).
- Module layout: a module with its own semantics is `foo.rs` (children in a sibling
  `foo/`); `foo/mod.rs` is only for pure namespace shells. Never both for one module.
- Keep files reasonably sized (split around ~500 lines) into real modules — never
  simulate structure with `// ---- section ----` banner comments. But don't
  over-extract either: inline single-use helpers.
- rustdoc every public item. Comments state non-obvious constraints only.
- Tests cover failure and edge paths, not just the happy path (state machines
  especially). No tautological tests that mirror the implementation; never weaken an
  assertion or special-case an input to make a test pass.

## Git & GitHub

- Conventional commits: `type(scope): imperative lowercase description`. Types in use:
  `feat fix refactor chore docs ci perf style build test`. Scopes are crate short names
  (`gui agent hidpp hid core hook ipc cli assets xtask`) or cross-cutting concerns
  (`release ci i18n windows linux macos tray infra`). `i18n` is a scope, not a type.
- Branches: `type/kebab-description` off `master`. Substantial or risky work goes in a
  worktree so parallel work doesn't collide; trivial fixes may go straight to master.
- Commits are small and focused — split unrelated concerns into separate commits; never
  one giant unreviewable diff.
- Merging PRs: **squash by default** with a hand-written subject
  `type(scope): description (#N)` (release-plz parses it; merge commits are disabled).
  Rebase-merge only when every commit on the branch is already release-quality
  conventional. Wait for the Greptile review check and CI before merging — findings get
  fixed, replied to, and resolved, not ignored.
- PR bodies: `## Summary`, `## Changes` (per-crate bullets), `## Testing` listing the
  exact commands run plus hardware-verification status (say "not runtime-tested on
  hardware" when true), and a closing `Fixes #N` line. Screenshots for UI changes.
- **All GitHub artifacts — PR titles/bodies, commits, issues, reviews, comments — are
  written in English.**
- **Never add AI attribution** ("Generated with …", AI co-author trailers) to commits,
  PRs, or issues — including when adopting contributors' work.
- Never post to external repos or reply publicly on the maintainer's behalf — draft the
  text for approval. Keep public drafts short, casual, and problem-focused.
- Contributor PRs are adopted, not rejected: check `maintainerCanModify`, rebase onto
  master in a worktree, fix review findings, push to the fork branch; preserve
  authorship (`Co-authored-by` when re-homing work).
- Issues use the bug/feature/device forms and the `type:`/`area:`/`platform:`/`needs:`/
  `status:` label families. Deferred or out-of-scope work becomes a linked issue, not a
  TODO comment.

## Releases

release-plz drives releases: one unified workspace version, ONE root `CHANGELOG.md`
(never per-crate changelogs), and a single `v{version}` tag that only release-plz
creates — **never hand-create the tag**. Published GitHub releases are immutable:
never re-run a failed release job or re-dispatch on an existing tag.
`release-plz.toml` is the versioning contract — don't trim it.

## Personal macOS release build (my-release only)

This is Zhipeng Su's private installation workflow and must stay on the `my-release` branch. Do not copy it to upstream branches, public release documentation, or CI.

The intended signing identity is `Apple Development: szp12345141@sina.com (NANPA56NE4)`, personal Team ID `ZCZ85Z8LUT`. Do not pass this identity directly to the current `xtask macos package` command and install its output. On macOS 26.5.2, `codesign --verify` succeeds but launchd rejects the app with `RBSRequestErrorDomain Code=5` / `NSPOSIXErrorDomain Code=163` and removes the rejected bundle. A minimal comparison app proves that this personal identity has the same failure while the company Apple Development identity launches successfully.

The personal identity requires Xcode-managed macOS development provisioning for the GUI and embedded Agent. The current packaging path neither embeds provisioning profiles nor signs with their required entitlements. Implement and validate that profile-backed path before documenting it as usable. Creating profiles may register bundle identifiers in the Apple developer account; do not invoke `xcodebuild -allowProvisioningUpdates` without explicit approval.

Never replace `/Applications/OpenLogi.app` as the first launch test. Extract the candidate into a disposable directory, verify the outer app and embedded Agent report `TeamIdentifier=ZCZ85Z8LUT`, launch the disposable app, and confirm both `openlogi-gui` and `openlogi-agent` remain running without the bundle being removed. Only then install it. The first working switch from official Team ID `8U3ZJ258K9` will require granting macOS permissions again. Later personal builds should retain those permissions only while the Team ID, bundle identifiers, profiles, and signing requirements remain stable. Never use ad-hoc signing for the installed copy.

## Verification

Define the concrete check that proves a change works before writing it — a failing test
that should pass, a command whose output should change, a behavior in the running app —
and loop on that check. Real-hardware verification (physical mice, receivers) is the
maintainer's job: every fix PR states how to test it. Report outcomes honestly,
including what was NOT verified.

## Subsystem rules — read before touching

Claude Code loads these automatically per path; other agents: read the listed file
before editing that area.

| Area | Rule file |
|---|---|
| `crates/openlogi-gui/**` (GPUI app) | `.claude/rules/gui.md` |
| `crates/openlogi-gui/locales/**`, `src/i18n.rs` | `.claude/rules/i18n.md` |
| `crates/openlogi-agent-core/**`, `crates/openlogi-agent/**` (IPC wire) | `.claude/rules/ipc-protocol.md` |
| `crates/openlogi-hidpp/**`, `crates/openlogi-hid/**` | `.claude/rules/hidpp.md` |
| `crates/openlogi-hook/**` (event taps) | `.claude/rules/hook.md` |
| `xtask/**`, `packaging/**`, `scripts/**` | `.claude/rules/xtask.md` (+ `xtask/README.md`) |
| `crates/openlogi-gui/src/platform/**` (ObjC FFI) | `crates/openlogi-gui/src/platform/AGENTS.md` |
