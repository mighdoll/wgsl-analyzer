//! Result ids for pull diagnostics (`textDocument/diagnostic` and
//! `workspace/diagnostic`).
//!
//! A result id is a hash of the LSP diagnostics the server sends for one file,
//! and of whether the file is open in the editor (see [`result_id`] for why).
//! The client echoes the id back as `previousResultId` on its next pull, and when
//! the file's current diagnostics hash to the same id, the server answers with an
//! *unchanged* report instead of resending the items. The server keeps no
//! record of the ids it sent. It recomputes the id from the current diagnostics
//! on every pull.
//!
//! A file with no diagnostics has no result id. The client then stops sending an
//! id for it, which is what lets a workspace pull tell "this file was fixed"
//! (the client sent an id, there is none now) from "nothing changed".
//!
//! An id is a pure function of the items and the open flag, so an id from
//! another server process or another build is harmless. If the hasher or the
//! `Hash` impls changed in between, the id just fails to match and the client
//! gets one full report.

use std::hash::{DefaultHasher, Hash as _, Hasher as _};

use lsp_types::{
    Diagnostic, DocumentDiagnosticReport, FullDocumentDiagnosticReport,
    RelatedFullDocumentDiagnosticReport, RelatedUnchangedDocumentDiagnosticReport,
    UnchangedDocumentDiagnosticReport,
};

/// Fingerprints the diagnostics the client will see for one file. `open` says
/// whether the file is open in the editor. Returns `None` for an empty list,
/// open or not, since a clean file has no id (see the module docs).
///
/// Callers pass the converted LSP items rather than the `ide` diagnostics, so the
/// id covers exactly what the client receives: ranges after line-index
/// conversion, related information, and tags. The hash is order-sensitive, so
/// the same items in a different order get a new id (and so a full report).
///
/// The open flag is hashed too, so that an id the client got while the file was
/// open never matches once the file is closed. While a file is open, VS Code
/// sends the document pull's id in workspace pulls. Once the file closes, it
/// sends the id from its last workspace pull instead, and that id can stand for
/// diagnostics VS Code no longer shows. Say the user fixed an error, so a
/// document pull cleared it, then closed the file without saving, so the error
/// is back on disk. If that last workspace pull ran while the file was open
/// (opening a file wakes a suspended pull), its id differs from the closed
/// file's. The next pull then answers *full* rather than *unchanged*, and the
/// error reappears. The cost is one redundant full report each time a file
/// with diagnostics closes.
///
/// `DefaultHasher::new()` is used rather than `FxHasher` because a collision here
/// leaves stale diagnostics on the client, so fingerprint quality matters more
/// than speed. Its keys are fixed, so the id is deterministic.
pub(crate) fn result_id(
    items: &[Diagnostic],
    open: bool,
) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    items.hash(&mut hasher);
    open.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

