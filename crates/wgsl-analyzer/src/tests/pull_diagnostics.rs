//! Drives the pull diagnostics handlers (`textDocument/diagnostic` and
//! `workspace/diagnostic`) against an in-memory workspace, whose files are
//! written straight into the VFS rather than read from disk. Most tests call
//! the handlers directly. The long-polling tests send requests through the main
//! loop's event handling and the task pool.

use std::{
    thread,
    time::{Duration, Instant},
};

use lsp_server::{ErrorCode, Message, RequestId};
use lsp_types::{
    CancelNotification, CancelParams, ClientCapabilities, ConfigurationRequest,
    DidChangeConfigurationNotification, DidChangeConfigurationParams, DocumentDiagnosticParams, Id,
    Notification as _, PartialResultParams, PreviousResultId, ProgressToken, Request as _,
    ShutdownRequest, TextDocumentIdentifier, Uri, WorkDoneProgressParams,
    WorkspaceDiagnosticParams, WorkspaceDiagnosticReport, WorkspaceDiagnosticRequest,
    WorkspaceDocumentDiagnosticReport,
};
use serde_json::json;
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSetConfig};

use super::get_test_directory;
use crate::{
    config::Config,
    diagnostics::workspace_report::FlushPolicy,
    global_state::GlobalState,
    handlers::request::{
        empty_diagnostic_report, empty_workspace_diagnostics, handle_document_diagnostics,
        handle_workspace_diagnostics, workspace_diagnostics,
    },
    in_memory_documents::DocumentData,
    long_poll::LongPoll,
    lsp::to_proto::url_from_abs_path,
    main_loop::{Event, Task},
    reload::SourceRootConfig,
};

/// A workspace whose only local source root is `src/`. Files under `lib/` fall
/// outside it, so they land in a library source root.
struct Workspace {
    root: AbsPathBuf,
    state: GlobalState,
    /// The messages the server sent the client. Holding the receiver also keeps
    /// the server's sends from failing.
    client: crossbeam_channel::Receiver<lsp_server::Message>,
}

impl Workspace {
    fn new(files: &[(&str, &[u8])]) -> Self {
        let root = get_test_directory().join("pull_diagnostics_fixture");
        let config = Config::new(
            root.clone(),
            ClientCapabilities::default(),
            vec![root.clone()],
            None,
        );
        let (sender, client) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender, config);

        let mut file_sets = FileSetConfig::builder();
        file_sets.add_file_set(vec![VfsPath::from(root.join("src"))]);
        state.source_root_config = SourceRootConfig {
            fsc: file_sets.build(),
            local_filesets: vec![0],
        };

