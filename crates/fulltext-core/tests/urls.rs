//! URL helpers: the skeleton primitives, EZproxy proxify / unproxify vectors (research/04 sections 6.3-6.4), URL
//! classification, tracking-parameter stripping and the hand-off links.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use fulltext_core::model::{Handoff, Ids, UrlClassKind};
use fulltext_core::urls::{
    self, HandoffConf, ProxyConf, UnproxyKind, canonical_key, doi_url, ezproxy_login_url, handoff,
    host_key, host_of, libkey_url, parse_http, percent_decode, percent_encode_component, proxify,
    redirector_url, resolve, strip_tracking, unproxify, url_class,
};
use std::collections::BTreeMap;

const KCL: ProxyConf<'static> = ProxyConf {
    login_host: "login.kcl.idm.oclc.org",
    suffix: "kcl.idm.oclc.org",
};

fn no_learned() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn unproxied(url: &str) -> (String, UnproxyKind) {
    let r = unproxify(&KCL, url, &no_learned());
    (r.url, r.kind)
}

// ------------------------------------------------------------------ skeleton helpers

#[test]
fn parse_host_and_key_helpers() {
    assert!(parse_http("  https://example.org/a ").is_some());
    assert!(parse_http("ftp://example.org/").is_none());
    assert!(parse_http("mailto:a@b.c").is_none());
    assert!(parse_http("not a url").is_none());
    assert_eq!(
        host_of("https://WWW.Example.ORG/x").as_deref(),
        Some("www.example.org")
    );
    assert_eq!(host_of("javascript:alert(1)"), None);
    assert_eq!(host_key("www.thieme-connect.de"), "www-thieme-connect-de");
    assert_eq!(host_key("Doi.ORG"), "doi-org");
}

#[test]
fn hosts_longer_than_any_dns_name_are_refused_before_the_parser_sees_them() {
    // A 100 KB host (50 000 labels) cost seconds in a debug build; no real host exceeds 253 octets (+ ":65535").
    let long_host = format!("https://{}com/", "a.".repeat(50_000));
    let many_labels = format!("https://{}/", vec!["x"; 200].join("."));
    let escaped = format!("https://{}.com/", "%61".repeat(300));
    for hostile in [long_host.as_str(), many_labels.as_str(), escaped.as_str()] {
        assert!(parse_http(hostile).is_none());
        assert_eq!(host_of(hostile), None);
    }
    let base = parse_http("https://example.org/a/").unwrap();
    assert!(resolve(&base, &long_host).is_none());
    // the longest legal host name (253 octets) and an ordinary percent-escaped host still parse
    let longest = vec!["a".repeat(61); 4].join(".");
    assert_eq!(longest.len(), 247);
    assert_eq!(
        host_of(&format!("https://{longest}:65535/x")).as_deref(),
        Some(longest.as_str())
    );
    assert_eq!(
        host_of("https://%65xample.org/").as_deref(),
        Some("example.org")
    );
}

#[test]
fn non_ascii_hosts_are_refused_without_tripping_the_debug_assertion_of_the_ascii_only_idna_adapter()
{
    // `idna_adapter` 1.0.0 `debug_assert!`s on an ASCII upper-case letter before a non-ASCII one; these must not reach it.
    let base = parse_http("https://example.org/a/").unwrap();
    for hostile in [
        "https://X\u{e9}.com/",
        "https://XY\u{4e2d}Z.COM/path",
        "http://user:Pw@X\u{e9}.com/",
        "https://X%C3%A9.com/",
        "https://X%c3%a9.com/",
        "https://X%FF.com/",
        "https:/\\X\u{e9}.com/",
        "https:\\\\X\u{e9}.com/",
        "https://X\t\u{e9}.com/",
        "  \u{1}https://X\u{e9}.com/  ",
        "ftp://X\u{e9}.com/",
    ] {
        assert!(parse_http(hostile).is_none(), "{hostile:?}");
        assert_eq!(host_of(hostile), None, "{hostile:?}");
        assert_eq!(canonical_key(hostile), hostile.trim(), "{hostile:?}");
        assert!(proxify(&KCL, hostile).is_none(), "{hostile:?}");
    }
    for hostile_href in [
        "//X\u{e9}.com/x",
        "https://X\u{e9}.com/x",
        "\\\\X\u{e9}.com/x",
        "/\\X\u{e9}.com/x",
        "HTTP:X\u{e9}.com",
        "ftp://X\u{e9}.com",
    ] {
        assert!(resolve(&base, hostile_href).is_none(), "{hostile_href:?}");
    }
    // Non-ASCII anywhere but the host is fine, and so are punycode and ordinary relative references.
    assert_eq!(
        parse_http("https://example.org/caf\u{e9}?q=\u{e9}#\u{e9}")
            .unwrap()
            .as_str(),
        "https://example.org/caf%C3%A9?q=%C3%A9#%C3%A9"
    );
    assert_eq!(
        parse_http("https://us\u{e9}r:p\u{e9}@example.org/")
            .unwrap()
            .host_str(),
        Some("example.org")
    );
    assert!(parse_http("https://XN--BCHER-KVA.example/").is_some());
    assert_eq!(
        resolve(&base, "caf\u{e9}.pdf").unwrap().as_str(),
        "https://example.org/a/caf%C3%A9.pdf"
    );
    assert_eq!(
        resolve(&base, "b.pdf?x=a:b").unwrap().as_str(),
        "https://example.org/a/b.pdf?x=a:b"
    );
}

#[test]
fn resolve_drops_fragment_and_non_http() {
    let base = parse_http("https://example.org/a/b/c.html?q=1").unwrap();
    assert_eq!(
        resolve(&base, "d.pdf").unwrap().as_str(),
        "https://example.org/a/b/d.pdf"
    );
    assert_eq!(
        resolve(&base, "/x.pdf#page=2").unwrap().as_str(),
        "https://example.org/x.pdf"
    );
    assert_eq!(
        resolve(&base, "//cdn.example.net/y.pdf").unwrap().as_str(),
        "https://cdn.example.net/y.pdf"
    );
    assert!(resolve(&base, "data:text/html,hi").is_none());
    assert!(resolve(&base, "").is_some_and(|u| u.path() == "/a/b/c.html"));
}

