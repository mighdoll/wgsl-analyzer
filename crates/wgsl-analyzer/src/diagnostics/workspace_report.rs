//! The `workspace/diagnostic` response, built one checked file at a time.
//!
//! The client sends `previousResultIds`, the result id it holds for each file it
//! got diagnostics for, from earlier workspace pulls or from document pulls (both
//! use [`result_id`], so for an open file the two pulls give the same id). For
//! each file the server checks, [`workspace_report_item`] decides what the
//! client needs:
//!
//! - diagnostics whose id matches the previous id: *unchanged*;
//! - diagnostics otherwise: *full*, with the new id;
//! - no diagnostics, but the client sent an id: *cleared*, a full report with no
//!   items and no id, so the client drops the stale diagnostics and stops sending
//!   an id for the file;
//! - no diagnostics and no previous id: nothing.
//!
//! [`WorkspaceReportBuilder::finish`] then clears every previous id that no
//! checked file claimed. An id goes unclaimed when its URI is unknown or
//! excluded, or when it names a file the pull does not check (a closed library
//! file, a non-shader file). The exception is an open file that the pull skips
//! (a non-shader file, or one with diagnostics disabled), which keeps its id
//! until the file closes (`finish` says why). The exception is rare, since the
//! pull checks every open shader file.
//!
//! A client that sends no `partialResultToken` gets every entry in the response
//! (a bulk pull). A client that sends one (VS Code always does) gets the *full*
//! entries with new diagnostics as partial results while the pull runs, so it
//! can show them before the whole workspace is checked (see [`Streaming`]).
//! Unchanged and cleared entries, and any full entries not yet sent, go in the
//! response.
//!
//! A pull with nothing new for the client (every entry unchanged, or no entries
//! at all, and no partial result sent) is not answered. The server suspends it
//! and runs it again once the server's state changes (long polling, see
//! [`crate::long_poll`]).
//!
//! The builder is modeled on how ty, Astral's Python type checker, answers
//! workspace pulls. The comments below say which choices follow ty.

use std::{
    mem,
    time::{Duration, Instant},
};

use lsp_types::{
    Diagnostic, FullDocumentDiagnosticReport, PreviousResultId, UnchangedDocumentDiagnosticReport,
    Uri, WorkspaceDiagnosticReport, WorkspaceDiagnosticReportPartialResult,
    WorkspaceDiagnosticRequest, WorkspaceDocumentDiagnosticReport,
    WorkspaceFullDocumentDiagnosticReport, WorkspaceUnchangedDocumentDiagnosticReport,
};
use rustc_hash::FxHashMap;
use vfs::FileId;

use super::result_id::result_id;
use crate::{long_poll::LongPoll, lsp::utilities::PartialResultSender};

/// Builds one `workspace/diagnostic` response from the client's previous ids and
/// each checked file's diagnostics. Call [`Self::add_file`] once per checked
/// file, then [`Self::finish`].
pub(crate) struct WorkspaceReportBuilder {
    /// The client's previous ids, keyed by file rather than by URI because the
    /// server's URI for a file is not guaranteed to be spelled like the client's.
    /// If the client sends two spellings of one file, only the later entry is
    /// kept and the earlier one gets no answer, as in ty. VS Code normalizes URIs
    /// before sending them, so it does not send both.
    previous: FxHashMap<FileId, PreviousResultId>,
    /// URIs of previous ids that do not name a known, non-excluded file.
    unresolved: Vec<Uri>,
    /// Entries for the response: all of them in a bulk pull, and the unchanged
    /// and cleared ones in a streaming pull.
    items: Vec<WorkspaceDocumentDiagnosticReport>,
    /// Present when the client sent a `partialResultToken`.
    streaming: Option<Streaming>,
}

impl WorkspaceReportBuilder {
    /// `resolve` maps a client URI to its file, or returns `None` when the file
    /// is unknown or excluded. `streaming` is `None` for a bulk pull.
    pub(crate) fn new(
        previous_result_ids: Vec<PreviousResultId>,
        mut resolve: impl FnMut(&Uri) -> Option<FileId>,
        streaming: Option<Streaming>,
    ) -> Self {
        let mut previous = FxHashMap::default();
        let mut unresolved = Vec::new();
        for entry in previous_result_ids {
            match resolve(&entry.uri) {
                Some(file_id) => {
                    previous.insert(file_id, entry);
                },
                None => unresolved.push(entry.uri),
            }
        }
        Self {
            previous,
            unresolved,
            items: Vec::new(),
            streaming,
        }
    }