        let mut workspace = Self {
            root,
            state,
            client,
        };
        for (path, contents) in files {
            workspace.write_file(path, contents);
        }
        workspace.state.process_changes();
        workspace
    }

    fn uri(
        &self,
        path: &str,
    ) -> Uri {
        url_from_abs_path(&self.root.join(path))
    }

    /// The result id `value` the client holds for the file at `path`.
    fn previous_id(
        &self,
        path: &str,
        value: &str,
    ) -> PreviousResultId {
        PreviousResultId {
            uri: self.uri(path),
            value: value.to_owned(),
        }
    }

    /// Calls the `workspace/diagnostic` handler directly, without a
    /// `partialResultToken`.
    fn pull_workspace(
        &self,
        previous_result_ids: Vec<PreviousResultId>,
    ) -> LongPoll<WorkspaceDiagnosticReport> {
        let parameters = WorkspaceDiagnosticParams {
            previous_result_ids,
            ..WorkspaceDiagnosticParams::default()
        };
        handle_workspace_diagnostics(self.state.snapshot(), parameters).unwrap()
    }

    /// Like [`Workspace::pull_workspace`], with a `partialResultToken`, so the
    /// pull streams under `policy`.
    fn pull_workspace_streaming(
        &self,
        previous_result_ids: Vec<PreviousResultId>,
        policy: FlushPolicy,
    ) -> LongPoll<WorkspaceDiagnosticReport> {
        let parameters = WorkspaceDiagnosticParams {
            previous_result_ids,
            partial_result_params: PartialResultParams {
                partial_result_token: Some(ProgressToken::from("workspace-pull")),
            },
            ..WorkspaceDiagnosticParams::default()
        };
        workspace_diagnostics(&self.state.snapshot(), parameters, policy).unwrap()
    }

    /// The partial results in the unread messages from the server. Every unread
    /// message must be a `$/progress` notification under the token of
    /// [`Workspace::pull_workspace_streaming`].
    fn partial_results(&self) -> Vec<WorkspaceDiagnosticReport> {
        self.client
            .try_iter()
            .map(|message| {
                let lsp_server::Message::Notification(notification) = message else {
                    panic!("expected a notification, got {message:?}");
                };
                assert_eq!(notification.method, "$/progress");
                assert_eq!(notification.params["token"], "workspace-pull");
                serde_json::from_value(notification.params["value"].clone()).unwrap()
            })
            .collect()
    }

    /// Calls the `textDocument/diagnostic` handler for the file at `path`, and
    /// returns the report as JSON to show its wire shape.
    fn pull_document(
        &self,
        path: &str,
        previous_result_id: Option<&str>,
    ) -> serde_json::Value {
        let parameters = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier::new(self.uri(path)),
            identifier: None,
            previous_result_id: previous_result_id.map(str::to_owned),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let report = handle_document_diagnostics(self.state.snapshot(), parameters).unwrap();
        serde_json::to_value(report).unwrap()
    }

    fn file_id(
        &self,
        path: &str,
    ) -> FileId {
        let path = VfsPath::from(self.root.join(path));
        self.state.vfs.read().0.file_id(&path).unwrap().0
    }

    /// Writes a file's text into the VFS only, as the main loop does while it
    /// handles an event. See [`Workspace::set_file`].
    fn write_file(
        &self,
        path: &str,
        contents: &[u8],
    ) {
        let vfs = &mut self.state.vfs.write().0;
        vfs.set_file_contents(VfsPath::from(self.root.join(path)), Some(contents.to_vec()));
    }

    /// Writes a file's text and applies the change to the database, as the main
    /// loop does at the end of an event, but without waking a suspended pull.
    fn set_file(
        &mut self,
        path: &str,
        contents: &[u8],
    ) {
        self.write_file(path, contents);
        self.state.process_changes();
    }

    /// Opens a file in the editor at `version` with `contents`, as
    /// `textDocument/didOpen` does, without waking a suspended pull.
    fn open(
        &mut self,
        path: &str,
        version: i32,
        contents: &[u8],
    ) {
        let document = DocumentData::new(version, contents.to_vec());
        self.state
            .in_memory_documents
            .insert(VfsPath::from(self.root.join(path)), document)
            .unwrap();
        self.set_file(path, contents);
    }

    /// Closes a file in the editor, as `textDocument/didClose` does, without
    /// waking a suspended pull. The VFS keeps the file's text rather than
    /// reloading it from disk.
    fn close(
        &mut self,
        path: &str,
    ) {
        self.state
            .in_memory_documents
            .remove(&VfsPath::from(self.root.join(path)))
            .unwrap();
    }

    /// Sends the main loop a `workspace/diagnostic` request, as the client would.
    /// Its handler runs on the task pool; see [`Workspace::finish_task`].
    fn send_workspace_pull(
        &mut self,
        id: i32,
        previous_result_ids: Vec<PreviousResultId>,
    ) {
        let parameters = WorkspaceDiagnosticParams {
            previous_result_ids,
            ..WorkspaceDiagnosticParams::default()
        };
        let request = lsp_server::Request::new(
            id.into(),
            WorkspaceDiagnosticRequest::METHOD.to_string(),
            parameters,
        );
        self.state
            .handle_event(Event::Lsp(Message::Request(request)));
    }

    /// Waits for the task pool to finish a task (a request's handler) and hands
    /// the main loop its result.
    fn finish_task(&mut self) {
        let task = self.next_task();
        self.state.handle_event(Event::Task(task));
    }

    /// Waits for the task pool to finish a task and returns its result without
    /// handing it to the main loop. The handler has returned, so its snapshot is
    /// gone and a later change does not cancel it.
    fn next_task(&self) -> Task {
        self.state
            .task_pool
            .receiver
            .recv_timeout(Duration::from_secs(60))
            .expect("no task finished within a minute")
    }

    /// Hands the main loop the VFS loader's report that a file changed on disk.
    /// The main loop applies the change at the end of the event, as it does
    /// after every event.
    fn change_file_on_disk(
        &mut self,
        path: &str,
        contents: &[u8],
    ) {
        let files = vec![(self.root.join(path), Some(contents.to_vec()))];
        self.state
            .handle_event(Event::Vfs(vfs::loader::Message::Changed { files }));
    }

    /// Sends the main loop the client's `workspace/didChangeConfiguration`, and
    /// answers the server's `workspace/configuration` request that follows with
    /// `settings` as the `wgsl-analyzer` section.
    fn change_configuration(
        &mut self,
        settings: serde_json::Value,
    ) {
        let notification = lsp_server::Notification::new(
            DidChangeConfigurationNotification::METHOD.to_string(),
            DidChangeConfigurationParams {
                settings: serde_json::Value::Null,
            },
        );
        self.state
            .handle_event(Event::Lsp(Message::Notification(notification)));
        let Ok(Message::Request(request)) = self.client.try_recv() else {
            panic!("expected the server to ask for the configuration");
        };
        assert_eq!(request.method, ConfigurationRequest::METHOD.as_str());
        let response = lsp_server::Response::new_ok(request.id, vec![settings]);
        self.state
            .handle_event(Event::Lsp(Message::Response(response)));
    }

    /// Sends the main loop the client's `$/cancelRequest` for request `id`.
    fn cancel(
        &mut self,
        id: i32,
    ) {
        let cancel = lsp_server::Notification::new(
            CancelNotification::METHOD.to_string(),
            CancelParams { id: Id::Int(id) },
        );
        self.state
            .handle_event(Event::Lsp(Message::Notification(cancel)));
    }

    /// Sends the main loop the client's `shutdown` request as request `id`.
    fn shutdown(
        &mut self,
        id: i32,
    ) {
        let request = lsp_server::Request::new(id.into(), ShutdownRequest::METHOD.to_string(), ());
        self.state
            .handle_event(Event::Lsp(Message::Request(request)));
    }

    /// Checks that the one response among the unread messages answers request
    /// `id` with `RequestCancelled`.
    fn assert_cancelled(
        &self,
        id: i32,
    ) {
        let response = self.response_to(id);
        #[expect(
            clippy::as_conversions,
            reason = "lsp_server sets this to the JSON RPC value"
        )]
        let request_cancelled = ErrorCode::RequestCanceled as i32;
        assert_eq!(
            response.response_result.as_ref().unwrap_err().code,
            request_cancelled
        );
    }

    /// The responses among the unread messages from the server, dropping the
    /// notifications and requests.
    fn responses(&self) -> Vec<lsp_server::Response> {
        self.client
            .try_iter()
            .filter_map(|message| match message {
                Message::Response(response) => Some(response),
                Message::Request(_) | Message::Notification(_) => None,
            })
            .collect()
    }

    /// The one response among the unread messages, which must answer request
    /// `id`.
    fn response_to(
        &self,
        id: i32,
    ) -> lsp_server::Response {
        only_response(&self.responses(), id)
    }

    /// The one response among the unread messages, which must answer request
    /// `id` with a workspace diagnostic report.
    fn workspace_response(
        &self,
        id: i32,
    ) -> WorkspaceDiagnosticReport {
        workspace_report(self.response_to(id))
    }

    /// Like [`Workspace::workspace_response`], for a pull the main loop ran again
    /// while it handled a task. After handling a task, the main loop also handles
    /// every task already finished, so the re-run may have been answered already.
    /// Waits for the re-run's task only when no response has arrived.
    fn workspace_response_to_rerun(
        &mut self,
        id: i32,
    ) -> WorkspaceDiagnosticReport {
        let mut responses = self.responses();
        if responses.is_empty() {
            self.finish_task();
            responses = self.responses();
        }
        workspace_report(only_response(&responses, id))
    }
}