#[test]
fn canonical_key_keeps_the_query() {
    assert_eq!(
        canonical_key("http://Example.org:80/p?a=1#f"),
        "https://example.org/p?a=1"
    );
    assert_eq!(
        canonical_key("https://example.org:443/p"),
        "https://example.org/p"
    );
    assert_eq!(
        canonical_key("https://example.org:8443/p"),
        "https://example.org:8443/p"
    );
    assert_ne!(
        canonical_key("https://journals.plos.org/plosone/article/file?id=10.1371/x&type=printable"),
        canonical_key(
            "https://journals.plos.org/plosone/article/file?id=10.1371/x&type=manuscript"
        )
    );
    assert_eq!(canonical_key("  not a url  "), "not a url");
}

#[test]
fn percent_helpers_round_trip_including_multibyte() {
    for s in [
        "plain",
        "a b&c=d/e?f#g+h",
        "10.1016/S0140-6736(20)30183-5",
        "caf\u{e9} na\u{ef}ve",
        "\u{4e2d}\u{6587}/\u{1F4DA}",
        "100%",
        "",
    ] {
        assert_eq!(percent_decode(&percent_encode_component(s)), s, "{s}");
    }
    assert_eq!(percent_encode_component("\u{e9}"), "%C3%A9");
    assert_eq!(percent_encode_component("\u{1F4DA}"), "%F0%9F%93%9A");
    assert_eq!(percent_encode_component("A-z_0.9!~*'()"), "A-z_0.9!~*'()");
    assert_eq!(percent_encode_component("+&#?=%"), "%2B%26%23%3F%3D%25");
    assert_eq!(percent_decode("%C3%A9"), "\u{e9}");
    assert_eq!(
        percent_decode("%zz%4"),
        "%zz%4",
        "invalid escapes stay literal"
    );
    assert_eq!(
        percent_decode("%FF"),
        "\u{FFFD}",
        "invalid UTF-8 becomes the replacement character"
    );
}

#[test]
fn builders_and_doi_url() {
    assert_eq!(
        doi_url("10.1016/S0140-6736(20)30183-5"),
        "https://doi.org/10.1016/S0140-6736(20)30183-5"
    );
    assert_eq!(
        doi_url("10.1002/(SICI)1097-0258(19980430)17:8<873::AID-SIM780>3.0.CO;2-1"),
        "https://doi.org/10.1002/(SICI)1097-0258(19980430)17%3A8%3C873%3A%3AAID-SIM780%3E3.0.CO%3B2-1"
    );
    assert_eq!(
        redirector_url("kcl.ac.uk", "https://doi.org/10.1002/bjs.11646"),
        "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646"
    );
    assert_eq!(
        ezproxy_login_url("login.kcl.idm.oclc.org", "https://www.bmj.com/x?a=1&b=2"),
        "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fwww.bmj.com%2Fx%3Fa%3D1%26b%3D2"
    );
    assert_eq!(
        libkey_url(286, "10.1002/bjs.11646"),
        "https://libkey.io/libraries/286/10.1002/bjs.11646"
    );
}

// ------------------------------------------------------------------ proxify

#[test]
fn proxify_vectors() {
    let cases = [
        (
            "https://doi.org/10.1038/nature12373",
            Some("https://doi-org.kcl.idm.oclc.org/10.1038/nature12373"),
        ),
        (
            "https://www.bmj.com/content/372/bmj.n71?x=1#a",
            Some("https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71?x=1#a"),
        ),
        (
            "https://www.thieme-connect.de/a",
            Some("https://www-thieme-connect-de.kcl.idm.oclc.org/a"),
        ),
        (
            "http://doi.org/x",
            Some("http://doi.org.kcl.idm.oclc.org/x"),
        ),
        ("https://example.org:8443/x", None),
        ("http://example.org:8080/x", None),
        ("https://10.0.0.1/x", None),
        ("http://192.168.1.1/x", None),
        ("https://[2001:db8::1]/x", None),
        ("ftp://example.org/x", None),
        ("mailto:a@b.c", None),
        ("not a url", None),
        ("", None),
    ];
    for (target, want) in cases {
        assert_eq!(proxify(&KCL, target).as_deref(), want, "{target}");
    }
}

#[test]
fn proxify_leaves_already_proxied_targets_unchanged() {
    for target in [
        "https://doi-org.kcl.idm.oclc.org/10.1038/nature12373",
        "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2Fx",
        "http://doi.org.kcl.idm.oclc.org/x",
        "https://login.kcl.idm.oclc.org:8443/menu",
    ] {
        assert_eq!(proxify(&KCL, target).as_deref(), Some(target), "{target}");
    }
}

#[test]
fn proxify_normalises_host_and_keeps_hyphens() {
    assert_eq!(
        proxify(&KCL, "https://WWW.BMJ.COM/A").as_deref(),
        Some("https://www-bmj-com.kcl.idm.oclc.org/A")
    );
    assert_eq!(
        proxify(&KCL, "https://my-journal.example.org/x").as_deref(),
        Some("https://my-journal-example-org.kcl.idm.oclc.org/x"),
        "existing hyphens are kept, not doubled"
    );
    assert_eq!(
        proxify(&KCL, "https://doi.org").as_deref(),
        Some("https://doi-org.kcl.idm.oclc.org/")
    );
    assert_eq!(
        proxify(&KCL, "https://doi.org:443/x").as_deref(),
        Some("https://doi-org.kcl.idm.oclc.org/x")
    );
    assert_eq!(
        proxify(&KCL, "http://doi.org:80/x").as_deref(),
        Some("http://doi.org.kcl.idm.oclc.org/x")
    );
    assert_eq!(
        proxify(&KCL, "https://doi.org./x").as_deref(),
        Some("https://doi-org.kcl.idm.oclc.org/x"),
        "root dot dropped"
    );
    assert_eq!(
        proxify(&KCL, "https://user:pw@doi.org/x").as_deref(),
        Some("https://doi-org.kcl.idm.oclc.org/x"),
        "credentials dropped"
    );
    assert_eq!(
        proxify(&KCL, "https://doi.org/a b?q=x y").as_deref(),
        Some("https://doi-org.kcl.idm.oclc.org/a%20b?q=x%20y")
    );
}

