//! Eligibility: decide from the JS snapshot, before any network, whether an item is worth a lookup.
//! JS re-checks right before saving (the user may have added a PDF meanwhile; `OpErrKind::AlreadyHasPdf`).
use fulltext_core::model::*;

/// `Some(reason)` when the item must be skipped. Order matters (first match wins): not a regular item, feed
/// item, read-only library, already has a PDF attachment, has a PDF attachment that is only synced (not
/// downloaded) - the last two share `HasPdf` / `PdfNotDownloaded` so the user is told to download, not to search.
pub(crate) fn eligibility(snap: &ItemSnapshot) -> Option<SkipReason> {
    if !snap.regular {
        return Some(SkipReason::NotRegular);
    }
    if snap.feed || snap.library.kind == LibraryKind::Feed {
        return Some(SkipReason::Feed);
    }
    if !snap.library.files_editable {
        return Some(SkipReason::ReadOnly);
    }
    if snap.best_attachment.kind == AttachKind::Pdf && snap.best_attachment.exists == Some(false) {
        return Some(SkipReason::PdfNotDownloaded);
    }
    if snap.pdf_attachments > 0 {
        return Some(SkipReason::HasPdf);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> ItemSnapshot {
        ItemSnapshot {
            item: 1,
            key: "1/AAAA".into(),
            library: LibraryInfo {
                id: 1,
                kind: LibraryKind::User,
                files_editable: true,
                name: String::new(),
            },
            regular: true,
            feed: false,
            item_type: "journalArticle".into(),
            title: "t".into(),
            fields: ItemFields::default(),
            pdf_attachments: 0,
            best_attachment: BestAttachment {
                kind: AttachKind::None,
                exists: None,
            },
        }
    }

    #[test]
    fn eligible_item() {
        assert_eq!(eligibility(&snap()), None);
    }

    #[test]
    fn skip_reasons_in_order() {
        let mut s = snap();
        s.pdf_attachments = 1;
        assert_eq!(eligibility(&s), Some(SkipReason::HasPdf));
        s.best_attachment = BestAttachment {
            kind: AttachKind::Pdf,
            exists: Some(false),
        };
        assert_eq!(eligibility(&s), Some(SkipReason::PdfNotDownloaded));
        s.library.files_editable = false;
        assert_eq!(eligibility(&s), Some(SkipReason::ReadOnly));
        s.feed = true;
        assert_eq!(eligibility(&s), Some(SkipReason::Feed));
        s.regular = false;
        assert_eq!(eligibility(&s), Some(SkipReason::NotRegular));
    }
}