/// The one response in `responses`, which must answer request `id`.
fn only_response(
    responses: &[lsp_server::Response],
    id: i32,
) -> lsp_server::Response {
    let [response] = responses else {
        panic!("expected one response, got {responses:#?}");
    };
    assert_eq!(response.id, RequestId::from(id));
    response.clone()
}

/// The workspace diagnostic report a response answers with.
fn workspace_report(response: lsp_server::Response) -> WorkspaceDiagnosticReport {
    serde_json::from_value(response.response_result.unwrap()).unwrap()
}

/// One line per entry, sorted, giving the file name, the kind, and for a full
/// report whether it has a result id and whether it has items.
fn summary(report: &WorkspaceDiagnosticReport) -> Vec<String> {
    let mut lines: Vec<String> = report
        .items
        .iter()
        .map(|item| match item {
            WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(full) => {
                let report = &full.full_document_diagnostic_report;
                format!(
                    "{} full id={} items={}",
                    file_name(&full.uri),
                    report.result_id.is_some(),
                    !report.items.is_empty()
                )
            },
            WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(
                unchanged,
            ) => format!("{} unchanged", file_name(&unchanged.uri)),
        })
        .collect();
    lines.sort();
    lines
}

fn file_name(uri: &Uri) -> &str {
    uri.path().rsplit('/').next().unwrap()
}