#[test]
fn proxify_only_ever_emits_ascii_hosts() {
    // The workspace pins idna_adapter 1.0.0 (ASCII hosts only), so a Unicode host may be refused outright; what must
    // never happen is a Unicode label in the output. A punycode host proxifies like any other (hyphens kept).
    if let Some(p) = proxify(&KCL, "https://b\u{fc}cher.example/x") {
        assert!(p.is_ascii() && p.ends_with(".kcl.idm.oclc.org/x"), "{p}");
    }
    assert_eq!(
        proxify(&KCL, "https://xn--bcher-kva.example/x").as_deref(),
        Some("https://xn--bcher-kva-example.kcl.idm.oclc.org/x")
    );
}

#[test]
fn proxify_with_a_blank_suffix_is_none() {
    let bad = ProxyConf {
        login_host: "",
        suffix: " ",
    };
    assert_eq!(proxify(&bad, "https://doi.org/x"), None);
}

// ------------------------------------------------------------------ unproxify

#[test]
fn unproxify_hostname_vectors() {
    // Ambiguous without a learned map ...
    assert_eq!(
        unproxied("https://doi-org.kcl.idm.oclc.org/10.1038/nature12373"),
        (
            "https://doi.org/10.1038/nature12373".into(),
            UnproxyKind::Ambiguous
        )
    );
    // ... Label with `doi-org -> doi.org` learned.
    let learned = BTreeMap::from([("doi-org".to_owned(), "doi.org".to_owned())]);
    let r = unproxify(
        &KCL,
        "https://doi-org.kcl.idm.oclc.org/10.1038/nature12373",
        &learned,
    );
    assert_eq!(
        (r.url.as_str(), r.kind),
        ("https://doi.org/10.1038/nature12373", UnproxyKind::Label)
    );

    assert_eq!(
        unproxied("http://doi.org.kcl.idm.oclc.org/x"),
        ("http://doi.org/x".into(), UnproxyKind::Label)
    );
    assert_eq!(
        unproxied("https://WWW-BMJ-COM.kcl.idm.oclc.org/"),
        ("https://www.bmj.com/".into(), UnproxyKind::Ambiguous)
    );
    assert_eq!(
        unproxied("https://www-thieme-connect-de.kcl.idm.oclc.org/a"),
        ("https://www.thieme-connect.de/a".into(), UnproxyKind::Label),
        "resolved through the built-in table"
    );
    // https with a dotted label (the http-style form over https) is a Label too.
    assert_eq!(
        unproxied("https://doi.org.kcl.idm.oclc.org/x"),
        ("https://doi.org/x".into(), UnproxyKind::Label)
    );
}

#[test]
fn unproxify_keeps_path_query_and_fragment_and_drops_the_port() {
    assert_eq!(
        unproxied("https://www-bmj-com.kcl.idm.oclc.org:443/content/372/bmj.n71?x=1&y=%2F#a"),
        (
            "https://www.bmj.com/content/372/bmj.n71?x=1&y=%2F#a".into(),
            UnproxyKind::Ambiguous
        )
    );
    assert_eq!(
        unproxied("https://www-bmj-com.kcl.idm.oclc.org:8443/a"),
        ("https://www.bmj.com/a".into(), UnproxyKind::Ambiguous)
    );
}

#[test]
fn unproxify_every_built_in_hyphenated_host_via_the_table() {
    for host in [
        "www.thieme-connect.de",
        "www.thieme-connect.com",
        "www.oecd-ilibrary.org",
        "www.pep-web.org",
        "www.wiley-vch.de",
        "www.e-elgar.com",
        "www.jbe-platform.com",
        "www.un-ilibrary.org",
        "wrds-www.wharton.upenn.edu",
    ] {
        let proxied = proxify(&KCL, &format!("https://{host}/p?q=1")).unwrap();
        assert_eq!(
            unproxied(&proxied),
            (format!("https://{host}/p?q=1"), UnproxyKind::Label),
            "{host}"
        );
    }
}

#[test]
fn learned_map_wins_over_the_table_and_the_guess() {
    let learned = BTreeMap::from([
        (
            "www-thieme-connect-de".to_owned(),
            "thieme.example".to_owned(),
        ),
        ("my-site-org".to_owned(), "my-site.org".to_owned()),
    ]);
    let a = unproxify(
        &KCL,
        "https://www-thieme-connect-de.kcl.idm.oclc.org/a",
        &learned,
    );
    assert_eq!(
        (a.url.as_str(), a.kind),
        ("https://thieme.example/a", UnproxyKind::Label)
    );
    let b = unproxify(&KCL, "https://my-site-org.kcl.idm.oclc.org/", &learned);
    assert_eq!(
        (b.url.as_str(), b.kind),
        ("https://my-site.org/", UnproxyKind::Label)
    );
    // A junk learned value cannot produce a broken URL: the input comes back unchanged.
    let junk = BTreeMap::from([("doi-org".to_owned(), "bad host/".to_owned())]);
    let c = unproxify(&KCL, "https://doi-org.kcl.idm.oclc.org/x", &junk);
    assert_eq!(c.kind, UnproxyKind::NotProxied);
    assert_eq!(c.url, "https://doi-org.kcl.idm.oclc.org/x");
}