    /// Records one checked file, and in a streaming pull sends the buffered
    /// entries as a chunk if one is due. `uri` is the server's URI for the file
    /// and `version` its open-document version, if it is open.
    pub(crate) fn add_file(
        &mut self,
        file_id: FileId,
        uri: Uri,
        version: Option<i32>,
        items: Vec<Diagnostic>,
    ) {
        let previous = self.previous.remove(&file_id);
        if let Some(item) = workspace_report_item(uri, version, items, previous) {
            match &mut self.streaming {
                Some(streaming) if has_new_diagnostics(&item) => streaming.unsent.push(item),
                Some(_) | None => self.items.push(item),
            }
        }
        // Also after a file with nothing to report, so that an entry buffered
        // earlier is sent once the interval passes, not only with the next one.
        if let Some(streaming) = &mut self.streaming {
            streaming.maybe_flush();
        }
    }

    /// Clears every previous id that no checked file claimed, under the URI the
    /// client sent, and returns the response, or [`LongPoll::Suspend`] when the
    /// pull has nothing new for the client. `version` gives a file's
    /// open-document version, `None` when the file is not open.
    ///
    /// A previous id for an open file that the pull skips is not cleared until
    /// the file closes. The pull checks every open shader file, so this covers
    /// only an open file that is not WGSL or WESL, or one with diagnostics
    /// disabled. When VS Code pulls an open file as a document, it ignores
    /// workspace entries for the file and keeps sending the document pull's id.
    /// Clearing that id would put a cleared entry in every pull, so no pull would
    /// suspend, and the server would re-check the whole workspace every 2 s.
    /// Sending nothing for the file also leaves the id VS Code holds from
    /// workspace pulls in place, so once the file closes, the next pull (or a
    /// suspended pull, which the close wakes) clears its diagnostics.
    ///
    /// In a streaming pull the response holds what was not sent as a partial
    /// result: the unchanged and cleared entries and the full entries still
    /// buffered. The LSP spec's general rule is that a response after partial
    /// results has no result values. Like ty, `finish` departs from that rule.
    /// vscode-languageclient reports the response's entries the same way as a
    /// chunk's, so VS Code shows them.
    ///
    /// A pull suspends when every entry is unchanged (or there are none) and no
    /// partial result went out. A pull that sent one must be answered even if the
    /// rest is unchanged, because a suspended pull runs again against the
    /// `previousResultIds` it was sent with, which lack the ids the client took
    /// from the chunks. A streamed file fixed before the re-run would have no
    /// previous id there, so it would be omitted rather than cleared, and the
    /// client would keep showing its old diagnostics.
    pub(crate) fn finish(
        self,
        version: impl Fn(FileId) -> Option<i32>,
    ) -> LongPoll<WorkspaceDiagnosticReport> {
        let mut items = self.items;
        let mut sent_chunk = false;
        if let Some(streaming) = self.streaming {
            sent_chunk = streaming.sent_chunk;
            items.extend(streaming.unsent);
        }
        let to_clear = self
            .previous
            .into_iter()
            .filter(|&(file_id, _)| version(file_id).is_none())
            .map(|(_, previous)| previous.uri)
            .chain(self.unresolved);
        items.extend(to_clear.map(|uri| cleared_report(uri, None)));
        if !sent_chunk && items.iter().all(is_unchanged) {
            LongPoll::Suspend
        } else {
            LongPoll::Respond(WorkspaceDiagnosticReport { items })
        }
    }
}

/// When a streaming pull sends the full entries it has buffered.
#[derive(Clone, Copy, Debug)]
pub(crate) enum FlushPolicy {
    /// Once this long has passed since the previous chunk was sent, or since the
    /// pull began, so that a large workspace costs the client a chunk per
    /// interval rather than a notification per file.
    Interval(Duration),
    /// Once this many entries are buffered, so that tests stream without
    /// depending on the clock.
    #[cfg(test)]
    Count(usize),
}

impl FlushPolicy {
    /// The server sends at most one chunk every 50 ms, as ty does.
    pub(crate) const SERVER: Self = Self::Interval(Duration::from_millis(50));
}

