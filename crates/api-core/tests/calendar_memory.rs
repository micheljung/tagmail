//! Allocation peak of extracting the invitation of a large message: the
//! message is streamed, so memory follows the calendar caps, not its size.

use std::io::{self, BufReader, Read};

use peak_alloc::PeakAlloc;
use tagmail_api_core::mime::reduce::{CALENDAR_PART_CAP, calendar_parts};

#[global_allocator]
static ALLOC: PeakAlloc = PeakAlloc;

const MESSAGE_SIZE: u64 = 25 << 20;
const INVITE: &str = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR";

#[test]
fn extracting_from_a_25_mb_stream_stays_bounded() {
    let head = concat!(
        "From: org@example.com\r\n",
        "Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n",
        "--m\r\nContent-Type: application/octet-stream; name=big.bin\r\n\r\n",
    );
    let tail = format!("\r\n--m\r\nContent-Type: text/calendar\r\n\r\n{INVITE}\r\n--m--\r\n");
    let stream = io::Cursor::new(head)
        .chain(io::repeat(b'A').take(MESSAGE_SIZE))
        .chain(io::Cursor::new(tail));

    let before = ALLOC.current_usage();
    ALLOC.reset_peak_usage();
    let found = calendar_parts(BufReader::new(stream)).unwrap();
    let peak = ALLOC.peak_usage() - before;

    assert_eq!(found.parts.len(), 1);
    assert_eq!(found.parts[0].text, INVITE);
    assert!(peak < CALENDAR_PART_CAP, "peak {} KiB", peak / 1024);
}