#[test]
fn double_hyphen_labels_are_ambiguous_and_invalid_ones_stay_put() {
    // `www-thieme--connect-de` is not something EZproxy produces; the guess `www.thieme..connect.de` is not a host.
    let url = "https://www-thieme--connect-de.kcl.idm.oclc.org/a";
    let r = unproxify(&KCL, url, &no_learned());
    assert!(
        matches!(r.kind, UnproxyKind::Ambiguous | UnproxyKind::NotProxied),
        "{r:?}"
    );
    if r.kind == UnproxyKind::NotProxied {
        assert_eq!(r.url, url);
    }
    // A double hyphen is never a confident answer.
    assert_ne!(
        unproxied("https://a--b.kcl.idm.oclc.org/x").1,
        UnproxyKind::Label
    );
    assert_ne!(
        unproxied("https://a--b.kcl.idm.oclc.org/x").1,
        UnproxyKind::Qurl
    );
}

#[test]
fn unproxify_login_host_urls() {
    assert_eq!(
        unproxied("https://login.kcl.idm.oclc.org/login?qurl=https://doi.org%2fx%3fa%3d1%26b%3d2"),
        ("https://doi.org/x?a=1&b=2".into(), UnproxyKind::Qurl)
    );
    assert_eq!(
        unproxied("https://login.kcl.idm.oclc.org/login?url=https://www.bmj.com/x?a=1&b=2"),
        (
            "https://www.bmj.com/x?a=1&b=2".into(),
            UnproxyKind::UrlParam
        ),
        "everything after url= is the target, `&` included"
    );
    // Our own builder round-trips, with every reserved character encoded.
    let target = "https://www.bmj.com/content/372/bmj.n71?a=1&b=x%20y+z#frag";
    let built = ezproxy_login_url("login.kcl.idm.oclc.org", target);
    assert_eq!(unproxied(&built), (target.into(), UnproxyKind::Qurl));
    // /connect behaves like /login; the path is case-insensitive; qurl beats url.
    assert_eq!(
        unproxied("https://login.kcl.idm.oclc.org/connect?qurl=https%3A%2F%2Fa.org%2Fx").1,
        UnproxyKind::Qurl
    );
    assert_eq!(
        unproxied("https://login.kcl.idm.oclc.org/LOGIN?url=https://a.org/x").1,
        UnproxyKind::UrlParam
    );
    assert_eq!(
        unproxied(
            "https://login.kcl.idm.oclc.org/login?url=https://b.org/&qurl=https%3A%2F%2Fa.org%2Fx"
        ),
        ("https://a.org/x".into(), UnproxyKind::Qurl)
    );
    // url= preceded by another pair.
    assert_eq!(
        unproxied("https://login.kcl.idm.oclc.org/login?auth=1&url=https://a.org/x?y=1&z=2"),
        ("https://a.org/x?y=1&z=2".into(), UnproxyKind::UrlParam)
    );
    // An encoded url= value is decoded once when the verbatim text is not a URL.
    assert_eq!(
        unproxied("https://login.kcl.idm.oclc.org/login?url=https%3A%2F%2Fa.org%2Fx"),
        ("https://a.org/x".into(), UnproxyKind::UrlParam)
    );
}

#[test]
fn unproxify_login_host_pages_without_a_target_are_not_proxied() {
    for url in [
        "https://login.kcl.idm.oclc.org/menu",
        "https://login.kcl.idm.oclc.org/logout",
        "https://login.kcl.idm.oclc.org/login",
        "https://login.kcl.idm.oclc.org/login?qurl=",
        "https://login.kcl.idm.oclc.org/login?qurl=javascript%3Aalert(1)",
        "https://login.kcl.idm.oclc.org/login?auth=1",
        "https://login.kcl.idm.oclc.org/",
        "https://login.kcl.idm.oclc.org/menu?url=https://a.org/x",
    ] {
        assert_eq!(
            unproxied(url),
            (url.to_owned(), UnproxyKind::NotProxied),
            "{url}"
        );
    }
}

#[test]
fn unproxify_openathens_redirector() {
    assert_eq!(
        unproxied(
            "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646"
        ),
        (
            "https://doi.org/10.1002/bjs.11646".into(),
            UnproxyKind::Redirector
        )
    );
    let built = redirector_url(
        "kcl.ac.uk",
        "https://doi.org/10.1016/S0140-6736(20)30183-5?a=1&b=2",
    );
    assert_eq!(
        unproxied(&built),
        (
            "https://doi.org/10.1016/S0140-6736(20)30183-5?a=1&b=2".into(),
            UnproxyKind::Redirector
        )
    );
    for url in [
        "https://go.openathens.net/redirector/kcl.ac.uk",
        "https://go.openathens.net/redirector/?url=https%3A%2F%2Fa.org%2F",
        "https://go.openathens.net/other?url=https%3A%2F%2Fa.org%2F",
        "https://go.openathens.net/redirector/kcl.ac.uk?url=notaurl",
    ] {
        assert_eq!(
            unproxied(url),
            (url.to_owned(), UnproxyKind::NotProxied),
            "{url}"
        );
    }
}

#[test]
fn unproxify_other_urls_are_untouched() {
    for url in [
        "https://www.nature.com/articles/nature12373",
        "https://doi.org/10.1038/nature12373",
        "https://kcl.idm.oclc.org.evil.example/x",
        "https://notkcl.idm.oclc.org.example/x",
        "https://10.0.0.1/x",
        "ftp://doi-org.kcl.idm.oclc.org/x",
        "mailto:a@b.c",
        "garbage \u{0}\u{1} input",
        "",
    ] {
        assert_eq!(
            unproxied(url),
            (url.to_owned(), UnproxyKind::NotProxied),
            "{url}"
        );
    }
}

