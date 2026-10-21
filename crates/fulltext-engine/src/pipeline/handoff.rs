//! Hand-off links attached to every institutional failure, so the user can finish in their own browser:
//! the OpenAthens redirector deep link, the EZproxy login link, a LibKey link, and the plain DOI URL.
//!
//! The links are built by `fulltext_core::urls::handoff`; this file only maps the engine's `Institution` onto
//! core's `HandoffConf`. (`pure.rs` has no wrapper for it, so this is the one pipeline file besides `pure.rs`
//! that names a core URL builder.)
//!
//! Target of the links: `https://doi.org/<doi>` when the item has a DOI; otherwise the publisher page the
//! route reached (`handoff_with_landing`), otherwise the item's own URL. With none of these the hand-off is
//! empty and the row simply offers no browser buttons.
use crate::pipeline::pure;
use crate::proto::Institution;
use fulltext_core::model::*;
use fulltext_core::urls::{self, HandoffConf, ProxyConf};

/// Build the hand-off for an item. Links that cannot be built (no DOI, no EZproxy, no LibKey id) are `None`.
/// LibKey: `https://libkey.io/libraries/<library id>/<doi>` (no token needed for the link).
pub(crate) fn handoff(inst: &Institution, ids: &Ids) -> Handoff {
    handoff_with_landing(inst, ids, None)
}

/// [`handoff`] for an item without a DOI whose route reached a publisher page: the links point at `landing`
/// (an un-proxied article URL) instead of the item's own URL. A DOI always wins as the target.
pub(crate) fn handoff_with_landing(
    inst: &Institution,
    ids: &Ids,
    landing: Option<&str>,
) -> Handoff {
    let conf = HandoffConf {
        openathens_domain: &inst.openathens_domain,
        ezproxy: inst
            .ezproxy
            .as_ref()
            .filter(|e| !e.login_host.trim().is_empty())
            .map(|e| ProxyConf {
                login_host: &e.login_host,
                suffix: &e.suffix,
            }),
        libkey_library_id: inst.libkey_library_id,
        saml_entity_id: pure::saml_entity(inst),
    };
    urls::handoff(&conf, ids, landing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Ezproxy;

    fn kcl() -> Institution {
        Institution {
            name: "King's College London".into(),
            openathens_domain: "kcl.ac.uk".into(),
            ezproxy: Some(Ezproxy {
                login_host: "login.kcl.idm.oclc.org".into(),
                suffix: "kcl.idm.oclc.org".into(),
            }),
            ezproxy_hosts: vec![],
            ezproxy_all_hosts: false,
            saml_entity_id: None,
            alma: None,
            libkey_library_id: Some(286),
            idp_hosts: vec![],
            entra_tenant: None,
            passthrough_hosts: vec![],
            warmup_url: String::new(),
        }
    }

    fn doi_ids(doi: &str) -> Ids {
        Ids {
            doi: Some(doi.into()),
            ..Ids::default()
        }
    }

    #[test]
    fn builds_all_links() {
        let ids = doi_ids("10.1002/bjs.11646");
        let h = handoff(&kcl(), &ids);
        assert_eq!(
            h.redirector_url.as_deref(),
            Some(
                "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646"
            )
        );
        assert_eq!(
            h.libkey_url.as_deref(),
            Some("https://libkey.io/libraries/286/10.1002/bjs.11646")
        );
        assert_eq!(
            h.doi_url.as_deref(),
            Some("https://doi.org/10.1002/bjs.11646")
        );
        assert!(
            h.ezproxy_url
                .as_deref()
                .is_some_and(|u| u.starts_with("https://login.kcl.idm.oclc.org/login?qurl="))
        );
    }

    #[test]
    fn ezproxy_link_wraps_the_doi_url_with_qurl() {
        let h = handoff(&kcl(), &doi_ids("10.1002/bjs.11646"));
        assert_eq!(
            h.ezproxy_url.as_deref(),
            Some(
                "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646"
            )
        );
    }

    #[test]
    fn an_institution_without_ezproxy_libkey_or_openathens_gets_only_what_it_has() {
        let inst = Institution {
            ezproxy: None,
            libkey_library_id: None,
            openathens_domain: String::new(),
            ..kcl()
        };
        let h = handoff(&inst, &doi_ids("10.1002/bjs.11646"));
        assert_eq!(h.redirector_url, None);
        assert_eq!(h.ezproxy_url, None);
        assert_eq!(h.libkey_url, None);
        assert_eq!(
            h.doi_url.as_deref(),
            Some("https://doi.org/10.1002/bjs.11646")
        );
    }

    #[test]
    fn an_item_without_a_doi_hands_off_its_own_url() {
        let ids = Ids {
            url: Some("https://www.bmj.com/content/372/bmj.n71".into()),
            ..Ids::default()
        };
        let h = handoff(&kcl(), &ids);
        assert_eq!(
            h.redirector_url.as_deref(),
            Some(
                "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fwww.bmj.com%2Fcontent%2F372%2Fbmj.n71"
            )
        );
        assert!(h.ezproxy_url.is_some());
        assert_eq!(
            (h.libkey_url, h.doi_url),
            (None, None),
            "LibKey and the DOI link need a DOI"
        );
    }

    #[test]
    fn a_reached_publisher_page_beats_the_item_url_but_never_a_doi() {
        let ids = Ids {
            url: Some("https://example.org/old".into()),
            ..Ids::default()
        };
        let landing = "https://www.bmj.com/content/372/bmj.n71";
        let h = handoff_with_landing(&kcl(), &ids, Some(landing));
        assert!(
            h.redirector_url
                .as_deref()
                .is_some_and(|u| u.contains("www.bmj.com")),
            "{h:?}"
        );
        let with_doi = Ids {
            doi: Some("10.1136/bmj.n71".into()),
            ..ids
        };
        let h = handoff_with_landing(&kcl(), &with_doi, Some(landing));
        assert!(
            h.redirector_url
                .as_deref()
                .is_some_and(|u| u.contains("doi.org")),
            "{h:?}"
        );
    }

    #[test]
    fn no_identifier_gives_an_empty_hand_off() {
        assert_eq!(handoff(&kcl(), &Ids::default()), Handoff::default());
        // A non-http item URL is not a target.
        let ids = Ids {
            url: Some("ftp://example.org/x".into()),
            ..Ids::default()
        };
        assert_eq!(handoff(&kcl(), &ids), Handoff::default());
    }

    #[test]
    fn unusual_dois_are_percent_encoded_per_segment() {
        let h = handoff(
            &kcl(),
            &doi_ids("10.1002/(sici)1097-0258(19980430)17:8<873::aid-sim780>3.0.co;2-i"),
        );
        let libkey = h.libkey_url.unwrap_or_default();
        assert!(
            libkey.starts_with("https://libkey.io/libraries/286/10.1002/"),
            "{libkey}"
        );
        assert!(!libkey.contains('<') && !libkey.contains('>'), "{libkey}");
    }
}
