//! The e2e rendering corpus (`web/e2e/corpus/*.eml`) rendered through the
//! render route's path (parse, sanitize, base document) and diffed against
//! `web/e2e/corpus/rendered/<name>.html`, which the e2e mock serves; with
//! author CSS on (`--render-author-css`, B1) against `<name>.b1.html`.

use std::path::{Path, PathBuf};

use tagmail_api_core::mime;

/// Set to rewrite the golden files from the current output.
const BLESS: &str = "TAGMAIL_BLESS";

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/e2e/corpus")
}

/// What the render route serves for `raw` with remote images blocked.
fn render(raw: &[u8], author_css: bool) -> String {
    let message = mime::parse(raw).expect("corpus mail parses");
    let body = mime::body_with(
        &message,
        &|part| format!("/api/parts/corpus-{part}"),
        None,
        author_css,
    );
    mime::document(&body.html)
}

#[test]
fn corpus_renders_match_their_golden_files() {
    let dir = corpus_dir();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension()? == "eml").then(|| path.file_stem()?.to_str().map(str::to_owned))?
        })
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no corpus mail in {}", dir.display());

    let manifest: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let mut listed: Vec<String> = manifest
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_owned())
        .collect();
    listed.sort();
    assert_eq!(
        names, listed,
        "every corpus .eml is listed in manifest.json"
    );

    let bless = std::env::var_os(BLESS).is_some();
    for name in &names {
        let raw = std::fs::read(dir.join(format!("{name}.eml"))).unwrap();
        for (author_css, file) in [
            (false, format!("{name}.html")),
            (true, format!("{name}.b1.html")),
        ] {
            let got = render(&raw, author_css);
            let path = dir.join("rendered").join(file);
            if bless {
                std::fs::write(&path, &got).unwrap();
                continue;
            }
            let expected = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e} (run with {BLESS}=1)", path.display()));
            assert_eq!(
                got,
                expected,
                "{}: rerun with {BLESS}=1 if the change is intended",
                path.display()
            );
        }
    }
}