const ROUND_TRIP_URLS: [&str; 20] = [
    "https://doi.org/10.1038/nature12373",
    "https://www.bmj.com/content/372/bmj.n71?x=1#a",
    "https://www.thieme-connect.de/a",
    "https://www.nature.com/articles/nature12373.pdf",
    "https://academic.oup.com/bjs/article/doi/10.1093/bjs/znaf176/8240140",
    "https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/bjs.11646?download=true",
    "https://www.sciencedirect.com/science/article/pii/S0140673620301835/pdfft?md5=abc&pid=1-s2.0-S0140673620301835-main.pdf",
    "https://link.springer.com/content/pdf/10.1007/s00268-020-05577-1.pdf",
    "https://journals.lww.com/annalsofsurgery/fulltext/2023/01000/x.5.aspx",
    "https://www.ncbi.nlm.nih.gov/pmc/articles/PMC2707599/pdf/pmed.1000097.pdf",
    "https://jamanetwork.com/journals/jama/articlepdf/2763401/jama_x.pdf",
    "https://www.tandfonline.com/doi/pdf/10.1080/00000000.2020.1?needAccess=true",
    "https://journals.sagepub.com/doi/pdf/10.1177/0000000000000?download=true",
    "https://www.cambridge.org/core/services/aop-cambridge-core/content/view/ABC/x.pdf",
    "https://www.pep-web.org/document.php?id=ijp.001.0001a",
    "https://www.oecd-ilibrary.org/docserver/x.pdf?expires=1&id=id&accname=guest",
    "http://doi.org/x",
    "http://example.org/a/b?c=d",
    "https://my-hyphen-site.example.org/x",
    "https://www.e-elgar.com/shop/x",
];

#[test]
fn proxify_unproxify_round_trip_on_assorted_urls() {
    for original in ROUND_TRIP_URLS {
        let proxied =
            proxify(&KCL, original).unwrap_or_else(|| panic!("{original} should proxify"));
        let back = unproxify(&KCL, &proxied, &no_learned());
        let host = host_of(original).unwrap();
        if original.starts_with("http://") || !host.contains('-') {
            assert_eq!(back.url, original, "{original} via {proxied}");
        } else {
            assert!(
                matches!(back.kind, UnproxyKind::Ambiguous | UnproxyKind::Label),
                "{original}: {:?}",
                back.kind
            );
            if back.kind == UnproxyKind::Label {
                assert_eq!(back.url, original, "{original}");
            }
        }
        // A hostname with a hyphen is recovered exactly once the label has been learned from a qurl redirect.
        let learned = BTreeMap::from([(host_key(&host), host.clone())]);
        let learned_back = unproxify(&KCL, &proxied, &learned);
        if original.starts_with("https://") {
            assert_eq!(learned_back.url, original, "{original} (learned)");
        }
    }
}

// ------------------------------------------------------------------ totality (seeded xorshift)

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// A syntactically valid, already-normalised http(s) URL.
fn random_url(rng: &mut XorShift) -> String {
    let scheme = if rng.below(4) == 0 { "http" } else { "https" };
    let labels = [
        "www",
        "doi",
        "academic",
        "journals",
        "my-site",
        "thieme-connect",
        "x1",
        "link",
        "a-b-c",
        "pmc",
        "ncbi",
        "nlm",
    ];
    let tlds = ["org", "com", "de", "co.uk", "net", "edu"];
    let mut host = String::new();
    for i in 0..(1 + rng.below(3)) {
        if i > 0 {
            host.push('.');
        }
        host.push_str(rng.pick(&labels));
    }
    host.push('.');
    host.push_str(rng.pick(&tlds));
    let segs = [
        "article",
        "10.1038",
        "nature12373",
        "S0140-6736(20)30183-5",
        "x%20y",
        "a;b",
        "pdf",
        "file.pdf",
        "~u",
        "caf%C3%A9",
    ];
    let mut path = String::new();
    for _ in 0..rng.below(4) {
        path.push('/');
        path.push_str(rng.pick(&segs));
    }
    if path.is_empty() {
        path.push('/');
    }
    let mut url = format!("{scheme}://{host}{path}");
    if rng.below(2) == 0 {
        url.push('?');
        let pairs = [
            "a=1",
            "b=%2F",
            "utm_source=x",
            "download=true",
            "id=10.1371/x",
            "q=a+b",
            "flag",
        ];
        let n = 1 + rng.below(3);
        let list: Vec<&str> = (0..n).map(|_| rng.pick(&pairs)).collect();
        url.push_str(&list.join("&"));
    }
    if rng.below(4) == 0 {
        url.push_str("#frag");
    }
    url
}

fn random_junk(rng: &mut XorShift) -> String {
    let alphabet: Vec<char> = "abcXYZ019:/?#&=%.-_ ~+@[]\u{e9}\u{4e2d}\u{0}\n"
        .chars()
        .collect();
    let len = rng.below(60);
    (0..len)
        .map(|_| alphabet[rng.below(alphabet.len())])
        .collect()
}

#[test]
fn proxify_and_unproxify_are_total_and_invert_each_other() {
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    for _ in 0..200 {
        let original = random_url(&mut rng);
        assert!(
            parse_http(&original).is_some(),
            "generator produced {original}"
        );

        let proxied =
            proxify(&KCL, &original).unwrap_or_else(|| panic!("{original} should proxify"));
        assert_eq!(
            proxify(&KCL, &proxied).as_deref(),
            Some(proxied.as_str()),
            "idempotent: {proxied}"
        );
        let back = unproxify(&KCL, &proxied, &no_learned());
        let host = host_of(&original).unwrap();
        if original.starts_with("http://") || !host.contains('-') {
            assert_eq!(back.url, original, "via {proxied}");
        } else {
            assert!(
                matches!(
                    back.kind,
                    UnproxyKind::Ambiguous | UnproxyKind::Label | UnproxyKind::NotProxied
                ),
                "{original}: {back:?}"
            );
            if back.kind == UnproxyKind::Label {
                assert_eq!(back.url, original);
            }
        }

        // The result of unproxify is always something we can parse again, or the input itself.
        for candidate in [
            original.clone(),
            proxied.clone(),
            ezproxy_login_url("login.kcl.idm.oclc.org", &original),
            redirector_url("kcl.ac.uk", &original),
        ] {
            let r = unproxify(&KCL, &candidate, &no_learned());
            if r.kind == UnproxyKind::NotProxied {
                assert_eq!(r.url, candidate);
            } else {
                assert!(parse_http(&r.url).is_some(), "{candidate} -> {r:?}");
            }
        }
        assert_eq!(
            strip_tracking(&strip_tracking(&original)),
            strip_tracking(&original),
            "idempotent: {original}"
        );
        let _ = url_class(Some(&KCL), &original);
    }
}

