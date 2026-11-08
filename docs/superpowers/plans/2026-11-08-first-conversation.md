# First Conversation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The first conversation has the default character introduce herself in the user's language and offer to stay or make way for a new one; `init` stops re-seeding `BOOTSTRAP.md` into lived-in workspaces; the GUI's input hint says "Hello" / 「こんにちは」.

**Architecture:** `BOOTSTRAP.md` stays a fixed template the system prompt already carries; its text changes. `seed_workspace` gains one rule (BOOTSTRAP.md only alongside a new IDENTITY.md). The GUI registers its own Fluent resources with the framework's i18n and reads the hint through `Message`, so it follows `gui.locale`.

**Tech Stack:** Rust; `corundum_framework_gui::i18n` (re-exported as `sapphire_framework_gui::i18n`): `Resources`, `register`, `install`, `LanguageSettings`, `Language`, `Message`, `check::{parse_errors, key_difference, unknown_ids}`.

**Spec:** `docs/superpowers/specs/2026-11-08-first-conversation-design.md`

## Global Constraints

- No language setting in the agent's config; no change to `workspace.rs` (CHARACTER.md stays out of the system prompt).
- `BOOTSTRAP.md` is written in English, fixed text, about as long as today's (≈50 lines).
- Placeholder copy, exact: en `Hello`, ja `こんにちは`. Other languages fall back to English (the framework's behaviour).
- Commit scopes: `(workspace)` for the server, `(gui)` for the GUI crate, `(desktop)` for `desktop/`.
- A GUI-only change makes no desktop release: Task 3 also adds a line under `## [Unreleased]` in `desktop/CHANGELOG.md`.
- Public artifacts never mention how the character's assets were made.

## Review Focus

1. Re-running `init` on a workspace whose user deleted `BOOTSTRAP.md` must not bring it back (test in Task 1).
2. A workspace with a hand-written `IDENTITY.md` but no `BOOTSTRAP.md` (an operator who set up by hand) must not get one (test in Task 1).
3. A user who deleted `CHARACTER.md` must still get a working first conversation — the template must say what to do (guard test in Task 2 checks the text names the fallback).
4. The GUI without settings (no bridge) must still show the hint, in English (Task 3: lookup before any `install` gives English).
5. A new `.rs` string literal starting with the GUI's message prefix but missing from the `.ftl` files must fail the build's tests (Task 3: `unknown_ids`).

---

### Task 1: `init` seeds BOOTSTRAP.md only with a new IDENTITY.md

**Files:**
- Modify: `crates/sapphire-agent-server/src/cli_init.rs` (`seed_workspace`, `run`'s message, tests)

**Interfaces:**
- Produces: `seed_workspace(root) -> Result<SeedReport>` unchanged in signature; `BOOTSTRAP.md` lands in `created` only when `IDENTITY.md` does too, else in `skipped`.

- [ ] **Step 1: Write the failing tests** in `cli_init.rs`'s `tests`:

```rust
#[test]
fn a_workspace_with_an_identity_gets_no_bootstrap() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("IDENTITY.md"), "- **Name:** Kit\n").unwrap();

    let report = seed_workspace(d.path()).unwrap();

    assert!(!d.path().join("BOOTSTRAP.md").exists());
    assert!(report.skipped.iter().any(|r| r == "BOOTSTRAP.md"));
    assert!(!report.created.iter().any(|r| r == "BOOTSTRAP.md"));
}

#[test]
fn a_finished_bootstrap_does_not_come_back() {
    let d = tempfile::tempdir().unwrap();
    seed_workspace(d.path()).unwrap();
    std::fs::remove_file(d.path().join("BOOTSTRAP.md")).unwrap();

    seed_workspace(d.path()).unwrap();

    assert!(!d.path().join("BOOTSTRAP.md").exists());
}
```

- [ ] **Step 2: Run them** — `cargo test -p sapphire-agent-server --lib cli_init` — Expected: both FAIL (BOOTSTRAP.md is created).

- [ ] **Step 3: Implement.** In `seed_workspace`, decide once before the loop whether `IDENTITY.md` exists; for `BOOTSTRAP.md`, treat "IDENTITY.md existed before this run" like "already exists" (push to `skipped`, write nothing). Update the `seed_workspace` doc comment to state the rule and why (a lived-in agent must not be re-introduced to the default character). Change the message in `run` to:

```
BOOTSTRAP.md drives the first conversation: the default character
introduces herself and asks whether to keep her or make a new one,
writes the answer into IDENTITY.md and SOUL.md, then deletes the file.
```

Extend the doc comment of `every_seeded_file_reaches_the_system_prompt`: `CHARACTER.md` is the one seeded file the prompt does not read; the first conversation reads it with `file_read`.

- [ ] **Step 4: Run** — `cargo test -p sapphire-agent-server --lib cli_init` — Expected: all PASS (including `a_fresh_directory_gets_every_workspace_file` and `a_second_run_keeps_what_the_user_wrote_and_reports_it_as_skipped`).

- [ ] **Step 5: Commit** — `fix(workspace): init leaves BOOTSTRAP.md out of a workspace that has an identity`

### Task 2: BOOTSTRAP.md introduces the default character

**Files:**
- Modify: `crates/sapphire-agent-server/templates/workspace/BOOTSTRAP.md`
- Test: `crates/sapphire-agent-server/src/cli_init.rs` (`tests`)

**Interfaces:**
- Consumes: `TEMPLATES` (to find the embedded `BOOTSTRAP.md` body); `CHARACTER.md` as merged in #382 (its "Name", "Speaking Japanese", "Speaking English" headings).

- [ ] **Step 1: Write the failing guard test:**

```rust
/// The first conversation's steps live only in BOOTSTRAP.md's prose; a
/// rewrite that drops one would go unnoticed. Cheap check that each file
/// the ritual reads or writes is still named, and the fallback too.
#[test]
fn the_bootstrap_names_every_file_its_ritual_touches() {
    let body = TEMPLATES
        .iter()
        .find(|(rel, _)| *rel == "BOOTSTRAP.md")
        .map(|(_, body)| *body)
        .unwrap();
    for name in ["CHARACTER.md", "IDENTITY.md", "SOUL.md", "USER.md"] {
        assert!(body.contains(name), "BOOTSTRAP.md never mentions {name}");
    }
    assert!(
        body.contains("no `CHARACTER.md`"),
        "BOOTSTRAP.md must say what to do without CHARACTER.md"
    );
}
```

- [ ] **Step 2: Run** — `cargo test -p sapphire-agent-server --lib the_bootstrap_names` — Expected: FAIL (today's text names neither CHARACTER.md nor USER.md).

- [ ] **Step 3: Rewrite `BOOTSTRAP.md`** per spec §2, in English, keeping today's voice and length. Required content, in this order:
  - Title `# BOOTSTRAP.md — First Run` and the opening italic line; the note that `init` seeds it with the other files and `memory/` is empty.
  - **The user's request comes first** paragraph, kept; add: reply in the language the user wrote in.
  - `## 1. Introduce yourself` — read `CHARACTER.md` with `file_read`; speak as that character, following its section for the user's language (Japanese → "Speaking Japanese"; English or any other language → "Speaking English") and the nickname for that language; ask, briefly and without pressure, whether to keep this character or make a new one. A line beginning `If there is no \`CHARACTER.md\``: skip the introduction and go to "make a new one".
  - `## 2. Write down the answer` — **Keep**: write `IDENTITY.md` (name, nickname, what you are, the vibe line, an emoji you choose and show the user) and `SOUL.md` (who she is, personality, and both speaking sections including what to avoid and the sample lines; under the template's Core Truths / Boundaries / Vibe / Continuity headings, speech under Vibe). **New**: today's flow — ask what to call you (do not choose a name yourself), agree a vibe line and emoji, write both files; mention the look can be replaced on the Character page. Keep today's warning that leaving templates erases what was agreed.
  - `## 3. Note the language` — add a directive to `USER.md` in its format (imperative, `<!-- observed: <date> | status: active -->`), e.g. `- Prefer replying in Japanese.`; change it later only when the user asks to switch.
  - `## 4. One safety note` — today's text.
  - Closing: delete this file (not `CHARACTER.md`, which stays as a reference), then one line inviting them to start; "Nothing recreates `BOOTSTRAP.md` once it is gone".

- [ ] **Step 4: Run** — `cargo test -p sapphire-agent-server --lib cli_init` — Expected: all PASS.

- [ ] **Step 5: Commit** — `feat(workspace): the first conversation introduces the default character`

### Task 3: The GUI's input hint follows the language

**Files:**
- Create: `crates/sapphire-agent-gui/locales/en/agent.ftl`, `crates/sapphire-agent-gui/locales/ja/agent.ftl`, `crates/sapphire-agent-gui/src/i18n.rs`
- Modify: `crates/sapphire-agent-gui/src/lib.rs` (`mod i18n;`), `crates/sapphire-agent-gui/src/settings.rs` (`follow`), `crates/sapphire-agent-gui/src/companion/view.rs` (hint), `desktop/CHANGELOG.md`

**Interfaces:**
- Produces: `pub(crate) const RESOURCES: sapphire_framework_gui::i18n::Resources` named `"sapphire-agent-gui"`; message id `agent-companion-input-hint`.

- [ ] **Step 1: Write the failing tests** in `src/i18n.rs` (`#[cfg(test)] mod tests`), mirroring sapphire-framework-gui's `i18n_check.rs`:
  - `agent_files_parse`: `parse_errors(&RESOURCES)` is empty.
  - `agent_en_and_ja_have_the_same_keys`: `key_difference(&RESOURCES, "en", "ja")` is empty.
  - `agent_ids_exist`: `unknown_ids(&RESOURCES, "en", <CARGO_MANIFEST_DIR>/src, "agent-", &[])` is empty.
  - `the_input_hint_greets_in_the_chosen_language`: on an `egui::Context::default()`, `register(&ctx, RESOURCES)`; `install(&ctx, &LanguageSettings { language: Language::Explicit("ja".into()) })` → `Message::new("agent-companion-input-hint").text(&ctx) == "こんにちは"`; same with `"en"` → `"Hello"`.
  - `the_input_hint_is_english_before_a_language_is_installed`: `register` only, no `install` → `"Hello"`.

- [ ] **Step 2: Run** — `cargo test -p sapphire-agent-gui --lib i18n` — Expected: compile FAIL (`RESOURCES` missing).

- [ ] **Step 3: Implement.**
  - `locales/en/agent.ftl`: a header comment (`# sapphire-agent-gui's strings. Keys are agent-<area>-<what>.`) and `agent-companion-input-hint = Hello`. `locales/ja/agent.ftl`: the header in Japanese and `agent-companion-input-hint = こんにちは`.
  - `src/i18n.rs`: `RESOURCES` with `name: "sapphire-agent-gui"` and `files: &[("en", include_str!("../locales/en/agent.ftl")), ("ja", include_str!("../locales/ja/agent.ftl"))]`; module doc says these follow `gui.locale` through the framework.
  - `settings::follow`: `i18n::register(ctx, crate::i18n::RESOURCES)` right after the framework's `register`, before the `settings` early return (so the hint resolves without a bridge).
  - `companion/view.rs`: `.hint_text(Message::new("agent-companion-input-hint").text(ctx))` in place of `"Type a message…"` (`ctx` is the egui context already in scope there; use `ui.ctx()` if not).
  - `desktop/CHANGELOG.md`, under `## [Unreleased]`: `### Changed` / `- The message box's hint greets in the app's language: "Hello", or 「こんにちは」 in Japanese`.

- [ ] **Step 4: Run** — `cargo test -p sapphire-agent-gui --lib` and `cargo clippy -p sapphire-agent-gui --all-targets -- -D warnings` — Expected: PASS, no warnings.

- [ ] **Step 5: Commit** — `feat(gui): the message box greets in the app's language` (code) and `docs(desktop): the greeting hint in the changelog` (CHANGELOG), two commits.

### Final check

- [ ] `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` — all clean.
- [ ] Manual (spec §5), reported to the user rather than run unattended: a fresh workspace, a first message in Japanese and one in English, each through "keep" and "new"; IDENTITY.md / SOUL.md / USER.md written, BOOTSTRAP.md gone, CHARACTER.md kept.
