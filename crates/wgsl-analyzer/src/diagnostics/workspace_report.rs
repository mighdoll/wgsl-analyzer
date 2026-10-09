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
//! file, a non-shader file).
//!
//! The builder is modeled on how ty, Astral's Python type checker, answers
//! workspace pulls. The comments below say which choices follow ty.

use lsp_types::{
    Diagnostic, FullDocumentDiagnosticReport, PreviousResultId, UnchangedDocumentDiagnosticReport,
    Uri, WorkspaceDiagnosticReport, WorkspaceDocumentDiagnosticReport,
    WorkspaceFullDocumentDiagnosticReport, WorkspaceUnchangedDocumentDiagnosticReport,
};
use rustc_hash::FxHashMap;
use vfs::FileId;

use super::result_id::result_id;

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
    /// Entries for the response.
    items: Vec<WorkspaceDocumentDiagnosticReport>,
}

impl WorkspaceReportBuilder {
    /// `resolve` maps a client URI to its file, or returns `None` when the file
    /// is unknown or excluded.
    pub(crate) fn new(
        previous_result_ids: Vec<PreviousResultId>,
        mut resolve: impl FnMut(&Uri) -> Option<FileId>,
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
        }
    }

    /// Records one checked file. `uri` is the server's URI for the file and
    /// `version` its open-document version, if it is open.
    pub(crate) fn add_file(
        &mut self,
        file_id: FileId,
        uri: Uri,
        version: Option<i32>,
        items: Vec<Diagnostic>,
    ) {
        let previous = self.previous.remove(&file_id);
        if let Some(item) = workspace_report_item(uri, version, items, previous) {
            self.items.push(item);
        }
    }

    /// Clears every previous id that no checked file claimed, under the URI the
    /// client sent, and returns the response.
    pub(crate) fn finish(self) -> WorkspaceDiagnosticReport {
        let mut items = self.items;
        let to_clear = self
            .previous
            .into_values()
            .map(|previous| previous.uri)
            .chain(self.unresolved);
        items.extend(to_clear.map(|uri| cleared_report(uri, None)));
        WorkspaceDiagnosticReport { items }
    }
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
    use lsp_types::{DiagnosticSeverity, Position, Range};

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
        );
        builder.add_file(checked, uri(SERVER_URI), None, items);
        let report = builder.finish();

        assert_eq!(
            sorted(report),
            vec![
                unchanged(SERVER_URI, None, id),
                full("file:///ws/deleted.wgsl", None, None, Vec::new()),
                full("file:///ws/skipped.wgsl", None, None, Vec::new()),
            ]
        );
    }
}
