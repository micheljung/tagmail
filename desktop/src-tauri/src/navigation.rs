//! Where the main webview may go: only the app itself and its own blob
//! downloads. Web and mail links open in the system's handler; everything
//! else is dropped.

use url::Url;

/// The app's origin: the custom protocol, as WebView2 serves it on Windows.
#[cfg(windows)]
pub const ORIGIN: &str = "http://app.localhost/";
#[cfg(not(windows))]
pub const ORIGIN: &str = "app://localhost/";
const EXTERNAL_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

#[derive(Debug, PartialEq, Eq)]
pub enum Navigation {
    Allow,
    External(Url),
    Deny,
}

fn in_app(url: &Url) -> bool {
    let app = Url::parse(ORIGIN).expect("the app origin parses");
    let same = (url.scheme(), url.host_str(), url.port()) == (app.scheme(), app.host_str(), None);
    same || url.as_str() == "about:blank"
}

fn app_blob(url: &Url) -> bool {
    url.scheme() == "blob"
        && Url::parse(url.path()).is_ok_and(|inner| inner.scheme() != "blob" && in_app(&inner))
}

fn external(url: Url) -> Navigation {
    if EXTERNAL_SCHEMES.contains(&url.scheme()) {
        Navigation::External(url)
    } else {
        Navigation::Deny
    }
}

/// A navigation of the main webview or one of its frames.
pub fn navigate(url: &Url) -> Navigation {
    if in_app(url) || app_blob(url) {
        Navigation::Allow
    } else {
        external(url.clone())
    }
}

/// A new window (`target="_blank"`, `window.open`): never opened in-app.
pub fn new_window(url: Url) -> Navigation {
    external(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn only_the_app_stays_in_the_webview() {
        let app = url(ORIGIN);
        for inside in [
            app.clone(),
            app.join("thread/4?x=1").unwrap(),
            url("about:blank"),
            url(&format!(
                "blob:{ORIGIN}0b6c1d36-8a51-4d1e-9c52-2f6a4b0f3c11"
            )),
        ] {
            assert_eq!(navigate(&inside), Navigation::Allow, "{inside}");
        }
        let other_app = if cfg!(windows) {
            "app://localhost/"
        } else {
            "http://app.localhost/"
        };
        assert_ne!(navigate(&url(other_app)), Navigation::Allow);
        for (link, expected) in [
            ("https://example.com/a", true),
            ("http://example.com/", true),
            ("mailto:ann@x.example", true),
            ("app://other/", false),
            ("app://localhost:8080/", false),
            ("file:///etc/passwd", false),
            ("javascript:alert(1)", false),
            ("data:text/html,hi", false),
            ("tagmail://pair?server=x", false),
            ("blob:https://example.com/0b6c1d36", false),
            ("blob:null/0b6c1d36", false),
            ("blob:blob:app://localhost/0b6c1d36", false),
        ] {
            let decided = navigate(&url(link));
            assert_eq!(
                matches!(decided, Navigation::External(_)),
                expected,
                "{link}"
            );
            if !expected {
                assert_eq!(decided, Navigation::Deny, "{link}");
            }
        }
    }

    #[test]
    fn new_windows_never_open_in_app() {
        assert_eq!(new_window(url("app://localhost/inbox")), Navigation::Deny);
        assert_eq!(
            new_window(url("https://github.com/micheljung/tagmail/releases")),
            Navigation::External(url("https://github.com/micheljung/tagmail/releases"))
        );
        assert_eq!(new_window(url("file:///etc/passwd")), Navigation::Deny);
    }
}
