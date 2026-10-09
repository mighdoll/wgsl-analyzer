# Workspace diagnostics: manual VS Code check

No automated test drives the real VS Code client against this branch. The
`tests/pull_diagnostics.rs` harness drives the server's main loop with a
simulated client. This checklist exercises the four mechanisms end to end:
result ids, the workspace report, streaming, and long polling. The design is
described in [workspace-diagnostics-report.md](workspace-diagnostics-report.md).

The expected results below come from reading the server code and
vscode-languageclient 9.0.1's `diagnostic.js`. None of them has been observed
yet. The extension's lockfile pins 10.1.2, which differs in two ways that
affect this check. Closing a file also drops its workspace result id, and the
client no longer pulls a document on every change; edits reach open files
through the server's `workspace/diagnostic/refresh` instead. Run the check on a
fresh install, and expect steps 3, 5, 8 and 9 to need a second look. Step 10
targets the first difference.

## Setup

1. Run `cargo xtask install` (server and extension), or point
   `wgsl-analyzer.server.path` at a build of this branch.
2. Set `"wgsl-analyzer.trace.server": "verbose"` and open the "wgsl-analyzer
   Language Server Trace" output channel.
3. Make a folder with `a.wgsl` (one error) and `b.wgsl` (clean), both closed.
   For step 6 you also need a folder whose check takes well over 50 ms. A
   260-file, 11 MB `.wgsl` corpus took 1.58 s cold in a release build.

## Steps and what the trace should show

1. **First workspace pull.** Shortly after startup the client sends
   `workspace/diagnostic` with a `partialResultToken`. A pull that arrives while
   the project still loads is answered `{"items": []}` at once, so look at the
   first one after loading. Its response, or a `$/progress` chunk before it, has
   a `full` entry with a `resultId` for `a.wgsl` and nothing for `b.wgsl`. The
   Problems panel shows the error in `a.wgsl` without the file being opened.
2. **Long polling.** About 2 s later a second `workspace/diagnostic` arrives,
   with the id for `a.wgsl` in `previousResultIds`. It gets no response while
   idle, and no further request follows every 2 s.
3. **Document result ids.** Open `a.wgsl`. `textDocument/diagnostic` answers a
   `full` report with a `resultId`. The open also answers the parked workspace
   pull with a `full` entry for `a.wgsl` carrying its `version`. The id changes
   when a file opens, because it includes whether the file is open. The next
   workspace pull, about 2 s later, sends the document pull's id and parks. Add
   a comment line at the end of the file, below the error so no range moves.
   The next document pull sends `previousResultId` and gets
   `{"kind": "unchanged", ...}`. The parked workspace pull re-runs on the edit
   and stays unanswered.
4. **A change answers the parked pull.** Introduce an error in `b.wgsl`. The
   parked pull is answered with a `full` entry for `b.wgsl`, and the next
   workspace pull follows about 2 s later and parks.
5. **Clearing.** Save and close `a.wgsl`. The close answers the parked pull with
   a `full` entry for `a.wgsl` with `"version": null` and a new `resultId`, and
   the next pull parks. Then fix its error from a terminal. The parked pull is
   answered with an entry for `a.wgsl` with `"items": []` and no `resultId`. The
   Problems entry goes, and later pulls no longer list `a.wgsl` in
   `previousResultIds`.
6. **Streaming.** In the large folder, the first workspace pull's `$/progress`
   notifications, whose token is its `partialResultToken`, arrive before its
   response. The response carries the remaining entries. No file appears twice
   across the chunks and the response.
7. **Cancel and shutdown.** With a pull parked, run "Stop server"
   (`wgsl-analyzer.stopServer`). The parked request is answered, either with a
   `RequestCancelled` error after `$/cancelRequest` or with `{"items": []}`
   before the `shutdown` response, and stopping does not hang.
8. **Open dependency file.** Open a `.wgsl` file with an error that lies under
   no package root. A file outside the workspace folder will do; the real case
   is a dependency file opened with go to definition. The parked pull is
   answered with a `full` entry for it carrying its `version`, and the next pull
   lists its id and parks. Close its tab. Once `textDocument/didClose` reaches
   the server, the parked pull is answered with an entry for the file with
   `"items": []`, no `resultId` and `"version": null`, and its Problems entry
   goes.
9. **"Don't Save" close.** With the error in `a.wgsl` saved on disk, open it and
   wait for the workspace answer from step 3. Fix the error without saving, and
   the Problems entry goes. Then close the tab and choose "Don't Save". A later
   workspace response has a `full` entry for `a.wgsl` with the error and
   `"version": null`, and the Problems panel shows the error again. A response
   with a cleared entry for `a.wgsl` may come first, while the server still
   holds the unsaved text.
10. **Close within 2 s of an answer (10.1.2).** Watch the trace for a
    workspace response, then within 2 s do one of these. Type an error into
    `b.wgsl` (clean on disk) and close it with "Don't Save". Or open a
    dependency file with errors and close its tab. Correct behavior: the
    Problems entry for that file goes once the next workspace pull is answered.
    If the entry stays for the rest of the session, the gap described in open
    question 2 of the maintainer report is real.

## Failure signs

- "Workspace diagnostic pull failed" in the client output. After the sixth
  failure, vscode-languageclient stops workspace pulls for the session.
- A new `workspace/diagnostic` request every 2 s while idle.
- A parked request that an edit changing diagnostics does not answer.
