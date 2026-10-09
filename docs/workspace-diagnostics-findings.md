# Workspace diagnostics: orientation findings

This is the findings list from an AI orientation pass (review mode) over branch
`workspace-diagnostics` at `018f6496`, run on 2026-10-08 after the history was logically rebased.
The code is identical from `218c82da` on. Commit hashes below cite that history and change when the
branch is rebased again. The merge base is `60019996` (origin/main). At `018f6496` the branch is
+3679/-105 across 20 files: code +1716/-105 in 14 files under `crates/wgsl-analyzer/src/`, tests
+944 in `crates/wgsl-analyzer/src/tests/pull_diagnostics.rs`, docs +1019 in 5 files.

Companions: the browsable version is `docs/workspace-diagnostics-orient.html`, and
`docs/workspace-diagnostics-report.md` explains the design and lists the open questions. "Question
N" and numbers in parentheses refer to that report's open questions. The plan and branch journal
mentioned below are the author's working notes and are not in the repository. Since this pass ran,
the report and the manual check were corrected for the vscode-languageclient 10.1.2 differences it
found, and line references into those two files were moved to match.

Tags: confidence [stated] / [planned] / [inferred]; severity [blocker] / [major] / [minor]. Each
section is ordered most severe first. [new] marks findings first made by this pass.

## Verification status

- `git merge-tree --write-tree workspace-diagnostics origin/main`: clean. origin/main is still the
  merge base, so main has not moved under the branch.
- **Each code commit builds and passes on its own**, checked in a separate worktree with rustc 1.98
  and `--ignore-rust-version`. `cargo test -p wgsl-analyzer`: fd548f9f 48, 7a7ba545 48, 9418af8a 56, <!-- spellchecker:disable-line -->
  6235a776 64, 1e9962ae 67, 1f6e4c50 75, 218c82da 92. All passed, 0 failed. The later commits change
  only docs.
- At 218c82da (same code as HEAD): `cargo clippy -p wgsl-analyzer --all-targets --no-deps -- -D
  warnings` clean; `cargo fmt --all -- --check` clean.