#[test]
fn junk_input_never_panics() {
    let mut rng = XorShift(0xDEAD_BEEF_CAFE_F00D);
    for _ in 0..200 {
        let junk = random_junk(&mut rng);
        let wrapped = format!("https://{junk}");
        for s in [junk.as_str(), wrapped.as_str()] {
            let _ = proxify(&KCL, s);
            let r = unproxify(&KCL, s, &no_learned());
            if r.kind == UnproxyKind::NotProxied {
                assert_eq!(r.url, s);
            }
            let _ = url_class(None, s);
            let _ = url_class(Some(&KCL), s);
            let _ = strip_tracking(s);
            let _ = handoff(
                &HandoffConf {
                    openathens_domain: "kcl.ac.uk",
                    ezproxy: Some(KCL),
                    libkey_library_id: Some(286),
                },
                &Ids {
                    doi: Some(s.to_owned()),
                    ..Ids::default()
                },
                Some(s),
            );
        }
    }
}

// ------------------------------------------------------------------ url_class

#[test]
fn url_class_openathens_redirector() {
    for url in [
        "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2Fx",
        "https://go.openathens.net/redirector/nhs",
        "https://GO.OPENATHENS.NET/Redirector/kcl.ac.uk",
    ] {
        assert_eq!(
            url_class(None, url),
            UrlClassKind::OpenathensRedirector,
            "{url}"
        );
    }
    assert_eq!(
        url_class(None, "https://go.openathens.net/"),
        UrlClassKind::Other
    );
    assert_eq!(
        url_class(None, "https://go.openathens.net/redirectorx"),
        UrlClassKind::Other
    );
}

#[test]
fn url_class_idp_login() {
    for url in [
        "https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/saml2?SAMLRequest=x",
        "https://login.microsoftonline.com/common/oauth2/authorize",
        "https://login.live.com/login.srf",
        "https://login.microsoft.com/",
        "https://device.login.microsoftonline.com/",
        "https://kclidp.kcl.ac.uk/idp/profile/SAML2/Redirect/SSO",
        "https://login.openathens.net/auth/kcl.ac.uk/oidc",
    ] {
        assert_eq!(url_class(None, url), UrlClassKind::IdpLogin, "{url}");
    }
    assert_eq!(
        url_class(None, "https://login.openathens.net/"),
        UrlClassKind::Other
    );
    assert_eq!(
        url_class(None, "https://login.openathens.net/other"),
        UrlClassKind::Other
    );
}

#[test]
fn url_class_ezproxy_login_uses_the_configured_host() {
    assert_eq!(
        url_class(None, "https://login.kcl.idm.oclc.org/login?qurl=x"),
        UrlClassKind::EzproxyLogin
    );
    assert_eq!(
        url_class(Some(&KCL), "https://login.kcl.idm.oclc.org/connect"),
        UrlClassKind::EzproxyLogin
    );
    assert_eq!(
        url_class(Some(&KCL), "https://login.kcl.idm.oclc.org/LOGIN"),
        UrlClassKind::EzproxyLogin
    );
    assert_eq!(
        url_class(Some(&KCL), "https://login.kcl.idm.oclc.org/menu"),
        UrlClassKind::Other
    );
    assert_eq!(
        url_class(Some(&KCL), "https://doi-org.kcl.idm.oclc.org/10.1/x"),
        UrlClassKind::Other
    );
    let other = ProxyConf {
        login_host: "ezproxy.example.ac.uk",
        suffix: "example.ac.uk",
    };
    assert_eq!(
        url_class(Some(&other), "https://ezproxy.example.ac.uk/login?url=x"),
        UrlClassKind::EzproxyLogin
    );
    assert_eq!(
        url_class(Some(&other), "https://login.kcl.idm.oclc.org/login"),
        UrlClassKind::Other,
        "a configured host replaces the default"
    );
}

#[test]
fn url_class_publisher_sso_hops() {
    for url in [
        "https://onlinelibrary.wiley.com/action/ssostart?idp=E&redirectUri=T",
        "https://www.tandfonline.com/action/ssostart?idp=E",
        "https://jamanetwork.com/Shibboleth.sso/Login?entityID=E&target=T",
        "https://shibboleth.cambridge.org/Shibboleth.sso/discovery?SAMLDS=1",
        "https://auth.elsevier.com/ShibAuth/institutionLogin?entityID=E&appReturnURL=T",
        "https://fsso.springer.com/federation/init?entityId=E",
        "https://sp.nature.com/saml/login?idp=E",
        "https://secure.nature.com/saml2/x",
        "https://oup-sp.sams-sigma.com/Shibboleth.sso/Login?entityID=E",
        "https://shibboleth.highwire.org/session/init?entityID=E",
        "https://profile.thieme.de/HTML/sso/ejournals/shibboleth.htm?x=1",
        "https://karger.com/ExternalAuthentication/KeystoneDeeplink?entityId=E",
        "https://shibboleth.cambridge.org/CJOShibb2/index",
    ] {
        assert_eq!(url_class(None, url), UrlClassKind::PublisherSsoHop, "{url}");
    }
    // Case-insensitive.
    assert_eq!(
        url_class(None, "https://x.org/SHIBBOLETH.SSO/LOGIN"),
        UrlClassKind::PublisherSsoHop
    );
}

