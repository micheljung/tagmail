//! UID FETCH: typed attributes, the raw identity fetch, windows and
//! conversion of codec types into [`MessageMeta`].

use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroU64};

use imap_next::imap_types::body::{BodyStructure, SpecificFields};
use imap_next::imap_types::command::{CommandBody, FetchModifier};
use imap_next::imap_types::core::{AString, IString, NString, Vec1};
use imap_next::imap_types::envelope::Address as ImapAddress;
use imap_next::imap_types::fetch::{
    MacroOrMessageDataItemNames, MessageDataItem, MessageDataItemName, Part, Section,
};
use imap_next::imap_types::flag::FlagFetch;
use imap_next::imap_types::mailbox::Mailbox;
use imap_next::imap_types::sequence::{SeqOrUid, Sequence, SequenceSet};
use mail_parser::Charset;

use crate::caps::{FetchAttr, FetchPlan, FetchSpec};
use crate::error::{Error, Result};
use crate::session::TokioSession;
use crate::types::{
    Address, BodySummary, Changes, Envelope, Flag, MessageMeta, ModSeq, PartInfo, Uid, UidSet,
};

/// UIDs per initial-sync FETCH window.
pub const WINDOW: usize = 500;

/// Splits `uids` into windows of at most `size` UIDs, newest first.
pub fn windows_newest_first(uids: &UidSet, size: usize) -> Vec<UidSet> {
    let size = size.max(1);
    let all: Vec<Uid> = uids.iter().rev().collect();
    all.chunks(size)
        .map(|c| c.iter().copied().collect())
        .collect()
}

pub(crate) fn sequence_set(uids: &UidSet) -> Result<SequenceSet> {
    let seqs: Vec<Sequence> = uids
        .ranges()
        .iter()
        .map(|&(lo, hi)| {
            let lo = NonZeroU32::new(lo).ok_or_else(|| Error::InvalidArgument("UID 0".into()))?;
            let hi = NonZeroU32::new(hi).ok_or_else(|| Error::InvalidArgument("UID 0".into()))?;
            Ok(if lo == hi {
                Sequence::Single(SeqOrUid::Value(lo))
            } else {
                Sequence::Range(SeqOrUid::Value(lo), SeqOrUid::Value(hi))
            })
        })
        .collect::<Result<_>>()?;
    SequenceSet::try_from(seqs).map_err(|_| Error::InvalidArgument("empty UID set".into()))
}

/// Converts a server-sent UID set; `*` cannot appear in VANISHED.
pub(crate) fn uid_set(set: &SequenceSet) -> UidSet {
    let mut out = UidSet::new();
    let value = |s: &SeqOrUid| match s {
        SeqOrUid::Value(v) => v.get(),
        SeqOrUid::Asterisk => u32::MAX,
    };
    for seq in set.0.as_ref() {
        match seq {
            Sequence::Single(s) => out.insert_range(value(s), value(s)),
            Sequence::Range(a, b) => out.insert_range(value(a), value(b)),
        }
    }
    out
}

pub(crate) fn mailbox_name(mailbox: &Mailbox<'_>) -> String {
    match mailbox {
        Mailbox::Inbox => "INBOX".to_owned(),
        Mailbox::Other(other) => String::from_utf8_lossy(other.as_ref()).into_owned(),
    }
}

fn nstring(s: &NString<'_>) -> Option<Vec<u8>> {
    s.0.as_ref().map(|s| s.as_ref().to_vec())
}

fn istring_lower(s: &IString<'_>) -> String {
    String::from_utf8_lossy(s.as_ref()).to_ascii_lowercase()
}

fn addresses(list: &[ImapAddress<'_>]) -> Vec<Address> {
    list.iter()
        .map(|a| Address {
            name: nstring(&a.name),
            mailbox: nstring(&a.mailbox),
            host: nstring(&a.host),
        })
        .collect()
}

fn param<'a>(params: &'a [(IString<'_>, IString<'_>)], name: &str) -> Option<&'a IString<'a>> {
    params
        .iter()
        .find(|(k, _)| k.as_ref().eq_ignore_ascii_case(name.as_bytes()))
        .map(|(_, v)| v)
}