- `--lib pull_diagnostics` (17 harness tests) ran 10 times at 018f6496: 10/10 green.
- markdownlint-cli2 0.18.1 (repo config) on `docs/book/src/diagnostics.md` and the four
  `docs/workspace-diagnostics-*.md` files: 0 errors. CI pins the v24.2.0 action, which may carry a
  newer markdownlint. typos 1.50.3 (CI's version) on every changed file: clean.
- **vscode-languageclient 10.1.2 (the lockfile's version) was read this run**, fetched with `npm
  pack`. Before this, every client claim came from 9.0.1 in `editors/code/node_modules`. Results are
  in New findings.
- Not run: rustc 1.99 (CI's toolchain), workspace-wide tests and clippy, mdBook, Codex, and the
  manual VS Code check.

## New findings

- [inferred] [major] [new] **With vscode-languageclient 10.1.2, closing a file within about 2 s of a
  workspace answer can leave its diagnostics stuck in the Problems panel.**
  - What changed in the client: 10.1.2's `forgetDocument` (`lib/common/diagnostic.js:287` in the
    10.1.2 package) calls `this.forget(PullState.workspace, document)` on close, which deletes the
    file's workspace result id. 9.0.1's `forgetDocument` does not. In both versions the close also
    does a final document pull before `didClose` is sent, so the server answers it from the open,
    unsaved text.
  - Why the branch depends on the old behavior: two of its fixes need the client to send the closed
    file's id in a later workspace pull. The closed-dependency-file fix in 6235a776 clears an
    unclaimed id (`crates/wgsl-analyzer/src/diagnostics/workspace_report.rs:168`). The "Don't Save"
    fix hashes the open flag (`crates/wgsl-analyzer/src/diagnostics/result_id.rs:38`).
  - When a workspace pull is parked at the close, its `previousResultIds` were captured earlier and
    still hold the id, so the wake clears or refreshes the file correctly.
  - When nothing is parked, the next pull carries no id for the file. This happens in the 2 s gap
    after each answer, or when the close cancels a running pull. The server then omits a clean file
    (`workspace_report.rs:307`) and never checks a closed library file
    (`crates/wgsl-analyzer/src/global_state.rs:682`). The errors from the final pre-close document
    pull then stay until the session ends.
  - Repros: (a) type an error into a file that is clean on disk, then within 2 s close it with
    "Don't Save"; (b) open a dependency file with errors through go to definition, then close it
    within 2 s.
  - Why nothing caught it: no test models the client. Manual check step 8
    (`docs/workspace-diagnostics-manual-check.md:65`) waits for the next pull to park before
    closing, and the branch docs assert that 10.1.2 behaves the same as 9.0.1.
  - To do: run repros (a) and (b) on 10.1.2 (`cargo xtask install` installs it, see
    `xtask/src/install.rs:89`). If they reproduce, a small server-side fix is to remember the files
    closed since the last answered workspace pull, and on the next pull report each one's current
    state even without a client id (a cleared entry when it is clean or no longer checked). This
    adds server state the design has avoided, but it would also close the narrower "Don't Save"
    residual in question 2.
- [stated] [minor] [new] **The branch docs claim 10.1.2's "relevant code is the same" as 9.0.1; it
  is not** (`docs/workspace-diagnostics-manual-check.md:11`,
  `docs/workspace-diagnostics-report.md:159`).
  - Two relevant differences: the close behavior above, and the default `diagnosticPullOptions`,
    which changed from `{ onChange: true, onSave: false }` to `{ onChange: false, onSave: false,
    onFocus: false }`.
  - The claims that do hold in 10.1.2 (checked this run):
    - a refresh re-pulls open, visible documents only;
    - `handleFailedRequest` turns an unrequested `RequestCancelled` into a counted failure;
    - the response's entries are applied like a chunk's;
    - chunk entries are converted inside a per-item try/catch, but response entries are not.
  - To do: correct the two sentences and re-derive the manual check's expected results against
    10.1.2.
  - Since this pass ran: the report and the manual check now name both differences, and the
    checklist has a step 10 for the close case. Its other expected results are still derived from
    9.0.1.
- [inferred] [minor] [new] **Under 10.1.2, as-you-type diagnostics for open files come only from the
  server's `workspace/diagnostic/refresh`** (`crates/wgsl-analyzer/src/main_loop.rs:533`,
  pre-existing, sent on `became_quiescent || state_changed`).
  - With `onChange: false` and no `diagnosticPullOptions` in `editors/code/src`, the client issues
    no document pull on `didChange`.
  - The woken workspace pull does report open files. But the client ignores workspace entries for
    any file it tracks by document pull (its source says to favor document pull results over workspace results).
  - So question 8's "stop sending refresh" option would freeze open files' diagnostics while typing.
    The refresh is load-bearing; keep it.
  - Not guarded: the harness runs with `ClientCapabilities::default()`, so refresh is off.
- [stated] [minor] [new] **Comments describe per-source-root disabling that cannot happen.**
  `Config::diagnostics` ignores its `source_root` argument and hard-codes `enabled: true`
  (`crates/wgsl-analyzer/src/config.rs:583`). So the `if !config.enabled` branch
  (`crates/wgsl-analyzer/src/handlers/request.rs:399`) and the `continue` at `request.rs:448` are
  dead. The comments at `request.rs:363`, `request.rs:390`, `request.rs:445`,
  `crates/wgsl-analyzer/src/diagnostics/workspace_report.rs:20` and `workspace_report.rs:132`
  describe that dead case as live. This widens question 11: if the plumbing stays, the comments
  should say it waits for a setting that does not exist yet.
- [inferred] [minor] [new] **Review artifacts are committed under `docs/`.** The orientation report,
  these findings and the manual check sit next to the maintainer report. Only the maintainer report
  was planned, and until this branch `docs/` held only `book/`. To decide: keep these files in main,
  or move the findings and the report's open questions into the PR description.
- [inferred] [minor] [new] **Test gaps beyond the earlier list:**
  - No document pull of a file written to the VFS but not yet applied, which is 7a7ba545's main <!-- spellchecker:disable-line -->
    case. Only the non-UTF-8 branch is tested
    (`crates/wgsl-analyzer/src/tests/pull_diagnostics.rs:633`). It is easy to add with `write_file`
    (`:175`) plus `pull_document` (`:149`) and no `process_changes`.
  - No main-loop test sends `didOpen`, `didClose` or `didChange` (the `open`/`close` helpers write
    the document set directly), and none covers a wake while the server is not quiescent. The wake
    site sits inside `if self.is_quiescent()` (`crates/wgsl-analyzer/src/main_loop.rs:496`), so such
    a change waits for the became-quiescent wake.
  - A trap for whoever adds those tests: with default capabilities, an open through the main loop
    makes the push path spawn `Task::Diagnostics` onto the channel `next_task` reads
    (`pull_diagnostics.rs:254`).
- [inferred] [minor] [new] **One file's error fails the whole workspace pull.** The `?` on
  `pull_diagnostics` (`crates/wgsl-analyzer/src/handlers/request.rs:448`) turns any one file's error
  or panic into a failed pull, which counts toward vscode-languageclient's six-failure cutoff. A
  document pull fails only for its own file. This is by design today, but it is the reason the
  empty-message and has-text guards matter so much here.
- [inferred] [minor] **Withdrawn: the `serde_json::to_value(chunk).unwrap()` finding**
  (`crates/wgsl-analyzer/src/lsp/utilities.rs:93`). `report_progress` in the same file unwraps the
  same way, and lsp-server 0.10 unwraps in `Notification::new` and `Response::new_ok`, so a guard
  here removes no reachable panic. This withdraws one of the earlier findings.

## Open edges

The "Where to focus" risks that no test guards, most severe first. Report question numbers in
parentheses.

- [inferred] [major] [new] 10.1.2 close within 2 s strands diagnostics (see New findings).
  `crates/wgsl-analyzer/src/diagnostics/workspace_report.rs:307`,
  `crates/wgsl-analyzer/src/global_state.rs:682`.
- [stated] [major] (4) Every change re-runs the full workspace check while a pull is parked.
  `crates/wgsl-analyzer/src/main_loop.rs:549`. On the 11 MB `.wgsl` corpus that is 370-590 ms per
  change (release build); on bevy `.wesl` it is 3.3-18 ms. No perf test.
- [stated] [major] (1) The final response after chunks is not empty
  (`crates/wgsl-analyzer/src/diagnostics/workspace_report.rs:155`). Guarded only in the sense that
  `tests/pull_diagnostics.rs:690` pins the current behavior. A spec-following client would lose the
  cleared entries. 10.1.2 applies the response, verified this run.
- [inferred] [minor] [new] As-you-type diagnostics depend on the refresh at
  `crates/wgsl-analyzer/src/main_loop.rs:533` under 10.1.2. Not tested.
- [stated] [minor] (3) Empty messages are patched only on the push path
  (`crates/wgsl-analyzer/src/global_state.rs:529`); the pull converter at
  `crates/wgsl-analyzer/src/handlers/request.rs:511` is not. In 10.1.2 a bad entry in a chunk is
  logged and dropped, but a bad entry in the response fails the pull.
- [stated] [minor] (5) `$/progress` chunks can follow a `RequestCancelled` answer.
  `crates/wgsl-analyzer/src/handlers/notification.rs:39`,
  `crates/wgsl-analyzer/src/lsp/utilities.rs:85`. `diagnostic_pull_cancelled`
  (`crates/wgsl-analyzer/src/handlers/request.rs:461`) is never reached by a test.
- [stated] [minor] (2) The "Don't Save" residual.
  `crates/wgsl-analyzer/src/diagnostics/result_id.rs:38`. The fixed case is guarded by
  `tests/pull_diagnostics.rs:579`. The comment's reasoning assumes 9.0.1's close behavior.
- [stated] [minor] (6) URI spelling: two client spellings collapse, and the earlier one gets no
  answer. `crates/wgsl-analyzer/src/diagnostics/workspace_report.rs:60`. No test.
- [inferred] [minor] (not a report question) Pulls before quiescence compute against the old package
  graph. The gate is only `vfs_done` (`crates/wgsl-analyzer/src/dispatch.rs:284`). No test.
- [stated] [minor] (7) Document pulls hash `open = true`
  (`crates/wgsl-analyzer/src/diagnostics/result_id.rs:82`).
- [stated] [minor] (not a report question) `FlushPolicy::SERVER` is never exercised
  (`crates/wgsl-analyzer/src/diagnostics/workspace_report.rs:195`).

## Deliberately not done

- [stated] [minor] Work-done progress ("checked n/m files"): VS Code sends no `workDoneToken`.
- [stated] [minor] Cleared entries do not stream
  (`crates/wgsl-analyzer/src/diagnostics/workspace_report.rs:256`).
- [stated] [minor] A pull during VFS loading is answered `{items: []}`, not parked
  (`crates/wgsl-analyzer/src/dispatch.rs:284`).
- [stated] [minor] No per-file close counter, debounce, revision skip, or naga memoization.
- [stated] [minor] `editors/code` untouched. It still needs no change on 10.1.2, provided the server
  keeps sending refresh (see New findings).
- [stated] [minor] `Event` / `handle_event` are `pub(crate)` for the harness
  (`crates/wgsl-analyzer/src/main_loop.rs:91`, `:395`).
- [stated] [minor] The manual VS Code check has never been run.

## Open questions

- [inferred] [major] [new] Does the 10.1.2 close scenario reproduce? If yes, fix it on the server
  (track closes since the last answer) before the PR, or file it as a follow-up? This decides
  whether question 2 can be closed rather than accepted.
- [stated] [major] (1) Keep ty's non-empty final response, or conform (send the rest as a final
  chunk, answer `{items: []}`)? `docs/book/src/diagnostics.md:31` changes with the answer.
- [inferred] [minor] [new] Which branch docs merge into main? `docs/book/src/diagnostics.md` clearly
  does. The report, findings, orient HTML and manual check are branch-review material.
- [stated] [minor] [new] Re-derive the manual check's expected results against 10.1.2? (The "same
  code" sentences are now corrected.)
- [stated] [minor] (8, now answered) Keep sending `workspace/diagnostic/refresh` on every state
  change: under 10.1.2 it is what updates open files as you type.
- [stated] [minor] (11) Strip the dead per-root "diagnostics disabled" plumbing and its comments, or
  keep it for a future setting with honest comments?
- [stated] [minor] (12) Keep `pub(crate)` on `Event` / `handle_event`, or add a `#[cfg(test)]` entry
  point?