#[test]
fn url_class_precedence_and_other() {
    // Redirector before IdP before EZproxy before SSO hop.
    assert_eq!(
        url_class(
            None,
            "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fa.org%2FShibboleth.sso%2FLogin"
        ),
        UrlClassKind::OpenathensRedirector
    );
    assert_eq!(
        url_class(
            None,
            "https://login.microsoftonline.com/x?r=/action/ssostart"
        ),
        UrlClassKind::IdpLogin
    );
    assert_eq!(
        url_class(
            None,
            "https://login.kcl.idm.oclc.org/login?url=https://a.org/action/ssostart"
        ),
        UrlClassKind::EzproxyLogin
    );
    for url in [
        "https://www.nature.com/articles/nature12373",
        "https://doi.org/10.1038/nature12373",
        "https://www-nature-com.kcl.idm.oclc.org/articles/x",
        "https://example.org/login",
        "not a url",
        "",
    ] {
        assert_eq!(url_class(None, url), UrlClassKind::Other, "{url}");
    }
}

// ------------------------------------------------------------------ strip_tracking

#[test]
fn strip_tracking_removes_only_tracking_pairs() {
    let cases = [
        (
            "https://a.org/p?utm_source=x&id=7&utm_medium=y",
            "https://a.org/p?id=7",
        ),
        (
            "https://a.org/p?UTM_Campaign=x&id=7",
            "https://a.org/p?id=7",
        ),
        (
            "https://a.org/p?fbclid=1&gclid=2&dclid=3&msclkid=4&mc_cid=5&mc_eid=6&ref_src=7&_ga=8&igshid=9&spm=a&cmpid=b&WT.mc_id=c&id=7",
            "https://a.org/p?id=7",
        ),
        (
            "https://a.org/p?wt.mc_id=c&FBCLID=1&k=v",
            "https://a.org/p?k=v",
        ),
        (
            "https://a.org/p?id=7&utm_source=x#frag",
            "https://a.org/p?id=7#frag",
        ),
        ("https://a.org/p?utm_source=x", "https://a.org/p"),
        ("https://a.org/p?utm_source=x#frag", "https://a.org/p#frag"),
        ("https://a.org/p?fbclid", "https://a.org/p"),
        (
            "https://a.org/p?a=1&&b=2&utm_x=3",
            "https://a.org/p?a=1&&b=2",
        ),
    ];
    for (input, want) in cases {
        assert_eq!(strip_tracking(input), want, "{input}");
    }
}

#[test]
fn strip_tracking_never_reorders_or_re_encodes_survivors() {
    let signed = "https://cdn.example.org/f.pdf?Expires=1700000000&Signature=a%2Bb%2Fc~d==&Key-Pair-Id=K1&utm_source=mail&response-content-disposition=attachment%3B%20filename%3D%22x.pdf%22";
    assert_eq!(
        strip_tracking(signed),
        "https://cdn.example.org/f.pdf?Expires=1700000000&Signature=a%2Bb%2Fc~d==&Key-Pair-Id=K1&response-content-disposition=attachment%3B%20filename%3D%22x.pdf%22"
    );
    // `utm` without the underscore and names that merely contain a tracking name are kept.
    for keep in [
        "https://a.org/p?utm=1",
        "https://a.org/p?xutm_source=1",
        "https://a.org/p?spmx=1",
        "https://a.org/p?gclid2=1",
        "https://a.org/p?q=utm_source%3D1",
    ] {
        assert_eq!(strip_tracking(keep), keep);
    }
}

#[test]
fn strip_tracking_returns_other_input_unchanged() {
    for s in [
        "https://a.org/p",
        "https://a.org/p?",
        "https://a.org/p#utm_source=x",
        "mailto:a@b.c?utm_source=x",
        "not a url?utm_source=x",
        "",
        "ftp://a.org/?fbclid=1",
    ] {
        assert_eq!(strip_tracking(s), s, "{s}");
    }
}

// ------------------------------------------------------------------ handoff

fn full_conf() -> HandoffConf<'static> {
    HandoffConf {
        openathens_domain: "kcl.ac.uk",
        ezproxy: Some(KCL),
        libkey_library_id: Some(286),
    }
}

fn ids_with(doi: Option<&str>, url: Option<&str>) -> Ids {
    Ids {
        doi: doi.map(str::to_owned),
        url: url.map(str::to_owned),
        ..Ids::default()
    }
}

#[test]
fn handoff_for_the_bjs_example() {
    let h = handoff(
        &full_conf(),
        &ids_with(Some("10.1002/bjs.11646"), None),
        None,
    );
    assert_eq!(
        h,
        Handoff {
            redirector_url: Some("https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646".into()),
            ezproxy_url: Some("https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646".into()),
            libkey_url: Some("https://libkey.io/libraries/286/10.1002/bjs.11646".into()),
            doi_url: Some("https://doi.org/10.1002/bjs.11646".into()),
        }
    );
}

#[test]
fn handoff_for_a_lancet_doi_with_parentheses() {
    let doi = "10.1016/S0140-6736(20)30183-5";
    let h = handoff(
        &full_conf(),
        &ids_with(Some(doi), None),
        Some("https://www.thelancet.com/x"),
    );
    assert_eq!(
        h.doi_url.as_deref(),
        Some("https://doi.org/10.1016/S0140-6736(20)30183-5")
    );
    assert_eq!(
        h.redirector_url.as_deref(),
        Some(
            "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1016%2FS0140-6736(20)30183-5"
        ),
        "the DOI wins over the landing URL (thelancet.com is unknown to the redirector)"
    );
    assert_eq!(
        h.ezproxy_url.as_deref(),
        Some(
            "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2F10.1016%2FS0140-6736(20)30183-5"
        )
    );
    assert_eq!(
        h.libkey_url.as_deref(),
        Some("https://libkey.io/libraries/286/10.1016/S0140-6736(20)30183-5")
    );
    // The EZproxy link is recoverable.
    assert_eq!(
        unproxified(h.ezproxy_url.as_deref().unwrap()),
        "https://doi.org/10.1016/S0140-6736(20)30183-5"
    );
    assert_eq!(
        unproxified(h.redirector_url.as_deref().unwrap()),
        "https://doi.org/10.1016/S0140-6736(20)30183-5"
    );
}