/// An RFC 2231 parameter as UTF-8: `name*`, else the continuations
/// `name*0`, `name*1*`, ... joined in order, percent-decoded where marked
/// and converted from the declared charset.
fn extended_param(params: &[(IString<'_>, IString<'_>)], name: &str) -> Option<Vec<u8>> {
    let suffix = |key: &[u8]| -> Option<String> {
        let key = std::str::from_utf8(key).ok()?;
        let head = key.get(..name.len())?;
        head.eq_ignore_ascii_case(name)
            .then(|| key[name.len()..].strip_prefix('*').map(str::to_owned))
            .flatten()
    };
    let mut segments: Vec<(u32, bool, &[u8])> = Vec::new();
    for (k, v) in params {
        let Some(rest) = suffix(k.as_ref()) else {
            continue;
        };
        if rest.is_empty() {
            segments = vec![(0, true, v.as_ref())];
            break;
        }
        let (index, encoded) = match rest.strip_suffix('*') {
            Some(index) => (index, true),
            None => (rest.as_str(), false),
        };
        if let Ok(index) = index.parse() {
            segments.push((index, encoded, v.as_ref()));
        }
    }
    segments.sort_by_key(|(index, _, _)| *index);
    let mut charset = None;
    let mut out = Vec::new();
    for (i, (_, encoded, value)) in segments.into_iter().enumerate() {
        if !encoded {
            out.extend_from_slice(value);
            continue;
        }
        let mut value = value;
        if i == 0 {
            let mut fields = value.splitn(3, |b| *b == b'\'');
            if let (Some(declared), Some(_), Some(rest)) =
                (fields.next(), fields.next(), fields.next())
            {
                charset = Charset::from_label(declared);
                value = rest;
            }
        }
        out.extend(percent_decode(value));
    }
    if out.is_empty() {
        return None;
    }
    Some(match charset {
        Some(charset) => charset.decode(&out).into_owned().into_bytes(),
        None => out,
    })
}