/// Builds the `textDocument/diagnostic` response for one file's diagnostics.
///
/// When `previous_result_id` (the id the client got from its last pull) matches
/// the result id of `items`, the report is *unchanged* and the client keeps the
/// diagnostics it already has. Otherwise the report is *full* and includes the
/// new id. A clean file always gets a full report with no items and no id, so the
/// client clears what it holds and sends no id next time.
///
/// The id is computed as for an open file (see [`result_id`]), since a client
/// pulls a document while it is open. A workspace pull of the open file computes
/// the same id. That matters because VS Code sends the document pull's id in its
/// workspace pulls while the file is open.
pub(crate) fn document_diagnostic_report(
    items: Vec<Diagnostic>,
    previous_result_id: Option<&str>,
) -> DocumentDiagnosticReport {
    match result_id(&items, true) {
        Some(result_id) if previous_result_id == Some(result_id.as_str()) => {
            RelatedUnchangedDocumentDiagnosticReport {
                related_documents: None,
                unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                    result_id,
                },
            }
            .into()
        },
        result_id => RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport { result_id, items },
        }
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use std::slice;

    use lsp_types::{
        Code, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag, Location, Position,
        Range, Uri,
    };

    use super::*;

    fn diagnostic(
        message: &str,
        line: u32,
    ) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 0), Position::new(line, 4)),
            severity: Some(DiagnosticSeverity::Error),
            message: message.into(),
            ..Diagnostic::default()
        }
    }

    fn full_report(
        result_id: Option<String>,
        items: Vec<Diagnostic>,
    ) -> DocumentDiagnosticReport {
        RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport { result_id, items },
        }
        .into()
    }

    fn unchanged_report(result_id: String) -> DocumentDiagnosticReport {
        RelatedUnchangedDocumentDiagnosticReport {
            related_documents: None,
            unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport { result_id },
        }
        .into()
    }

    #[test]
    fn result_id_is_deterministic() {
        let first = [
            diagnostic("unknown type", 1),
            diagnostic("unused variable", 2),
        ];
        let second = [
            diagnostic("unknown type", 1),
            diagnostic("unused variable", 2),
        ];
        assert_eq!(result_id(&first, false), result_id(&second, false));
    }

    #[test]
    fn result_id_is_none_without_diagnostics() {
        assert_eq!(result_id(&[], false), None);
        assert_eq!(result_id(&[], true), None);
    }

    /// The same diagnostics get one id while the file is open and another once
    /// it is closed, so that an id from while it was open does not match after.
    #[test]
    fn result_id_depends_on_whether_the_file_is_open() {
        let items = [diagnostic("unknown type", 1)];
        assert_ne!(result_id(&items, true), result_id(&items, false));
    }

    #[test]
    fn result_id_is_order_sensitive() {
        let unknown = diagnostic("unknown type", 1);
        let unused = diagnostic("unused variable", 2);
        assert_ne!(
            result_id(&[unknown.clone(), unused.clone()], false),
            result_id(&[unused, unknown], false)
        );
    }

    #[test]
    fn result_id_changes_with_each_field_the_client_sees() {
        let base = diagnostic("unknown type", 1);
        let base_id = result_id(slice::from_ref(&base), false);

        let mut message = base.clone();
        message.message = "unknown function".into();
        let mut range = base.clone();
        range.range.end.character = 5;
        let mut severity = base.clone();
        severity.severity = Some(DiagnosticSeverity::Warning);
        let mut code = base.clone();
        code.code = Some(Code::String("E0002".to_owned()));
        let mut source = base.clone();
        source.source = Some("naga".to_owned());
        let mut related = base.clone();
        related.related_information = Some(vec![DiagnosticRelatedInformation {
            location: Location {
                uri: Uri::parse("file:///lib.wgsl").unwrap(),
                range: base.range,
            },
            message: "declared here".to_owned(),
        }]);
        let mut tags = base;
        tags.tags = Some(vec![DiagnosticTag::Unnecessary]);

        for (field, changed) in [
            ("message", message),
            ("range", range),
            ("severity", severity),
            ("code", code),
            ("source", source),
            ("related information", related),
            ("tags", tags),
        ] {
            assert_ne!(
                result_id(slice::from_ref(&changed), false),
                base_id,
                "changing the {field} should change the result id"
            );
        }
    }

    #[test]
    fn report_is_full_without_a_previous_id() {
        let items = vec![diagnostic("unknown type", 1)];
        let id = result_id(&items, true).unwrap();
        assert_eq!(
            document_diagnostic_report(items.clone(), None),
            full_report(Some(id), items)
        );
    }

    #[test]
    fn report_is_unchanged_when_the_previous_id_matches() {
        let items = vec![diagnostic("unknown type", 1)];
        let id = result_id(&items, true).unwrap();
        assert_eq!(
            document_diagnostic_report(items, Some(&id)),
            unchanged_report(id)
        );
    }

    #[test]
    fn report_is_full_when_the_previous_id_is_stale() {
        let stale_id = result_id(&[diagnostic("unknown type", 1)], true).unwrap();
        let items = vec![diagnostic("unknown type", 2)];
        let id = result_id(&items, true).unwrap();
        assert_eq!(
            document_diagnostic_report(items.clone(), Some(&stale_id)),
            full_report(Some(id), items)
        );
    }

    /// A clean file never gets an id or an unchanged report, whatever the client
    /// sends: an empty full report clears the client's state for the file.
    #[test]
    fn report_for_a_clean_file_is_full_without_an_id() {
        let fixed_id = result_id(&[diagnostic("unknown type", 1)], true).unwrap();
        for previous_result_id in [None, Some(fixed_id.as_str())] {
            assert_eq!(
                document_diagnostic_report(Vec::new(), previous_result_id),
                full_report(None, Vec::new())
            );
        }
    }
}
