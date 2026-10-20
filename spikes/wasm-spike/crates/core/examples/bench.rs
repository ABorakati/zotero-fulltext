//! Native benchmark over real OA landing pages.
//! cargo run --release -p fulltext-core --example bench --features tl,lolhtml,html5gum,scraper
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fulltext_core::html::{self, Extracted};
use fulltext_core::logic::candidates_from;
use std::time::Instant;
use url::Url;

type Backend = (&'static str, fn(&str) -> Extracted);

fn main() {
    let backends: Vec<Backend> = vec![
        #[cfg(feature = "hand")]
        ("hand", html::hand::extract),
        #[cfg(feature = "tl")]
        ("tl", html::tl_impl::extract),
        #[cfg(feature = "html5gum")]
        ("html5gum", html::gum::extract),
        #[cfg(feature = "lolhtml")]
        ("lol_html", html::lol::extract),
        #[cfg(feature = "scraper")]
        ("scraper", html::scrap::extract),
    ];
    let pages = [
        ("plos-med", "https://journals.plos.org/plosmedicine/article?id=10.1371/journal.pmed.1000097"),
        ("arxiv", "https://arxiv.org/abs/1706.03762"),
        ("bmc-med", "https://link.springer.com/article/10.1186/1741-7015-8-18"),
        ("pmc", "https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/"),
        ("frontiers", "https://www.frontiersin.org/journals/psychology/articles/10.3389/fpsyg.2013.00863/full"),
    ];
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/");
    let iters = 20;
    for (name, url) in pages {
        let src = std::fs::read_to_string(format!("{dir}{name}.html")).unwrap();
        let page = Url::parse(url).unwrap();
        println!("== {name} ({} KB)", src.len() / 1024);
        let mut reference: Option<Vec<String>> = None;
        for (bname, f) in &backends {
            let ex = f(&src); // warm-up + result
            let t = Instant::now();
            for _ in 0..iters {
                std::hint::black_box(f(std::hint::black_box(&src)));
            }
            let per = t.elapsed().as_secs_f64() * 1000.0 / f64::from(iters);
            let cands = candidates_from(&ex, &page);
            let top: Vec<String> = cands.iter().take(3).map(|c| format!("{}[{}]", c.url, c.source)).collect();
            let pdf_meta = ex.metas.iter().find(|m| m.name == "citation_pdf_url").map(|m| m.content.clone());
            let urls: Vec<String> = cands.iter().map(|c| c.url.clone()).collect();
            let agree = match &reference {
                None => {
                    reference = Some(urls.clone());
                    "ref".to_string()
                }
                Some(r) => {
                    if *r == urls {
                        "same".into()
                    } else {
                        format!("DIFF({} vs {})", urls.len(), r.len())
                    }
                }
            };
            println!(
                "  {bname:<9} {per:7.2} ms  metas={:<3} links={:<3} anchors={:<4} iframes={:<2} pdf_meta={} cands={} {agree}",
                ex.metas.len(),
                ex.links.len(),
                ex.anchors.len(),
                ex.iframes.len(),
                pdf_meta.as_deref().unwrap_or("-"),
                cands.len()
            );
            if *bname == "hand" {
                for t in top {
                    println!("              top: {t}");
                }
            }
        }
    }
}