/// Sends the full entries with new diagnostics to the client as partial results
/// while the pull runs, in chunks timed by a [`FlushPolicy`].
///
/// The spec asks for the first chunk to be a `WorkspaceDiagnosticReport` and
/// later ones to be `WorkspaceDiagnosticReportPartialResult`s. Both are
/// `{"items": [...]}` on the wire
/// (<https://github.com/microsoft/language-server-protocol/issues/2281>), so every
/// chunk is sent as the request's partial result type.
///
/// Unchanged entries never stream (nothing changed for the client to show), and
/// neither do cleared ones (as in ty). Both wait for the response.
pub(crate) struct Streaming {
    sender: PartialResultSender<WorkspaceDiagnosticRequest>,
    policy: FlushPolicy,
    last_flush: Instant,
    /// Full entries not yet sent.
    unsent: Vec<WorkspaceDocumentDiagnosticReport>,
    /// Whether a chunk has gone out, which rules out suspending the pull (see
    /// [`WorkspaceReportBuilder::finish`]).
    sent_chunk: bool,
}

impl Streaming {
    pub(crate) fn new(
        sender: PartialResultSender<WorkspaceDiagnosticRequest>,
        policy: FlushPolicy,
    ) -> Self {
        Self {
            sender,
            policy,
            last_flush: Instant::now(),
            unsent: Vec::new(),
            sent_chunk: false,
        }
    }

    /// Sends the buffered entries as one chunk if the policy says it is time.
    fn maybe_flush(&mut self) {
        if self.unsent.is_empty() {
            return;
        }
        let due = match self.policy {
            FlushPolicy::Interval(interval) => self.last_flush.elapsed() >= interval,
            #[cfg(test)]
            FlushPolicy::Count(count) => self.unsent.len() >= count,
        };
        if due {
            let items = mem::take(&mut self.unsent);
            self.sender
                .send(WorkspaceDiagnosticReportPartialResult { items });
            self.last_flush = Instant::now();
            self.sent_chunk = true;
        }
    }
}

/// Whether `item` carries new diagnostics: a full report with a result id. A
/// cleared entry is a full report with no id.
const fn has_new_diagnostics(item: &WorkspaceDocumentDiagnosticReport) -> bool {
    match item {
        WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(full) => {
            full.full_document_diagnostic_report.result_id.is_some()
        },
        WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(_) => false,
    }
}

const fn is_unchanged(item: &WorkspaceDocumentDiagnosticReport) -> bool {
    matches!(
        item,
        WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(_)
    )
}

/// Decides the report for one checked file, given the client's previous id for it.
/// `version` is set when the file is open, and the result id hashes whether it
/// is (see [`result_id`]).
///
/// A cleared entry uses the client's URI from `previous` rather than `uri`, so
/// that it clears the diagnostics the client holds under the URI it sent.
fn workspace_report_item(
    uri: Uri,
    version: Option<i32>,
    items: Vec<Diagnostic>,
    previous: Option<PreviousResultId>,
) -> Option<WorkspaceDocumentDiagnosticReport> {
    match (result_id(&items, version.is_some()), previous) {
        (Some(result_id), Some(previous)) if previous.value == result_id => Some(
            WorkspaceUnchangedDocumentDiagnosticReport {
                uri,
                version,
                unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                    result_id,
                },
            }
            .into(),
        ),
        (Some(result_id), _) => Some(
            WorkspaceFullDocumentDiagnosticReport {
                uri,
                version,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: Some(result_id),
                    items,
                },
            }
            .into(),
        ),
        (None, Some(previous)) => Some(cleared_report(previous.uri, version)),
        (None, None) => None,
    }
}

