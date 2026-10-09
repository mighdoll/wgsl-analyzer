# Workspace diagnostics: implementation report

This branch implements LSP 3.17 `workspace/diagnostic` for wgsl-analyzer
([issue #975](https://github.com/wgsl-analyzer/wgsl-analyzer/issues/975)) and
moves `textDocument/diagnostic` onto the same result-id scheme. The design
follows ty's implementation in ruff
([workspace_diagnostic.rs @ d1d6a5d](https://github.com/astral-sh/ruff/blob/d1d6a5d4ae2845e40f33c9f4af4706dba314a359/crates/ty_server/src/server/api/requests/workspace_diagnostic.rs)),
adapted to this server's rust-analyzer-style main loop. It was planned by a
Claude session and built by Claude agents; this report is for the maintainers
who review it. The spec section is
[workspace_diagnostic](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#workspace_diagnostic).

## What a user gets

- Diagnostics for every `.wgsl` and `.wesl` file in the project's local source
  roots, open or not, plus every open shader file (so a dependency file opened
  through go-to-definition is covered while it is open).
- Unchanged files cost almost nothing on the wire: the server answers
  "unchanged" when the client's result id still matches.
- Results stream to the client as they are computed when the client sends a
  `partialResultToken` (VS Code always does).
- An idle editor costs nothing: a workspace pull with nothing new is held open
  and answered only when the server's state changes. Without this, VS Code
  re-pulls the whole workspace every 2 seconds.

Nothing in `editors/code` changed. vscode-languageclient starts workspace pulls
on its own once the server advertises `workspaceDiagnostics: true`.

## How it works

All paths are under `crates/wgsl-analyzer/src/`.

### Result ids (`diagnostics/result_id.rs`)

A result id is a fingerprint of the LSP diagnostics the server would send for
one file: the converted `lsp_types::Diagnostic` list (ranges after line-index
conversion, related information, tags) plus whether the file is open, hashed
with a fixed-key SipHash and rendered as 16 hex digits. A file with no
diagnostics has no id. The server stores nothing between pulls; it recomputes
the diagnostics and compares ids. `document_diagnostic_report` is the pure
decision for `textDocument/diagnostic`: unchanged when the ids match, full
with the new id otherwise. Before this branch the document handler echoed the
client's `previousResultId` back as the new id, so an unchanged report was
impossible.

Why open-ness is part of the id: vscode-languageclient feeds document-pull ids
into workspace pulls and prefers them. Without the flag, a file fixed in the
editor but closed with "Don't Save" could come back as "unchanged" against an
id learned while the file was open, and the on-disk errors would stay hidden.

### Workspace report (`diagnostics/workspace_report.rs`)

`handle_workspace_diagnostics` (in `handlers/request.rs`) resolves the client's
`previousResultIds` to `FileId`s, enumerates the files to check
(`GlobalStateSnapshot::workspace_shader_files`), computes each file's
diagnostics with the same helper the document pull uses, and feeds them to a
`WorkspaceReportBuilder`. Per file: a matching id gives an unchanged entry; a
non-matching id or no id gives a full entry with the new id; no diagnostics
with a previous id gives a "cleared" entry (`full`, no id, no items) so the
client drops its state; no diagnostics and no previous id gives nothing. Ids
the client sent for files the pull did not check are cleared under the URI
string the client sent, unless the file is open (vscode-languageclient keeps
re-sending a document-pull id for an open file, and clearing it every pull
would defeat long polling).

### Streaming

With a `partialResultToken`, changed (full) entries are buffered and flushed as
`$/progress` chunks at most every 50 ms. The first chunk is a
`WorkspaceDiagnosticReport`, later ones are
`WorkspaceDiagnosticReportPartialResult`; the wire shape is identical. Unchanged
and cleared entries never stream; they go in the final response together with
any unflushed changed entries. The handler sends chunks through a clone of the
connection's sender held by `GlobalStateSnapshot` (`PartialResultSender` in
`lsp/utilities.rs`). This matches ty, and departs from the spec's general rule
that a final response after partial results should be empty. vscode-languageclient
9.x and 10.x accept the non-empty response. See open question 1.

### Long polling (`long_poll.rs`, `dispatch.rs::on_long_poll`, `main_loop.rs`)

`WorkspaceReportBuilder::finish` returns `LongPoll::Suspend` when every entry is
unchanged (or there are none) and no chunk was sent; otherwise
`LongPoll::Respond(report)`. A new dispatcher method, `on_long_poll`, replaces
`on_with_vfs_default` for this request because the latter always responds; it
takes the same three arguments (handler, VFS-loading default, cancellation
error). On `Suspend` the worker returns `Task::Suspend`, and the main loop
parks the request in `GlobalState::suspended_request` (a `SuspendedRequest`
slot). The slot counts wake-ups; if the count moved while the handler ran, the
request runs again immediately instead of parking a stale answer.

Wakes happen wherever the push path already refreshed: after `process_changes`
reports a change, when a document opens or closes, when the project becomes
quiescent, and (added during review) after a configuration change in
`reload.rs::update_configuration`. A cancel while parked answers
`RequestCancelled`; shutdown answers a parked request with an empty report; a
second pull that parks while one is parked displaces the first, which is
answered with an empty report (not `RequestCancelled`, which
vscode-languageclient counts toward a six-failure cutoff that stops workspace
pulls for the session). Salsa cancellation mid-run answers `ServerCancelled`
with `retriggerRequest: true`, as the document pull already did.

### Where this departs from ty

- No `supports_full_diagnostic_output()` gate (an Astral-specific extension).
- No notebook handling, no uv-environment gating, no separate "unnecessary
  hints" stream (wgsl-analyzer already folds `unused` into diagnostic tags).
- The wake-storm guard is a wake counter on the parked-request slot rather than
  ty's salsa revision.
- Open files are always checked, and open-ness is hashed into the id. ty does
  neither; both fixes came out of tracing vscode-languageclient's bookkeeping
  (see "Found in review").

## Found in review

Three review passes (two partitioned bug passes, one whole-scope cross-file
read) ran after implementation. The fixes they produced:

- **Config changes did not wake a parked pull.** Toggling a diagnostics setting
  waited for the next edit. Fixed with a wake in `update_configuration`.
- **A snapshot could panic on a file the database had not seen.**
  `process_changes` recorded line endings under the VFS lock before
  `apply_change`, so a worker could find a file with text but no source root.
  Line endings are now recorded after `apply_change`, which also covered the
  startup case when the std library lands. A document pull of a non-UTF-8 file
  panicked in `file_line_index`; it now answers the empty report.
- **Closed dependency files kept stale diagnostics.** With
  `workspaceDiagnostics: true`, vscode-languageclient stops deleting a closed
  document's diagnostics itself and relies on the workspace pull. Library files
  were never checked, so a dependency file opened with go-to-definition kept its
  errors in the Problems panel after its tab closed. Workspace pulls now check
  every open shader file; the pull after a close clears it (under
  vscode-languageclient 10.1.2 only when a pull is parked at the close, see
  open question 2).
- **A stale workspace id could hide errors after "Don't Save".** Fixed by
  hashing open-ness into the id (above). Narrowed, not fully closed: see open
  question 2.
- **An open file the pull stopped checking forced a full re-check every 2 s.**
  Its unclaimed document-pull id is now left alone while the file is open.

## Verification

- `cargo test -p wgsl-analyzer`: 92 tests, 17 of them in a new
  `tests/pull_diagnostics.rs` harness that builds a real `GlobalState` with an
  in-memory VFS and a recording client channel. It drives both handlers
  directly and the long-polling paths through `handle_event` and the real task
  pool (park, wake, run-again race, displacement, cancel, shutdown, VFS
  loading, config-change wake, the mid-reload race, non-UTF-8 files, open
  library files, the open-id staleness case). Every new test was checked to
  fail against the code it guards.
- `cargo test --workspace` green; `cargo clippy -p wgsl-analyzer --all-targets
  -D warnings` clean; `cargo fmt --check` clean.
- Validated on rustc 1.98 with `--ignore-rust-version` (the workspace asks for
  1.99, which was not installed where this ran); CI on 1.99 is the first real
  check. Workspace-wide clippy on 1.98 fails on `crates/stdx` (untouched here)
  and `cargo doc -D warnings` fails on a pre-existing broken link in
  `global_state.rs`; both predate the branch.
- Not run: mdBook, and the manual VS Code check (nine steps in
  [workspace-diagnostics-manual-check.md](workspace-diagnostics-manual-check.md)).
  Most client-behavior claims were first read from vscode-languageclient 9.0.1
  in `editors/code/node_modules`. The lockfile pins 10.1.2, which differs in two
  ways that matter here. Closing a file also drops its workspace result id, and
  document pulls no longer run on every change by default, so open files update
  as you type only through the server's `workspace/diagnostic/refresh`. The
  first difference opens a gap described in open question 2.

### Measurements

Release build, 260 files per corpus, one `handle_workspace_diagnostics` call:

| corpus | cold pull | re-check, nothing changed | after a one-file edit |
| --- | --- | --- | --- |
| bevy `.wesl` | 77 ms | 3.3 ms | 18 ms |
| Unity boat_attack `.wgsl` (11 MB) | 1.58 s | 370 ms | 592 ms |

Naga runs only for `.wgsl` files and its output is not cached between pulls,
which is where the `.wgsl` cost comes from.

## Open questions for maintainers

Ranked. File and line locations for each, with the test that guards it or a
note that none does, are in
[workspace-diagnostics-findings.md](workspace-diagnostics-findings.md). The
browsable orientation report is `docs/workspace-diagnostics-orient.html`; open
it locally in a browser, since GitHub shows HTML as source.

1. **Non-empty final response after streamed chunks.** ty does this and VS
   Code accepts it, but the spec says the final response should be empty. A
   spec-following client would miss the cleared entries and keep stale
   diagnostics. Conforming is a few lines in `finish` (send the remainder as a
   last chunk, answer `{items: []}`); the docs change with it. Lean: conform.
2. **Closing a file can leave stale diagnostics.** vscode-languageclient
   10.1.2, the version the extension pins, drops a file's workspace result id
   when the file closes (9.0.1 does not). If a workspace pull is parked at the
   close, its request still holds the id and the wake clears or refreshes the
   file. If not, which happens in the 2 s gap after each answer or when the
   close cancels a running pull, the next pull carries no id for the file. A
   clean file is then omitted and a closed dependency file is never checked, so
   errors from the last document pull before the close stay in the Problems
   panel for the session. Repros: type an error into a file that is clean on
   disk and close it with "Don't Save" within 2 s, or open a dependency file
   with errors through go-to-definition and close it within 2 s. Not yet
   reproduced. A server-side fix would remember the files closed since the last
   answered workspace pull and report each one's current state on the next
   pull, even without a client id. That is server state the design has
   avoided, but it would also close the narrower "Don't Save" race the open
   flag leaves under 9.0.1 (it helps only when the id VS Code holds was
   learned while the file was open). Fix before merging, or follow up?
3. **Empty diagnostic messages.** The push path patches an empty message to
   `" "` for VS Code; the pull path does not. One empty message would fail a
   whole workspace pull in vscode-languageclient and count toward its
   six-failure cutoff. No known producer today. Move the patch into the shared
   converter, or leave as pre-existing?
4. **Per-keystroke cost while a pull is parked.** Every state change re-runs
   the whole workspace check (370-590 ms on the 11 MB `.wgsl` corpus, 18 ms on
   bevy `.wesl`). Options: debounce the wake, skip the re-run when the salsa
   revision did not move, or memoize naga per file. Merge and follow up, or fix
   first?
5. **Chunks after `RequestCancelled`.** A cancelled handler keeps streaming
   until salsa cancels it. VS Code drops the chunks. Fixing needs per-request
   cancellation state visible from the snapshot.
6. **URI spelling.** Full and unchanged entries use the server's URI, cleared
   entries use the client's (ty's rule). Answer under the client's URI
   everywhere?
7. **Document pulls hash `open = true` unconditionally.** Right for VS Code; a
   client that pulls documents the server does not have open and mixes ids into
   workspace pulls would never suspend.
8. **Refresh policy (answered: keep it).** The server sends
   `workspace/diagnostic/refresh` when its state changes.
   vscode-languageclient answers it by re-pulling open documents only, so it
   does not disturb a parked workspace pull. Under 10.1.2 it is also the only
   thing that updates an open file's diagnostics as you type: document pulls no
   longer run on change by default, and the client ignores workspace entries for
   files it tracks by document pull.
9. **Cleared entries wait for the final response.** Streaming them is a
   one-line change.
10. **Work-done progress** ("checked n/m files") is not implemented: VS Code
    sends no `workDoneToken`, and server-initiated progress has to go through
    the main loop's outgoing request queue.
11. **Dead "diagnostics disabled" plumbing.** `Config::diagnostics` hard-codes
    `enabled: true`, yet both pull paths and `finish` carry handling for a
    per-root disable. Keep for the future setting, or strip?
12. **`Event` and `handle_event` are `pub(crate)`** for the main-loop tests.
13. **Nothing has run against the real client yet.** The
    [manual VS Code check](workspace-diagnostics-manual-check.md) should run
    before a PR, on a fresh 10.1.2 install.

## Reading the branch

The history was logically rebased after review, so each commit is one reviewable
step and the review fixes are folded into the commits they belong to.

1. **Share the VFS-loading answer in the request dispatcher.** A refactor of
   `dispatch.rs` with no behavior change, so the long-polling dispatch method
   can reuse it.
2. **Fix document pulls of files the database has not seen.** A fix to existing
   behavior: `process_changes` records line endings only after `apply_change`,
   closing the panic window, and a document pull of a file without text answers
   the empty report.
3. **Result ids and unchanged reports for document diagnostics.** Adds
   `diagnostics/result_id.rs`.
4. **Answer `workspace/diagnostic` pulls.** The bulk handler,
   `diagnostics/workspace_report.rs`, the file enumeration, and the capability.
5. **Stream workspace diagnostics as partial results.**
6. **Hold workspace diagnostic pulls open until something changes.** Adds
   `long_poll.rs`, `on_long_poll`, and the wakes in the main loop and in
   `update_configuration`.
7. **Test pull diagnostics against an in-memory workspace.** The
   `tests/pull_diagnostics.rs` harness and its tests. Unit tests for the report
   logic live with the code in steps 3 to 6.
8. **Document pull diagnostics.** The user-facing "Pull diagnostics" section in
   `docs/book/src/diagnostics.md`.
9. **This report**, then commits adding the orientation report, its findings,
   and the manual VS Code check.