fn unproxified(url: &str) -> String {
    unproxify(&KCL, url, &no_learned()).url
}

#[test]
fn handoff_encodes_query_hostile_doi_characters() {
    let doi = "10.1002/(SICI)1097-0258(19980430)17:8<873::AID-SIM780>3.0.CO;2-1";
    let h = handoff(&full_conf(), &ids_with(Some(doi), None), None);
    let doi_link = h.doi_url.unwrap();
    assert_eq!(
        doi_link,
        "https://doi.org/10.1002/(SICI)1097-0258(19980430)17%3A8%3C873%3A%3AAID-SIM780%3E3.0.CO%3B2-1"
    );
    assert!(!doi_link.contains(['<', '>', ';']));
    let libkey = h.libkey_url.unwrap();
    assert_eq!(
        libkey,
        "https://libkey.io/libraries/286/10.1002/(SICI)1097-0258(19980430)17%3A8%3C873%3A%3AAID-SIM780%3E3.0.CO%3B2-1"
    );
    // Query and fragment delimiters inside a DOI cannot escape the path.
    let nasty = handoff(&full_conf(), &ids_with(Some("10.1000/a?b#c&d"), None), None);
    assert_eq!(
        nasty.doi_url.as_deref(),
        Some("https://doi.org/10.1000/a%3Fb%23c%26d")
    );
    assert_eq!(
        nasty.libkey_url.as_deref(),
        Some("https://libkey.io/libraries/286/10.1000/a%3Fb%23c%26d")
    );
    assert!(!nasty.redirector_url.unwrap().contains('#'));
}

#[test]
fn handoff_without_a_doi_uses_landing_then_item_url() {
    let h = handoff(
        &full_conf(),
        &ids_with(None, Some("https://item.example/1")),
        Some("https://www.bmj.com/content/372/bmj.n71?x=1"),
    );
    assert_eq!(
        h.redirector_url.as_deref(),
        Some(
            "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fwww.bmj.com%2Fcontent%2F372%2Fbmj.n71%3Fx%3D1"
        )
    );
    assert_eq!(
        h.ezproxy_url.as_deref(),
        Some(
            "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fwww.bmj.com%2Fcontent%2F372%2Fbmj.n71%3Fx%3D1"
        )
    );
    assert_eq!((h.libkey_url, h.doi_url), (None, None));

    let from_item = handoff(
        &full_conf(),
        &ids_with(None, Some("https://item.example/1")),
        None,
    );
    assert_eq!(
        from_item.ezproxy_url.as_deref(),
        Some("https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fitem.example%2F1")
    );

    // A landing URL that is not http(s) is skipped in favour of the item URL.
    let skip = handoff(
        &full_conf(),
        &ids_with(None, Some("https://item.example/1")),
        Some("javascript:void(0)"),
    );
    assert_eq!(skip, from_item);
}

#[test]
fn handoff_with_no_usable_target_is_empty() {
    assert_eq!(
        handoff(&full_conf(), &Ids::default(), None),
        Handoff::default()
    );
    assert_eq!(
        handoff(
            &full_conf(),
            &ids_with(Some("  "), Some("ftp://x.org/f")),
            Some("not a url")
        ),
        Handoff::default()
    );
    assert_eq!(
        handoff(
            &full_conf(),
            &ids_with(None, Some("file:///etc/passwd")),
            None
        ),
        Handoff::default()
    );
}

#[test]
fn handoff_follows_the_configuration() {
    let doi = ids_with(Some("10.1002/bjs.11646"), None);
    let bare = HandoffConf {
        openathens_domain: "",
        ezproxy: None,
        libkey_library_id: None,
    };
    assert_eq!(
        handoff(&bare, &doi, None),
        Handoff {
            doi_url: Some("https://doi.org/10.1002/bjs.11646".into()),
            ..Handoff::default()
        }
    );
    let only_ezproxy = HandoffConf {
        openathens_domain: "  ",
        ezproxy: Some(KCL),
        libkey_library_id: None,
    };
    let h = handoff(&only_ezproxy, &doi, None);
    assert!(h.redirector_url.is_none() && h.libkey_url.is_none() && h.ezproxy_url.is_some());
    let only_libkey = HandoffConf {
        libkey_library_id: Some(286),
        ..bare
    };
    let h = handoff(&only_libkey, &doi, None);
    assert!(h.libkey_url.is_some() && h.redirector_url.is_none() && h.ezproxy_url.is_none());
    // LibKey needs a DOI.
    assert_eq!(
        handoff(&only_libkey, &ids_with(None, Some("https://a.org/x")), None),
        Handoff::default()
    );
    // Two different institutions give different links.
    let other = HandoffConf {
        openathens_domain: "ox.ac.uk",
        ezproxy: None,
        libkey_library_id: Some(7),
    };
    let h = handoff(&other, &doi, None);
    assert!(h.redirector_url.unwrap().contains("/redirector/ox.ac.uk?"));
    assert_eq!(
        h.libkey_url.as_deref(),
        Some("https://libkey.io/libraries/7/10.1002/bjs.11646")
    );
}

#[test]
fn handoff_serialises_only_the_links_that_exist() {
    let bare = HandoffConf {
        openathens_domain: "",
        ezproxy: None,
        libkey_library_id: None,
    };
    let h = handoff(&bare, &ids_with(Some("10.1/x"), None), None);
    assert_eq!(
        serde_json::to_string(&h).unwrap(),
        r#"{"doi_url":"https://doi.org/10.1/x"}"#
    );
    let _ = urls::OPENATHENS_HOST;
}