/// A full report with no items and no id: the client clears the file's
/// diagnostics and sends no id for it on the next pull.
fn cleared_report(
    uri: Uri,
    version: Option<i32>,
) -> WorkspaceDocumentDiagnosticReport {
    WorkspaceFullDocumentDiagnosticReport {
        uri,
        version,
        full_document_diagnostic_report: FullDocumentDiagnosticReport::default(),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use crossbeam_channel::Receiver;
    use lsp_types::{DiagnosticSeverity, Position, ProgressToken, Range};
    use serde_json::json;

    use super::*;

    fn diagnostic(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(1, 0), Position::new(1, 4)),
            severity: Some(DiagnosticSeverity::Error),
            message: message.into(),
            ..Diagnostic::default()
        }
    }

    fn uri(text: &str) -> Uri {
        Uri::parse(text).unwrap()
    }

    fn previous(
        uri_text: &str,
        value: &str,
    ) -> PreviousResultId {
        PreviousResultId {
            uri: uri(uri_text),
            value: value.to_owned(),
        }
    }

    fn full(
        uri_text: &str,
        version: Option<i32>,
        result_id: Option<String>,
        items: Vec<Diagnostic>,
    ) -> WorkspaceDocumentDiagnosticReport {
        WorkspaceFullDocumentDiagnosticReport {
            uri: uri(uri_text),
            version,
            full_document_diagnostic_report: FullDocumentDiagnosticReport { result_id, items },
        }
        .into()
    }

    fn unchanged(
        uri_text: &str,
        version: Option<i32>,
        result_id: String,
    ) -> WorkspaceDocumentDiagnosticReport {
        WorkspaceUnchangedDocumentDiagnosticReport {
            uri: uri(uri_text),
            version,
            unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport { result_id },
        }
        .into()
    }

    /// The report's items in URI order, since cleared entries come out of a hash map.
    fn sorted(report: WorkspaceDiagnosticReport) -> Vec<WorkspaceDocumentDiagnosticReport> {
        let mut items = report.items;
        items.sort_by(|left, right| item_uri(left).cmp(item_uri(right)));
        items
    }

    fn item_uri(item: &WorkspaceDocumentDiagnosticReport) -> &str {
        match item {
            WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(full) => {
                full.uri.as_str()
            },
            WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(
                unchanged,
            ) => unchanged.uri.as_str(),
        }
    }

    const SERVER_URI: &str = "file:///c:/ws/a.wgsl";
    /// The same file as [`SERVER_URI`], spelled the way a client might.
    const CLIENT_URI: &str = "file:///C%3A/ws/a.wgsl";

    const A_WGSL: &str = "file:///ws/a.wgsl";
    const B_WGSL: &str = "file:///ws/b.wgsl";
    const C_WGSL: &str = "file:///ws/c.wgsl";
    const D_WGSL: &str = "file:///ws/d.wgsl";
    const E_WGSL: &str = "file:///ws/e.wgsl";

    #[test]
    fn diagnostics_without_a_previous_id_are_full() {
        let items = vec![diagnostic("unknown type")];
        let id = result_id(&items, true).unwrap();
        assert_eq!(
            workspace_report_item(uri(SERVER_URI), Some(3), items.clone(), None),
            Some(full(SERVER_URI, Some(3), Some(id), items))
        );
    }

    #[test]
    fn diagnostics_matching_the_previous_id_are_unchanged() {
        let items = vec![diagnostic("unknown type")];
        let id = result_id(&items, true).unwrap();
        assert_eq!(
            workspace_report_item(
                uri(SERVER_URI),
                Some(3),
                items,
                Some(previous(CLIENT_URI, &id))
            ),
            Some(unchanged(SERVER_URI, Some(3), id))
        );
    }

    #[test]
    fn diagnostics_with_a_stale_previous_id_are_full() {
        let stale_id = result_id(&[diagnostic("unknown type")], false).unwrap();
        let items = vec![diagnostic("unknown function")];
        let id = result_id(&items, false).unwrap();
        assert_eq!(
            workspace_report_item(
                uri(SERVER_URI),
                None,
                items.clone(),
                Some(previous(CLIENT_URI, &stale_id))
            ),
            Some(full(SERVER_URI, None, Some(id), items))
        );
    }

    #[test]
    fn a_fixed_file_is_cleared_under_the_clients_uri() {
        let fixed_id = result_id(&[diagnostic("unknown type")], true).unwrap();
        assert_eq!(
            workspace_report_item(
                uri(SERVER_URI),
                Some(7),
                Vec::new(),
                Some(previous(CLIENT_URI, &fixed_id))
            ),
            Some(full(CLIENT_URI, Some(7), None, Vec::new()))
        );
    }

    #[test]
    fn a_clean_file_without_a_previous_id_is_omitted() {
        assert_eq!(
            workspace_report_item(uri(SERVER_URI), Some(3), Vec::new(), None),
            None
        );
    }

    /// The id the client got while the file was open does not match once the
    /// file is closed, so the closed file's diagnostics are sent in full.
    #[test]
    fn an_id_from_while_the_file_was_open_is_stale_once_it_is_closed() {
        let items = vec![diagnostic("unknown type")];
        let open_id = result_id(&items, true).unwrap();
        let closed_id = result_id(&items, false).unwrap();
        assert_eq!(
            workspace_report_item(
                uri(SERVER_URI),
                None,
                items.clone(),
                Some(previous(CLIENT_URI, &open_id))
            ),
            Some(full(SERVER_URI, None, Some(closed_id), items))
        );
    }

    /// Previous ids that no checked file claims are cleared under the client's
    /// URI: one for a file the pull skipped and one that names no known file. A
    /// claimed id is answered once, by its file.
    #[test]
    fn unclaimed_previous_ids_are_cleared_under_the_clients_uri() {
        let checked = FileId::from_raw(0);
        let skipped = FileId::from_raw(1);
        let items = vec![diagnostic("unknown type")];
        let id = result_id(&items, false).unwrap();

        let resolve = |uri: &Uri| match uri.as_str() {
            CLIENT_URI => Some(checked),
            "file:///ws/skipped.wgsl" => Some(skipped),
            _ => None,
        };
        let mut builder = WorkspaceReportBuilder::new(
            vec![
                previous(CLIENT_URI, &id),
                previous("file:///ws/skipped.wgsl", "skipped-id"),
                previous("file:///ws/deleted.wgsl", "deleted-id"),
            ],
            resolve,
            None,
        );
        builder.add_file(checked, uri(SERVER_URI), None, items);
        let report = builder.finish(|_| None).unwrap_respond();

        assert_eq!(
            sorted(report),
            vec![
                unchanged(SERVER_URI, None, id),
                full("file:///ws/deleted.wgsl", None, None, Vec::new()),
                full("file:///ws/skipped.wgsl", None, None, Vec::new()),
            ]
        );
    }

    /// A previous id for an open file the pull does not check (here a manifest)
    /// is left alone. It does not stop a pull with nothing else new from
    /// suspending, and a pull answered for another file does not clear it. Once
    /// the file is closed, the id is cleared.
    #[test]
    fn an_open_file_the_pull_does_not_check_keeps_its_previous_id() {
        const MANIFEST: &str = "file:///ws/Cargo.toml";
        let checked = FileId::from_raw(0);
        let manifest = FileId::from_raw(1);
        let items = vec![diagnostic("unknown type")];
        let id = result_id(&items, false).unwrap();
        let resolve = |uri: &Uri| match uri.as_str() {
            A_WGSL => Some(checked),
            MANIFEST => Some(manifest),
            _ => None,
        };
        let manifest_is_open = |file_id: FileId| (file_id == manifest).then_some(4);
        let pull = |checked_items: Vec<Diagnostic>| {
            let mut builder = WorkspaceReportBuilder::new(
                vec![previous(A_WGSL, &id), previous(MANIFEST, "manifest-id")],
                resolve,
                None,
            );
            builder.add_file(checked, uri(A_WGSL), None, checked_items);
            builder
        };

        assert_eq!(
            pull(items.clone()).finish(manifest_is_open),
            LongPoll::Suspend
        );
        assert_eq!(
            pull(Vec::new()).finish(manifest_is_open),
            LongPoll::Respond(WorkspaceDiagnosticReport {
                items: vec![full(A_WGSL, None, None, Vec::new())]
            })
        );
        assert_eq!(
            sorted(pull(items).finish(|_| None).unwrap_respond()),
            vec![
                full(MANIFEST, None, None, Vec::new()),
                unchanged(A_WGSL, None, id),
            ]
        );
    }

    /// A streaming mode whose chunks land in the returned receiver.
    fn streaming(policy: FlushPolicy) -> (Streaming, Receiver<lsp_server::Message>) {
        let (sender, client) = crossbeam_channel::unbounded();
        let sender = PartialResultSender::new(sender, ProgressToken::from("token"));
        (Streaming::new(sender, policy), client)
    }

    /// The chunks sent so far, as the `value`s of `$/progress` notifications
    /// under the client's token.
    fn chunks(client: &Receiver<lsp_server::Message>) -> Vec<serde_json::Value> {
        client
            .try_iter()
            .map(|message| {
                let message = serde_json::to_value(message).unwrap();
                assert_eq!(message["method"], "$/progress");
                assert_eq!(message["params"]["token"], "token");
                message["params"]["value"].clone()
            })
            .collect()
    }

    fn chunk(items: &[WorkspaceDocumentDiagnosticReport]) -> serde_json::Value {
        json!({ "items": items })
    }

    /// Full entries stream once two are buffered. The unchanged entry, the
    /// cleared entries (a fixed file, and an id that names no known file) and the
    /// full entry still buffered at the end go in the response.
    #[test]
    fn streaming_sends_new_diagnostics_in_chunks_and_holds_the_rest() {
        let items = vec![diagnostic("unknown type")];
        let id = result_id(&items, false).unwrap();
        let open_id = result_id(&items, true).unwrap();
        let stale_id = result_id(&[diagnostic("unknown function")], false).unwrap();
        let file = FileId::from_raw;
        let resolve = |uri: &Uri| match uri.as_str() {
            B_WGSL => Some(file(1)),
            C_WGSL => Some(file(2)),
            D_WGSL => Some(file(3)),
            _ => None,
        };
        let (streaming, client) = streaming(FlushPolicy::Count(2));
        let mut builder = WorkspaceReportBuilder::new(
            vec![
                previous(B_WGSL, &id),
                previous(C_WGSL, &stale_id),
                previous(D_WGSL, &id),
                previous("file:///ws/deleted.wgsl", "deleted-id"),
            ],
            resolve,
            Some(streaming),
        );

        builder.add_file(file(0), uri(A_WGSL), None, items.clone());
        builder.add_file(file(1), uri(B_WGSL), None, items.clone());
        assert_eq!(chunks(&client), Vec::<serde_json::Value>::new());

        builder.add_file(file(2), uri(C_WGSL), Some(2), items.clone());
        assert_eq!(
            chunks(&client),
            [chunk(&[
                full(A_WGSL, None, Some(id.clone()), items.clone()),
                full(C_WGSL, Some(2), Some(open_id), items.clone()),
            ])]
        );

        builder.add_file(file(3), uri(D_WGSL), Some(5), Vec::new());
        builder.add_file(file(4), uri(E_WGSL), None, items.clone());
        let report = builder.finish(|_| None).unwrap_respond();
        assert_eq!(chunks(&client), Vec::<serde_json::Value>::new());
        assert_eq!(
            sorted(report),
            vec![
                unchanged(B_WGSL, None, id.clone()),
                full(D_WGSL, Some(5), None, Vec::new()),
                full("file:///ws/deleted.wgsl", None, None, Vec::new()),
                full(E_WGSL, None, Some(id), items),
            ]
        );
    }

    /// The spec wants the first chunk to be a `WorkspaceDiagnosticReport` and
    /// the later ones `WorkspaceDiagnosticReportPartialResult`s. Both are
    /// `{"items": [...]}`, so this checks that each chunk parses as the type the
    /// spec names for its position. Everything streamed, so the response is empty
    /// (and still sent).
    #[test]
    fn chunks_parse_as_the_report_types_the_spec_names() {
        let items = vec![diagnostic("unknown type")];
        let id = result_id(&items, false).unwrap();
        let (streaming, client) = streaming(FlushPolicy::Count(1));
        let mut builder = WorkspaceReportBuilder::new(Vec::new(), |_| None, Some(streaming));
        builder.add_file(FileId::from_raw(0), uri(A_WGSL), None, items.clone());
        builder.add_file(FileId::from_raw(1), uri(B_WGSL), None, items.clone());
        assert_eq!(
            builder.finish(|_| None),
            LongPoll::Respond(WorkspaceDiagnosticReport::default())
        );

        let chunks = chunks(&client);
        let [first, second] = chunks.as_slice() else {
            panic!("expected two chunks, got {chunks:#?}");
        };
        let first: WorkspaceDiagnosticReport = serde_json::from_value(first.clone()).unwrap();
        assert_eq!(
            first.items,
            [full(A_WGSL, None, Some(id.clone()), items.clone())]
        );
        let second: WorkspaceDiagnosticReportPartialResult =
            serde_json::from_value(second.clone()).unwrap();
        assert_eq!(second.items, [full(B_WGSL, None, Some(id), items)]);
    }

    /// The server's policy holds full entries until the interval has passed, then
    /// sends them on the next file checked, even a file with nothing to report.
    #[test]
    fn the_interval_policy_sends_once_the_interval_has_passed() {
        let items = vec![diagnostic("unknown type")];
        let id = result_id(&items, false).unwrap();
        let (streaming, client) = streaming(FlushPolicy::Interval(Duration::from_secs(3600)));
        let mut builder = WorkspaceReportBuilder::new(Vec::new(), |_| None, Some(streaming));
        builder.add_file(FileId::from_raw(0), uri(A_WGSL), None, items.clone());
        assert_eq!(chunks(&client), Vec::<serde_json::Value>::new());

        builder.streaming.as_mut().unwrap().policy = FlushPolicy::Interval(Duration::ZERO);
        builder.add_file(FileId::from_raw(1), uri(B_WGSL), None, Vec::new());
        assert_eq!(
            chunks(&client),
            [chunk(&[full(A_WGSL, None, Some(id), items)])]
        );
        assert_eq!(
            builder.finish(|_| None),
            LongPoll::Respond(WorkspaceDiagnosticReport::default())
        );
    }

    /// A pull whose entries are all unchanged, or that has none, suspends
    /// (long polling), streaming or not. A cleared entry changes what the client
    /// shows, so a pull with one is answered.
    #[test]
    fn a_pull_with_nothing_new_suspends() {
        let items = vec![diagnostic("unknown type")];
        let id = result_id(&items, false).unwrap();
        let resolve = |uri: &Uri| (uri.as_str() == A_WGSL).then_some(FileId::from_raw(0));

        let mut bulk = WorkspaceReportBuilder::new(vec![previous(A_WGSL, &id)], resolve, None);
        bulk.add_file(FileId::from_raw(0), uri(A_WGSL), None, items.clone());
        bulk.add_file(FileId::from_raw(1), uri(B_WGSL), None, Vec::new());
        assert_eq!(bulk.finish(|_| None), LongPoll::Suspend);

        let (streaming, client) = streaming(FlushPolicy::Count(1));
        let mut streamed =
            WorkspaceReportBuilder::new(vec![previous(A_WGSL, &id)], resolve, Some(streaming));
        streamed.add_file(FileId::from_raw(0), uri(A_WGSL), None, items);
        assert_eq!(streamed.finish(|_| None), LongPoll::Suspend);
        assert_eq!(chunks(&client), Vec::<serde_json::Value>::new());

        let empty = WorkspaceReportBuilder::new(Vec::new(), |_| None, None);
        assert_eq!(empty.finish(|_| None), LongPoll::Suspend);

        let cleared =
            WorkspaceReportBuilder::new(vec![previous(B_WGSL, "deleted-id")], |_| None, None);
        assert_eq!(
            cleared.finish(|_| None),
            LongPoll::Respond(WorkspaceDiagnosticReport {
                items: vec![full(B_WGSL, None, None, Vec::new())]
            })
        );
    }

    /// A pull that sent a chunk is answered even though the rest is unchanged.
    /// Suspended, it would run again against the previous ids it was sent with,
    /// which lack the ids the client took from the chunk.
    #[test]
    fn a_pull_that_sent_a_chunk_is_answered() {
        let items = vec![diagnostic("unknown type")];
        let id = result_id(&items, false).unwrap();
        let resolve = |uri: &Uri| (uri.as_str() == B_WGSL).then_some(FileId::from_raw(1));
        let (streaming, client) = streaming(FlushPolicy::Count(1));
        let mut builder =
            WorkspaceReportBuilder::new(vec![previous(B_WGSL, &id)], resolve, Some(streaming));
        builder.add_file(FileId::from_raw(0), uri(A_WGSL), None, items.clone());
        builder.add_file(FileId::from_raw(1), uri(B_WGSL), None, items.clone());

        assert_eq!(
            chunks(&client),
            [chunk(&[full(A_WGSL, None, Some(id.clone()), items)])]
        );
        assert_eq!(
            builder.finish(|_| None),
            LongPoll::Respond(WorkspaceDiagnosticReport {
                items: vec![unchanged(B_WGSL, None, id)]
            })
        );
    }
}