/// The report's entry for the file named `name`, as JSON to show its wire
/// shape.
fn entry(
    report: &WorkspaceDiagnosticReport,
    name: &str,
) -> serde_json::Value {
    let report = serde_json::to_value(report).unwrap();
    report["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["uri"].as_str().unwrap().ends_with(&format!("/{name}")))
        .unwrap_or_else(|| panic!("no entry for {name} in {report:#}"))
        .clone()
}

/// The result id of the full report for the file named `name`, which must have
/// one.
fn full_result_id(
    report: &WorkspaceDiagnosticReport,
    name: &str,
) -> String {
    report
        .items
        .iter()
        .find_map(|item| match item {
            WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(full)
                if file_name(&full.uri) == name =>
            {
                full.full_document_diagnostic_report.result_id.clone()
            },
            WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(_)
            | WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(_) => {
                None
            },
        })
        .unwrap()
}

/// While no file is open, a pull checks only the shader files in local source
/// roots. The manifest in `src/` (full of parse errors if checked as WGSL), the
/// broken file in the library root, and a file that is not UTF-8 (it has no
/// line-endings entry, see `has_text`, so checking it would panic) produce
/// nothing. A previous
/// id for a clean file or an unknown file gets a cleared entry, and an id from
/// one pull gets an unchanged entry in the next. A pull with only unchanged
/// entries is suspended rather than answered (long polling).
#[test]
fn workspace_pull_checks_local_shader_files_and_round_trips_ids() {
    let workspace = Workspace::new(&[
        ("src/broken.wgsl", b"fn broken( {"),
        ("src/clean.wgsl", b"fn clean() {}"),
        ("src/not_utf8.wgsl", b"fn \xff( {"),
        ("src/Cargo.toml", b"[package]\nname = \"shaders\"\n"),
        ("lib/library.wgsl", b"fn library( {"),
    ]);
    let stale = workspace.previous_id("src/clean.wgsl", "id-from-before-the-fix");
    let unknown = workspace.previous_id("src/deleted.wgsl", "id-of-a-deleted-file");

    let first = workspace
        .pull_workspace(vec![stale.clone(), unknown])
        .unwrap_respond();
    assert_eq!(
        summary(&first),
        [
            "broken.wgsl full id=true items=true",
            "clean.wgsl full id=false items=false",
            "deleted.wgsl full id=false items=false",
        ]
    );

    let broken = workspace.previous_id("src/broken.wgsl", &full_result_id(&first, "broken.wgsl"));
    let second = workspace
        .pull_workspace(vec![broken.clone(), stale])
        .unwrap_respond();
    assert_eq!(
        summary(&second),
        [
            "broken.wgsl unchanged",
            "clean.wgsl full id=false items=false"
        ]
    );
    assert_eq!(workspace.pull_workspace(vec![broken]), LongPoll::Suspend);
}

/// An open file is checked even outside the local source roots (a dependency
/// file opened by go to definition, say), so that the client holds a result id
/// for it. Its entry has the open document's version. While the file stays
/// open, a pull with that id has nothing new. Once the file closes, VS Code
/// leaves clearing its diagnostics to the workspace pull, and the pull clears
/// them in answer to that id.
#[test]
fn workspace_pull_checks_an_open_library_file_and_clears_it_once_closed() {
    let mut workspace = Workspace::new(&[
        ("src/broken.wgsl", b"fn broken( {"),
        ("lib/library.wgsl", b"fn library( {"),
    ]);
    workspace.open("lib/library.wgsl", 4, b"fn library( {");
    let open = workspace.pull_workspace(Vec::new()).unwrap_respond();
    assert_eq!(
        summary(&open),
        [
            "broken.wgsl full id=true items=true",
            "library.wgsl full id=true items=true",
        ]
    );
    assert_eq!(entry(&open, "library.wgsl")["version"], 4);

    let previous_ids = vec![
        workspace.previous_id("src/broken.wgsl", &full_result_id(&open, "broken.wgsl")),
        workspace.previous_id("lib/library.wgsl", &full_result_id(&open, "library.wgsl")),
    ];
    assert_eq!(
        workspace.pull_workspace(previous_ids.clone()),
        LongPoll::Suspend
    );

    workspace.close("lib/library.wgsl");
    let closed = workspace.pull_workspace(previous_ids).unwrap_respond();
    assert_eq!(
        summary(&closed),
        [
            "broken.wgsl unchanged",
            "library.wgsl full id=false items=false"
        ]
    );
    assert_eq!(
        entry(&closed, "library.wgsl"),
        json!({
            "kind": "full",
            "uri": workspace.uri("lib/library.wgsl").as_str(),
            "version": null,
            "items": [],
        })
    );
}

/// A file's result id records whether the file is open, so a pull with an id
/// from while the file was open gets the closed file's diagnostics in full.
/// Once a file closes, VS Code sends the file's id from the last workspace
/// pull, which can name diagnostics VS Code no longer shows (an error fixed in
/// the editor, then closed without saving). While the file is open, the
/// document pull and the workspace pull give it the same id, so a workspace
/// pull with the document pull's id (which VS Code sends for an open file) is
/// suspended.
#[test]
fn a_result_id_from_while_the_file_was_open_is_stale_once_it_is_closed() {
    let mut workspace = Workspace::new(&[("src/a.wgsl", b"fn a( {")]);
    let closed = workspace.pull_workspace(Vec::new()).unwrap_respond();
    let closed_id = full_result_id(&closed, "a.wgsl");

    workspace.open("src/a.wgsl", 1, b"fn a( {");
    let open = workspace
        .pull_workspace(vec![workspace.previous_id("src/a.wgsl", &closed_id)])
        .unwrap_respond();
    assert_eq!(summary(&open), ["a.wgsl full id=true items=true"]);
    let open_id = full_result_id(&open, "a.wgsl");
    assert_ne!(open_id, closed_id);
    assert_eq!(
        workspace.pull_document("src/a.wgsl", None)["resultId"],
        open_id.as_str()
    );
    assert_eq!(
        workspace.pull_workspace(vec![workspace.previous_id("src/a.wgsl", &open_id)]),
        LongPoll::Suspend
    );

    workspace.close("src/a.wgsl");
    let after_close = workspace
        .pull_workspace(vec![workspace.previous_id("src/a.wgsl", &open_id)])
        .unwrap_respond();
    assert_eq!(summary(&after_close), ["a.wgsl full id=true items=true"]);
    assert_eq!(full_result_id(&after_close, "a.wgsl"), closed_id);
}

/// A broken file gets an id and then an unchanged report. A clean file gets a
/// full report with no items and no id, even when the client sends an id.
#[test]
fn document_pull_gives_a_clean_file_no_id() {
    let workspace = Workspace::new(&[
        ("src/broken.wgsl", b"fn broken( {"),
        ("src/clean.wgsl", b"fn clean() {}"),
    ]);

    let broken = workspace.pull_document("src/broken.wgsl", None);
    assert_eq!(broken["kind"], "full");
    let id = broken["resultId"].as_str().unwrap();
    assert_eq!(
        workspace.pull_document("src/broken.wgsl", Some(id)),
        json!({ "kind": "unchanged", "resultId": id })
    );
    assert_eq!(
        workspace.pull_document("src/clean.wgsl", Some(id)),
        json!({ "kind": "full", "items": [] })
    );
}

/// A file whose text is not in the database (here, because it is not UTF-8)
/// gets an empty report rather than a panic, as an excluded file does.
#[test]
fn document_pull_of_a_file_without_text_is_empty() {
    let workspace = Workspace::new(&[("src/not_utf8.wgsl", b"fn \xff( {")]);
    assert_eq!(
        workspace.pull_document("src/not_utf8.wgsl", None),
        json!({ "kind": "full", "items": [] })
    );
}

/// A pull still running when the main loop applies a change that adds a file is
/// cancelled, as on any change, rather than panicking on the new file.
/// Snapshots share the VFS live, so the pull sees the new file there while its
/// database predates the file.
#[test]
fn workspace_pull_running_while_a_file_is_added_is_cancelled() {
    let mut workspace = Workspace::new(&[("src/broken.wgsl", b"fn broken( {")]);
    let broken = workspace.file_id("src/broken.wgsl");
    let snapshot = workspace.state.snapshot();
    workspace.write_file("src/new.wgsl", b"fn new( {");

    let pull = thread::spawn(move || {
        // Wait until `process_changes` asks running queries to cancel. It then
        // waits for this snapshot to be dropped before it changes the database.
        let deadline = Instant::now() + Duration::from_secs(60);
        while snapshot.analysis.parse(broken).is_ok() {
            assert!(
                Instant::now() < deadline,
                "the change never cancelled the snapshot"
            );
            thread::yield_now();
        }
        handle_workspace_diagnostics(snapshot, WorkspaceDiagnosticParams::default())
    });
    workspace.state.process_changes();
    let result = pull.join().expect("the pull panicked");
    assert!(result.unwrap_err().is::<salsa::Cancelled>());
}

/// The empty fallback reports have no result id. The server sends them while
/// the VFS loads, for a file it does not check, and for a suspended pull it
/// gives up on.
#[test]
fn fallback_reports_have_no_result_id() {
    assert_eq!(
        serde_json::to_value(empty_diagnostic_report()).unwrap(),
        json!({ "kind": "full", "items": [] })
    );
    assert_eq!(
        serde_json::to_value(empty_workspace_diagnostics()).unwrap(),
        json!({ "items": [] })
    );
}

/// Without a `partialResultToken`, a pull sends no partial results and puts
/// every entry in the response. With one, the entry with new diagnostics is
/// sent as a partial result, and the unchanged and cleared entries go in the
/// response.
#[test]
fn workspace_pull_streams_new_diagnostics_only_with_a_partial_result_token() {
    let workspace = Workspace::new(&[
        ("src/a.wgsl", b"fn a( {"),
        ("src/b.wgsl", b"fn b( {"),
        ("src/clean.wgsl", b"fn clean() {}"),
    ]);
    let stale = workspace.previous_id("src/clean.wgsl", "id-from-before-the-fix");

    let bulk = workspace
        .pull_workspace(vec![stale.clone()])
        .unwrap_respond();
    assert_eq!(
        summary(&bulk),
        [
            "a.wgsl full id=true items=true",
            "b.wgsl full id=true items=true",
            "clean.wgsl full id=false items=false",
        ]
    );
    assert_eq!(workspace.partial_results(), []);

    let b_id = workspace.previous_id("src/b.wgsl", &full_result_id(&bulk, "b.wgsl"));
    let streamed = workspace
        .pull_workspace_streaming(vec![b_id, stale], FlushPolicy::Count(1))
        .unwrap_respond();
    let partial_results = workspace.partial_results();
    assert_eq!(
        partial_results.iter().map(summary).collect::<Vec<_>>(),
        [["a.wgsl full id=true items=true"]]
    );
    assert_eq!(
        summary(&streamed),
        ["b.wgsl unchanged", "clean.wgsl full id=false items=false"]
    );
}

/// The handler suspends a pull with nothing new for the client (long polling),
/// whether the workspace has no diagnostics or the client already holds them
/// all, and whether or not the pull streams. A changed file makes the same pull
/// answer.
#[test]
fn workspace_pull_with_nothing_new_is_suspended() {
    let mut workspace = Workspace::new(&[("src/clean.wgsl", b"fn clean() {}")]);
    assert_eq!(workspace.pull_workspace(Vec::new()), LongPoll::Suspend);

    workspace.set_file("src/broken.wgsl", b"fn broken( {");
    let first = workspace.pull_workspace(Vec::new()).unwrap_respond();
    let broken = workspace.previous_id("src/broken.wgsl", &full_result_id(&first, "broken.wgsl"));
    assert_eq!(
        workspace.pull_workspace(vec![broken.clone()]),
        LongPoll::Suspend
    );
    assert_eq!(
        workspace.pull_workspace_streaming(vec![broken.clone()], FlushPolicy::Count(1)),
        LongPoll::Suspend
    );
    assert_eq!(workspace.partial_results(), []);

    workspace.set_file("src/broken.wgsl", b"fn broken( { fn");
    let changed = workspace.pull_workspace(vec![broken]).unwrap_respond();
    assert_eq!(summary(&changed), ["broken.wgsl full id=true items=true"]);
}

/// A streaming pull that sent a chunk is answered even when the rest of its
/// entries are unchanged, or when it has no entries left for the response.
#[test]
fn workspace_pull_that_streamed_is_answered() {
    let workspace = Workspace::new(&[("src/a.wgsl", b"fn a( {"), ("src/b.wgsl", b"fn b( {")]);
    let all = workspace.pull_workspace(Vec::new()).unwrap_respond();
    let b_id = workspace.previous_id("src/b.wgsl", &full_result_id(&all, "b.wgsl"));

    let rest_unchanged = workspace
        .pull_workspace_streaming(vec![b_id], FlushPolicy::Count(1))
        .unwrap_respond();
    let partial_results = workspace.partial_results();
    assert_eq!(
        partial_results.iter().map(summary).collect::<Vec<_>>(),
        [["a.wgsl full id=true items=true"]]
    );
    assert_eq!(summary(&rest_unchanged), ["b.wgsl unchanged"]);

    let nothing_left = workspace
        .pull_workspace_streaming(Vec::new(), FlushPolicy::Count(1))
        .unwrap_respond();
    assert_eq!(workspace.partial_results().len(), 2);
    assert_eq!(nothing_left, WorkspaceDiagnosticReport::default());
}

/// Through the main loop, a pull with nothing new gets no answer until the
/// server's state changes. A change that leaves the diagnostics as they were
/// runs the pull again, and the pull is suspended again. A change to the
/// diagnostics answers it.
#[test]
fn suspended_workspace_pull_is_answered_after_a_change() {
    let mut workspace = Workspace::new(&[
        ("src/broken.wgsl", b"fn broken( {"),
        ("src/clean.wgsl", b"fn clean() {}"),
    ]);
    workspace.send_workspace_pull(1, Vec::new());
    workspace.finish_task();
    let first = workspace.workspace_response(1);
    let broken = workspace.previous_id("src/broken.wgsl", &full_result_id(&first, "broken.wgsl"));

    workspace.send_workspace_pull(2, vec![broken]);
    workspace.finish_task();
    assert_eq!(workspace.responses().len(), 0);

    workspace.change_file_on_disk("src/clean.wgsl", b"fn clean() { }");
    workspace.finish_task();
    assert_eq!(workspace.responses().len(), 0);

    workspace.change_file_on_disk("src/broken.wgsl", b"fn broken() {}");
    workspace.finish_task();
    assert_eq!(
        summary(&workspace.workspace_response(2)),
        ["broken.wgsl full id=false items=false"]
    );
}

/// A pull whose handler found nothing new, but whose result the main loop
/// receives only after a change, runs again at once instead of being
/// suspended. Its "nothing new" predates the change, and the change found no
/// suspended pull to wake.
#[test]
fn workspace_pull_that_finished_before_a_change_runs_again() {
    let mut workspace = Workspace::new(&[("src/broken.wgsl", b"fn broken( {")]);
    workspace.send_workspace_pull(1, Vec::new());
    workspace.finish_task();
    let first = workspace.workspace_response(1);
    let broken = workspace.previous_id("src/broken.wgsl", &full_result_id(&first, "broken.wgsl"));

    workspace.send_workspace_pull(2, vec![broken]);
    let suspended = workspace.next_task();
    assert!(matches!(suspended, Task::Suspend(_)));
    workspace.change_file_on_disk("src/broken.wgsl", b"fn broken() {}");
    workspace.state.handle_event(Event::Task(suspended));
    assert_eq!(
        summary(&workspace.workspace_response_to_rerun(2)),
        ["broken.wgsl full id=false items=false"]
    );
}

/// A configuration change runs a suspended pull again: turning semantic errors
/// off clears a file whose only diagnostic was one.
#[test]
fn suspended_workspace_pull_is_answered_after_a_configuration_change() {
    let mut workspace = Workspace::new(&[("src/unresolved.wesl", b"fn f() { let x = y; }")]);
    workspace.send_workspace_pull(1, Vec::new());
    workspace.finish_task();
    let first = workspace.workspace_response(1);
    let unresolved = workspace.previous_id(
        "src/unresolved.wesl",
        &full_result_id(&first, "unresolved.wesl"),
    );

    workspace.send_workspace_pull(2, vec![unresolved]);
    workspace.finish_task();
    assert_eq!(workspace.responses().len(), 0);

    workspace.change_configuration(json!({ "diagnostics": { "semanticErrors": false } }));
    workspace.finish_task();
    assert_eq!(
        summary(&workspace.workspace_response(2)),
        ["unresolved.wesl full id=false items=false"]
    );
}

/// A pull cancelled while suspended, or before the main loop receives its
/// handler's result, is answered with `RequestCancelled` and is not held for a
/// later run.
#[test]
fn suspended_workspace_pull_is_answered_on_cancel() {
    let mut workspace = Workspace::new(&[("src/clean.wgsl", b"fn clean() {}")]);

    workspace.send_workspace_pull(1, Vec::new());
    workspace.finish_task();
    assert_eq!(workspace.responses().len(), 0);
    workspace.cancel(1);
    workspace.assert_cancelled(1);
    assert!(workspace.state.suspended_request.give_up().is_none());

    workspace.send_workspace_pull(2, Vec::new());
    workspace.cancel(2);
    workspace.assert_cancelled(2);
    workspace.finish_task();
    assert_eq!(workspace.responses().len(), 0);
    assert!(workspace.state.suspended_request.give_up().is_none());
}

/// A shutdown answers a suspended pull with an empty report. A pull whose
/// suspension the main loop receives after the shutdown is answered the same
/// way.
#[test]
fn suspended_workspace_pull_is_answered_on_shutdown() {
    let mut workspace = Workspace::new(&[("src/clean.wgsl", b"fn clean() {}")]);
    workspace.send_workspace_pull(1, Vec::new());
    workspace.finish_task();
    assert_eq!(workspace.responses().len(), 0);
    // The main loop receives this pull's result only after the shutdown.
    workspace.send_workspace_pull(2, Vec::new());
    workspace.shutdown(3);
    let responses = workspace.responses();
    let [suspended, shutdown] = responses.as_slice() else {
        panic!("expected two responses, got {responses:#?}");
    };
    assert_eq!(suspended.id, RequestId::from(1));
    assert_eq!(
        suspended.response_result.as_ref().unwrap(),
        &json!({ "items": [] })
    );
    assert_eq!(shutdown.id, RequestId::from(3));

    workspace.finish_task();
    let late = workspace.workspace_response(2);
    assert_eq!(late, WorkspaceDiagnosticReport::default());
    assert!(workspace.state.suspended_request.give_up().is_none());
}

/// A second pull suspended while one is suspended takes its place, and the
/// first is answered with an empty report so that the client is not left
/// waiting for it. The next change answers the second.
#[test]
fn a_second_suspended_workspace_pull_answers_the_first() {
    let mut workspace = Workspace::new(&[("src/clean.wgsl", b"fn clean() {}")]);
    workspace.send_workspace_pull(1, Vec::new());
    workspace.finish_task();
    assert_eq!(workspace.responses().len(), 0);

    workspace.send_workspace_pull(2, Vec::new());
    workspace.finish_task();
    assert_eq!(
        workspace.workspace_response(1),
        WorkspaceDiagnosticReport::default()
    );

    workspace.change_file_on_disk("src/clean.wgsl", b"fn clean( {");
    workspace.finish_task();
    assert_eq!(
        summary(&workspace.workspace_response(2)),
        ["clean.wgsl full id=true items=true"]
    );
}

/// A workspace pull that arrives while the VFS is loading is answered at once,
/// on the main thread, with an empty report rather than run or held.
#[test]
fn workspace_pull_while_the_vfs_loads_is_answered_with_an_empty_report() {
    let mut workspace = Workspace::new(&[("src/broken.wgsl", b"fn broken( {")]);
    workspace.state.vfs_done = false;
    workspace.send_workspace_pull(1, Vec::new());
    assert_eq!(
        workspace.workspace_response(1),
        WorkspaceDiagnosticReport::default()
    );
}
