//! Allocation peak of rendering a large message: body text parts are
//! decoded up to a cap and attachments are never decoded, also when
//! reduced from a stream first.

use base64::Engine as _;
use peak_alloc::PeakAlloc;
use tagmail_api_core::mime;

#[global_allocator]
static ALLOC: PeakAlloc = PeakAlloc;

const MIB: usize = 1 << 20;

fn base64_lines(data: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = String::with_capacity(encoded.len() * 77 / 76 + 2);
    for line in encoded.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(line).unwrap());
        out.push_str("\r\n");
    }
    out
}

fn huge(html_bytes: usize, attachment_bytes: usize) -> Vec<u8> {
    let paragraph = "<p>Lorem ipsum <b>dolor</b> sit amet, consectetur adipiscing.</p>\r\n";
    let html = paragraph.repeat(html_bytes / paragraph.len());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.resize(attachment_bytes, 7);
    let mut inline = b"\x89PNG\r\n\x1a\n".to_vec();
    inline.resize(attachment_bytes / 4, 9);
    format!(
        concat!(
            "From: a@example.com\r\nSubject: huge\r\n",
            "Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n",
            "--m\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{html}\r\n",
            "--m\r\nContent-Type: image/png\r\nContent-Disposition: inline\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n{inline}",
            "--m\r\nContent-Type: image/png; name=big.png\r\n",
            "Content-Disposition: attachment; filename=big.png\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n{png}",
            "--m--\r\n",
        ),
        html = html,
        inline = base64_lines(&inline),
        png = base64_lines(&png),
    )
    .into_bytes()
}

#[test]
fn rendering_caps_body_text_and_never_decodes_attachments() {
    let attachment = 32 * MIB;
    let raw = huge(4 * MIB, attachment);
    let before = ALLOC.current_usage();
    ALLOC.reset_peak_usage();
    let message = mime::parse(&raw).unwrap();
    let body = mime::body(&message, &|id| format!("/api/parts/t{id}"), None);
    let parts = mime::parts(&message, &body.cids);
    let peak = ALLOC.peak_usage() - before;

    let budget = 24 * MIB;
    assert!(peak < budget, "peak {} KiB", peak / 1024);
    assert!(body.html.len() <= mime::BODY_TEXT_CAP + 1024);
    assert!(body.html.contains("Message too large to display fully"));
    let big = parts.iter().find(|p| p.name.as_deref() == Some("big.png"));
    assert_eq!(big.map(|p| p.size), Some(attachment as u64));

    let before = ALLOC.current_usage();
    ALLOC.reset_peak_usage();
    let reduced = mime::reduce::reduce(raw.as_slice()).unwrap();
    let message = mime::parse(&reduced).unwrap();
    let reduced_body = mime::body(&message, &|id| format!("/api/parts/t{id}"), None);
    let reduced_parts = mime::parts(&message, &reduced_body.cids);
    let peak = ALLOC.peak_usage() - before;
    assert!(peak < budget, "reduced peak {} KiB", peak / 1024);
    assert_eq!(reduced_parts, parts);
    assert_eq!(reduced_body, body);

    let raw = huge(MIB / 2, 1024);
    let message = mime::parse(&raw).unwrap();
    let body = mime::body(&message, &|id| format!("/api/parts/t{id}"), None);
    assert!(!body.html.contains("Message too large"));
    assert!(body.html.len() > MIB / 2 * 9 / 10);
}