fn percent_decode(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
    let mut i = 0;
    while i < value.len() {
        let hex = value
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (value[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

fn collect_parts(body: &BodyStructure<'_>, prefix: &str, out: &mut Vec<PartInfo>) {
    match body {
        BodyStructure::Single {
            body,
            extension_data,
        } => {
            let section = if prefix.is_empty() {
                "1".to_owned()
            } else {
                prefix.to_owned()
            };
            let mime_type = match &body.specific {
                SpecificFields::Basic { r#type, subtype } => {
                    format!("{}/{}", istring_lower(r#type), istring_lower(subtype))
                }
                SpecificFields::Message { .. } => "message/rfc822".to_owned(),
                SpecificFields::Text { subtype, .. } => format!("text/{}", istring_lower(subtype)),
            };
            let params = &body.basic.parameter_list;
            let disposition = extension_data
                .as_ref()
                .and_then(|e| e.tail.as_ref())
                .and_then(|d| d.disposition.as_ref());
            let filename = disposition
                .and_then(|(_, p)| {
                    extended_param(p, "filename")
                        .or_else(|| param(p, "filename").map(|v| v.as_ref().to_vec()))
                })
                .or_else(|| extended_param(params, "name"))
                .or_else(|| param(params, "name").map(|v| v.as_ref().to_vec()));
            out.push(PartInfo {
                section,
                mime_type,
                charset: param(params, "charset").map(istring_lower),
                encoding: istring_lower(&body.basic.content_transfer_encoding),
                size: body.basic.size,
                disposition: disposition.map(|(d, _)| istring_lower(d)),
                filename,
            });
        }
        BodyStructure::Multi { bodies, .. } => {
            for (i, child) in bodies.as_ref().iter().enumerate() {
                let section = if prefix.is_empty() {
                    (i + 1).to_string()
                } else {
                    format!("{prefix}.{}", i + 1)
                };
                collect_parts(child, &section, out);
            }
        }
    }
}

pub(crate) fn body_summary(body: &BodyStructure<'_>) -> BodySummary {
    let mut parts = Vec::new();
    collect_parts(body, "", &mut parts);
    BodySummary { parts }
}

pub(crate) fn meta_from_items(seq: u32, items: &[MessageDataItem<'_>]) -> MessageMeta {
    let mut meta = MessageMeta {
        seq,
        ..MessageMeta::default()
    };
    for item in items {
        match item {
            MessageDataItem::Uid(uid) => meta.uid = Some(Uid(uid.get())),
            MessageDataItem::Flags(flags) => {
                meta.flags = Some(
                    flags
                        .iter()
                        .filter_map(|f| match f {
                            FlagFetch::Flag(f) => Flag::parse(&f.to_string()),
                            FlagFetch::Recent => None,
                        })
                        .collect(),
                );
            }
            MessageDataItem::ModSeq(m) => meta.modseq = Some(ModSeq(m.get())),
            MessageDataItem::InternalDate(d) => meta.internal_date = Some(d.as_ref().timestamp()),
            MessageDataItem::Rfc822Size(n) => meta.size = Some(*n),
            MessageDataItem::Envelope(e) => {
                meta.envelope = Some(Envelope {
                    date: nstring(&e.date),
                    subject: nstring(&e.subject),
                    from: addresses(&e.from),
                    sender: addresses(&e.sender),
                    reply_to: addresses(&e.reply_to),
                    to: addresses(&e.to),
                    cc: addresses(&e.cc),
                    bcc: addresses(&e.bcc),
                    in_reply_to: nstring(&e.in_reply_to),
                    message_id: nstring(&e.message_id),
                });
            }
            MessageDataItem::BodyStructure(b) | MessageDataItem::Body(b) => {
                meta.body = Some(body_summary(b));
            }
            MessageDataItem::BodyExt { section, data, .. } => match section {
                Some(Section::HeaderFields(None, _)) => meta.header_fields = nstring(data),
                Some(Section::Part(Part(p))) if p.as_ref() == [NonZeroU32::MIN] => {
                    meta.snippet = nstring(data);
                }
                _ => {}
            },
            _ => {}
        }
    }
    meta
}

pub(crate) fn item_names(plan: &FetchPlan) -> Result<Vec<MessageDataItemName<'static>>> {
    plan.attrs
        .iter()
        .map(|a| {
            Ok(match a {
                FetchAttr::Uid => MessageDataItemName::Uid,
                FetchAttr::Flags => MessageDataItemName::Flags,
                FetchAttr::InternalDate => MessageDataItemName::InternalDate,
                FetchAttr::Size => MessageDataItemName::Rfc822Size,
                FetchAttr::Envelope => MessageDataItemName::Envelope,
                FetchAttr::BodyStructure => MessageDataItemName::BodyStructure,
                FetchAttr::ModSeq => MessageDataItemName::ModSeq,
                FetchAttr::HeaderFields(fields) => {
                    let fields = fields
                        .iter()
                        .map(|f| {
                            AString::try_from(f.clone())
                                .map_err(|e| Error::InvalidArgument(format!("header {f:?}: {e}")))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let fields = Vec1::try_from(fields)
                        .map_err(|_| Error::InvalidArgument("no header fields".into()))?;
                    MessageDataItemName::BodyExt {
                        section: Some(Section::HeaderFields(None, fields)),
                        partial: None,
                        peek: true,
                    }
                }
                FetchAttr::Snippet(n) => MessageDataItemName::BodyExt {
                    section: Some(Section::Part(Part(Vec1::from(NonZeroU32::MIN)))),
                    partial: Some((
                        0,
                        NonZeroU32::new(*n)
                            .ok_or_else(|| Error::InvalidArgument("snippet size 0".into()))?,
                    )),
                    peek: true,
                },
            })
        })
        .collect()
}

fn merge_identity(target: &mut MessageMeta, id: MessageMeta) {
    target.email_id = id.email_id.or(target.email_id.take());
    target.thread_id = id.thread_id.or(target.thread_id.take());
    target.gmail_msgid = id.gmail_msgid.or(target.gmail_msgid);
    target.gmail_thrid = id.gmail_thrid.or(target.gmail_thrid);
}

impl TokioSession {
    pub(crate) async fn fetch_impl(
        &mut self,
        uids: &UidSet,
        spec: &FetchSpec,
        changed_since: Option<ModSeq>,
    ) -> Result<Changes> {
        let nomodseq = self.selected_state()?.nomodseq;
        if uids.is_empty() {
            return Ok(Changes::default());
        }
        let plan = FetchPlan::new(&self.caps, spec, nomodseq);
        let set = sequence_set(uids)?;
        let mut modifiers = Vec::new();
        if let Some(since) = changed_since.and_then(|m| NonZeroU64::new(m.0)) {
            modifiers.push(FetchModifier::ChangedSince(since));
            if self.qresync {
                modifiers.push(FetchModifier::Vanished);
            }
        }
        let body = CommandBody::Fetch {
            sequence_set: set,
            macro_or_item_names: MacroOrMessageDataItemNames::MessageDataItemNames(item_names(
                &plan,
            )?),
            uid: true,
            modifiers,
        };
        let (out, _) = self.run(body).await?;
        let too_long = out.too_long;
        let (mut messages, vanished) =
            self.split_events(out, |m| m.uid.is_some_and(|u| uids.contains(u)));
        if too_long {
            tracing::warn!(set = %uids, "dropped a FETCH response over the size limit");
        }
        if let Some(identity) = plan.identity
            && !messages.is_empty()
        {
            let fetched: UidSet = messages.iter().filter_map(|m| m.uid).collect();
            let line = format!("UID FETCH {fetched} (UID {})", identity.items());
            let (mut out, _) = self.run_raw(&line).await?;
            let raw = std::mem::take(&mut out.raw_fetches);
            self.split_events(out, |_| false);
            let mut ids: HashMap<Uid, MessageMeta> =
                raw.into_iter().filter_map(|m| Some((m.uid?, m))).collect();
            for meta in &mut messages {
                if let Some(id) = meta.uid.and_then(|u| ids.remove(&u)) {
                    merge_identity(meta, id);
                }
            }
        }
        Ok(Changes { messages, vanished })
    }
}

#[cfg(test)]
mod tests {
    use imap_codec::ResponseCodec;
    use imap_codec::decode::Decoder;
    use imap_next::imap_types::response::{Data, Response};

    use super::*;

    fn decode_fetch(line: &[u8]) -> MessageMeta {
        let (_, resp) = ResponseCodec::default().decode(line).expect("decodes");
        match resp {
            Response::Data(Data::Fetch { seq, items }) => {
                meta_from_items(seq.get(), items.as_ref())
            }
            other => panic!("not a FETCH: {other:?}"),
        }
    }

    #[test]
    fn windows_are_newest_first_and_bounded() {
        let mut uids = UidSet::new();
        uids.insert_range(1, 1203);
        let w = windows_newest_first(&uids, WINDOW);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].ranges(), &[(704, 1203)]);
        assert_eq!(w[2].ranges(), &[(1, 203)]);
        assert!(windows_newest_first(&UidSet::new(), WINDOW).is_empty());
    }

    #[test]
    fn sequence_set_round_trip() {
        let uids: UidSet = [1, 2, 3, 7, 9, 10].into_iter().map(Uid).collect();
        let set = sequence_set(&uids).unwrap();
        assert_eq!(uid_set(&set), uids);
    }

    #[test]
    fn converts_envelope_flags_and_sections() {
        let m = decode_fetch(
            b"* 4 FETCH (UID 17 FLAGS (\\Seen \\Recent $label_Work) MODSEQ (99) RFC822.SIZE 321 \
              INTERNALDATE \"22-Nov-2026 10:00:00 +0100\" \
              ENVELOPE (\"Sun, 22 Nov 2026 10:00:00 +0000\" \"=?UTF-8?Q?Gr=C3=BC=C3=9Fe?=\" \
              ((\"Ann\" NIL \"ann\" \"example.com\")) NIL NIL ((NIL NIL \"bob\" \"example.org\")) NIL NIL \
              \"<p@x>\" \"<m@x>\") \
              BODY[HEADER.FIELDS (REFERENCES)] {16}\r\nReferences: <p>\n BODY[1]<0> {4}\r\nbody)\r\n",
        );
        assert_eq!(m.uid, Some(Uid(17)));
        assert_eq!(m.modseq, Some(ModSeq(99)));
        assert_eq!(m.size, Some(321));
        assert_eq!(m.internal_date, Some(1_795_338_000));
        assert_eq!(
            m.flags.unwrap(),
            vec![Flag::Seen, Flag::Keyword("$label_Work".into())]
        );
        let env = m.envelope.unwrap();
        assert_eq!(env.from[0].name.as_deref(), Some(&b"Ann"[..]));
        assert_eq!(env.to[0].host.as_deref(), Some(&b"example.org"[..]));
        assert_eq!(env.message_id.as_deref(), Some(&b"<m@x>"[..]));
        assert_eq!(m.header_fields.as_deref(), Some(&b"References: <p>\n"[..]));
        assert_eq!(m.snippet.as_deref(), Some(&b"body"[..]));
    }

    #[test]
    fn body_structure_sections_and_attachments() {
        let m = decode_fetch(
            b"* 1 FETCH (UID 1 BODYSTRUCTURE (((\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"quoted-printable\" 10 1 NIL NIL NIL NIL)\
              (\"text\" \"html\" (\"charset\" \"utf-8\") NIL NIL \"7bit\" 20 1 NIL NIL NIL NIL) \"alternative\" (\"boundary\" \"y\") NIL NIL NIL)\
              (\"application\" \"pdf\" (\"name\" \"a.pdf\") NIL NIL \"base64\" 100 NIL (\"attachment\" (\"filename\" \"a.pdf\")) NIL NIL) \
              \"mixed\" (\"boundary\" \"x\") NIL NIL NIL))\r\n",
        );
        let body = m.body.unwrap();
        let sections: Vec<_> = body.parts.iter().map(|p| p.section.as_str()).collect();
        assert_eq!(sections, ["1.1", "1.2", "2"]);
        assert!(body.has_attachment());
        let text = body.text_part().unwrap();
        assert_eq!(
            (
                text.section.as_str(),
                text.charset.as_deref(),
                text.encoding.as_str()
            ),
            ("1.1", Some("utf-8"), "quoted-printable")
        );
        assert_eq!(body.parts[2].filename.as_deref(), Some(&b"a.pdf"[..]));

        let extended = decode_fetch(
            b"* 1 FETCH (UID 1 BODYSTRUCTURE ((\"application\" \"octet-stream\" (\"name*\" \"utf-8''Termin%20A.ics\") NIL NIL \"base64\" 10 NIL NIL NIL NIL)\
              (\"application\" \"octet-stream\" NIL NIL NIL \"base64\" 10 NIL (\"attachment\" (\"filename*0*\" \"utf-8''Ter%C3%A9\" \"filename*1\" \"min.ics\")) NIL NIL) \
              \"mixed\" (\"boundary\" \"x\") NIL NIL NIL))\r\n",
        );
        let names: Vec<_> = extended
            .body
            .unwrap()
            .parts
            .into_iter()
            .map(|p| p.filename)
            .collect();
        assert_eq!(
            names,
            [
                Some(b"Termin A.ics".to_vec()),
                Some("Terémin.ics".as_bytes().to_vec())
            ]
        );

        let preferred = decode_fetch(
            b"* 1 FETCH (UID 1 BODYSTRUCTURE ((\"application\" \"octet-stream\" (\"name\" \"plain.bin\" \"name*\" \"utf-8''real.ics\" \"name*0*\" \"utf-8''other\") NIL NIL \"base64\" 10 NIL NIL NIL NIL)\
              (\"application\" \"octet-stream\" NIL NIL NIL \"base64\" 10 NIL (\"attachment\" (\"filename\" \"x.bin\" \"filename*\" \"iso-8859-1'fr'Caf%E9.ics\")) NIL NIL) \
              \"mixed\" (\"boundary\" \"x\") NIL NIL NIL))\r\n",
        );
        let names: Vec<_> = preferred
            .body
            .unwrap()
            .parts
            .into_iter()
            .map(|p| p.filename)
            .collect();
        assert_eq!(
            names,
            [
                Some(b"real.ics".to_vec()),
                Some("Café.ics".as_bytes().to_vec())
            ]
        );

        let single = decode_fetch(
            b"* 1 FETCH (BODYSTRUCTURE (\"text\" \"plain\" (\"charset\" \"us-ascii\") NIL NIL \"7bit\" 10 1 NIL NIL NIL NIL))\r\n",
        );
        assert_eq!(single.body.unwrap().parts[0].section, "1");
    }
}
