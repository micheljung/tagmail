//! Server capabilities and the capability-driven FETCH attribute lists.

use std::collections::BTreeSet;

/// The capabilities a server advertised, uppercased (`AUTH=PLAIN`, `QRESYNC`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    names: BTreeSet<String>,
}

impl Capabilities {
    pub fn has(&self, name: &str) -> bool {
        self.names.contains(&name.to_ascii_uppercase())
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.names
            .iter()
            .map(String::as_str)
            .filter(|n| *n != REMOVED_CONDSTORE)
    }

    pub fn condstore(&self) -> bool {
        self.has("CONDSTORE") || (self.has("QRESYNC") && !self.removed_condstore())
    }

    /// QRESYNC implies CONDSTORE (RFC 7162); a CONDSTORE override also
    /// disables QRESYNC.
    pub fn qresync(&self) -> bool {
        self.has("QRESYNC") && self.condstore()
    }

    pub fn enable(&self) -> bool {
        self.has("ENABLE")
    }

    pub fn idle(&self) -> bool {
        self.has("IDLE")
    }

    pub fn r#move(&self) -> bool {
        self.has("MOVE")
    }

    pub fn uidplus(&self) -> bool {
        self.has("UIDPLUS")
    }

    pub fn special_use(&self) -> bool {
        self.has("SPECIAL-USE")
    }

    pub fn objectid(&self) -> bool {
        self.has("OBJECTID")
    }

    /// RFC 5464 server annotations (`METADATA` or `METADATA-SERVER`).
    pub fn metadata(&self) -> bool {
        self.has("METADATA") || self.has("METADATA-SERVER")
    }

    pub fn gmail(&self) -> bool {
        self.has("X-GM-EXT-1")
    }

    pub fn sasl_ir(&self) -> bool {
        self.has("SASL-IR")
    }

    pub fn auth_plain(&self) -> bool {
        self.has("AUTH=PLAIN")
    }

    pub fn login_disabled(&self) -> bool {
        self.has("LOGINDISABLED")
    }

    pub fn starttls(&self) -> bool {
        self.has("STARTTLS")
    }

    fn removed_condstore(&self) -> bool {
        self.names.contains(REMOVED_CONDSTORE)
    }
}

/// Marker kept when an override removes CONDSTORE from a QRESYNC server.
const REMOVED_CONDSTORE: &str = "-CONDSTORE";

impl<S: AsRef<str>> FromIterator<S> for Capabilities {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        Self {
            names: iter
                .into_iter()
                .map(|s| s.as_ref().trim().to_ascii_uppercase())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }
}

/// Hides or adds capabilities, e.g. to test the no-CONDSTORE paths
/// against a server that has it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityOverride {
    pub remove: Vec<String>,
    pub add: Vec<String>,
}

impl CapabilityOverride {
    pub fn apply(&self, caps: &mut Capabilities) {
        for name in &self.remove {
            let name = name.to_ascii_uppercase();
            if name == "CONDSTORE" {
                caps.names.insert(REMOVED_CONDSTORE.to_owned());
            }
            caps.names.remove(&name);
        }
        for name in &self.add {
            caps.names.insert(name.to_ascii_uppercase());
        }
    }
}

/// What to FETCH. UID is always fetched; MODSEQ and the message identity
/// are only requested when the server supports them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchSpec {
    pub flags: bool,
    pub internal_date: bool,
    pub size: bool,
    pub envelope: bool,
    pub body_structure: bool,
    /// `BODY.PEEK[HEADER.FIELDS (...)]` when not empty.
    pub header_fields: Vec<String>,
    /// `BODY.PEEK[1]<0.n>` for a snippet.
    pub snippet: Option<u32>,
    /// MODSEQ, under CONDSTORE only.
    pub modseq: bool,
    /// EMAILID/THREADID under OBJECTID, X-GM-MSGID/X-GM-THRID on Gmail.
    pub identity: bool,
}

pub const SNIPPET_BYTES: u32 = 2048;

impl FetchSpec {
    /// The initial-sync window attributes (plan §3.4 step 4).
    pub fn sync_window(snippet: bool) -> Self {
        Self {
            flags: true,
            internal_date: true,
            size: true,
            envelope: true,
            body_structure: true,
            header_fields: ["REFERENCES", "IN-REPLY-TO", "LIST-ID"]
                .map(String::from)
                .to_vec(),
            snippet: snippet.then_some(SNIPPET_BYTES),
            modseq: true,
            identity: true,
        }
    }

    pub fn flags() -> Self {
        Self {
            flags: true,
            modseq: true,
            ..Self::default()
        }
    }
}

/// The message identity attributes the server supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityAttr {
    /// RFC 8474 `EMAILID THREADID`.
    ObjectId,
    /// Gmail `X-GM-MSGID X-GM-THRID`.
    Gmail,
}

impl IdentityAttr {
    pub fn for_caps(caps: &Capabilities) -> Option<Self> {
        if caps.objectid() {
            Some(Self::ObjectId)
        } else if caps.gmail() {
            Some(Self::Gmail)
        } else {
            None
        }
    }

    pub(crate) fn items(self) -> &'static str {
        match self {
            Self::ObjectId => "EMAILID THREADID",
            Self::Gmail => "X-GM-MSGID X-GM-THRID",
        }
    }
}

/// One attribute in a planned FETCH.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchAttr {
    Uid,
    Flags,
    InternalDate,
    Size,
    Envelope,
    BodyStructure,
    HeaderFields(Vec<String>),
    Snippet(u32),
    ModSeq,
}

/// A FETCH split by codec support: `attrs` are sent typed; `identity`
/// needs a raw command because imap-codec cannot encode or parse it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchPlan {
    pub attrs: Vec<FetchAttr>,
    pub identity: Option<IdentityAttr>,
}

impl FetchPlan {
    /// `nomodseq` is the selected mailbox's `NOMODSEQ` state.
    pub fn new(caps: &Capabilities, spec: &FetchSpec, nomodseq: bool) -> Self {
        let mut attrs = vec![FetchAttr::Uid];
        if spec.flags {
            attrs.push(FetchAttr::Flags);
        }
        if spec.internal_date {
            attrs.push(FetchAttr::InternalDate);
        }
        if spec.size {
            attrs.push(FetchAttr::Size);
        }
        if spec.envelope {
            attrs.push(FetchAttr::Envelope);
        }
        if spec.body_structure {
            attrs.push(FetchAttr::BodyStructure);
        }
        if !spec.header_fields.is_empty() {
            attrs.push(FetchAttr::HeaderFields(spec.header_fields.clone()));
        }
        if let Some(n) = spec.snippet {
            attrs.push(FetchAttr::Snippet(n));
        }
        if spec.modseq && caps.condstore() && !nomodseq {
            attrs.push(FetchAttr::ModSeq);
        }
        let identity = spec
            .identity
            .then(|| IdentityAttr::for_caps(caps))
            .flatten();
        Self { attrs, identity }
    }

    /// The attribute names as they appear on the wire, for logs and tests.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .attrs
            .iter()
            .map(|a| match a {
                FetchAttr::Uid => "UID".into(),
                FetchAttr::Flags => "FLAGS".into(),
                FetchAttr::InternalDate => "INTERNALDATE".into(),
                FetchAttr::Size => "RFC822.SIZE".into(),
                FetchAttr::Envelope => "ENVELOPE".into(),
                FetchAttr::BodyStructure => "BODYSTRUCTURE".into(),
                FetchAttr::HeaderFields(f) => {
                    format!("BODY.PEEK[HEADER.FIELDS ({})]", f.join(" "))
                }
                FetchAttr::Snippet(n) => format!("BODY.PEEK[1]<0.{n}>"),
                FetchAttr::ModSeq => "MODSEQ".into(),
            })
            .collect();
        if let Some(id) = self.identity {
            names.extend(id.items().split(' ').map(String::from));
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(names: &[&str]) -> Capabilities {
        names.iter().collect()
    }

    #[test]
    fn modseq_only_with_condstore_and_not_nomodseq() {
        let spec = FetchSpec::sync_window(false);
        let plain = FetchPlan::new(&caps(&["IMAP4rev1"]), &spec, false);
        assert!(!plain.names().contains(&"MODSEQ".into()));
        let cs = FetchPlan::new(&caps(&["CONDSTORE"]), &spec, false);
        assert!(cs.names().contains(&"MODSEQ".into()));
        let qr = FetchPlan::new(&caps(&["QRESYNC"]), &spec, false);
        assert!(qr.names().contains(&"MODSEQ".into()));
        let nomodseq = FetchPlan::new(&caps(&["CONDSTORE"]), &spec, true);
        assert!(!nomodseq.names().contains(&"MODSEQ".into()));
    }

    #[test]
    fn identity_only_with_objectid_or_gmail() {
        let spec = FetchSpec::sync_window(true);
        assert_eq!(
            FetchPlan::new(&caps(&["IMAP4rev1"]), &spec, false).identity,
            None
        );
        let oid = FetchPlan::new(&caps(&["OBJECTID", "X-GM-EXT-1"]), &spec, false);
        assert_eq!(oid.identity, Some(IdentityAttr::ObjectId));
        assert!(oid.names().contains(&"EMAILID".into()));
        let gm = FetchPlan::new(&caps(&["X-GM-EXT-1"]), &spec, false);
        assert!(gm.names().contains(&"X-GM-MSGID".into()));
        assert!(!gm.names().contains(&"EMAILID".into()));
        assert!(gm.names().contains(&"BODY.PEEK[1]<0.2048>".into()));
    }

    #[test]
    fn override_removing_condstore_disables_qresync() {
        let mut c = caps(&["QRESYNC", "CONDSTORE", "IDLE"]);
        assert!(c.qresync() && c.condstore());
        CapabilityOverride {
            remove: vec!["condstore".into()],
            add: vec!["OBJECTID".into()],
        }
        .apply(&mut c);
        assert!(!c.condstore() && !c.qresync() && c.objectid() && c.idle());

        let mut c = caps(&["QRESYNC", "CONDSTORE"]);
        CapabilityOverride {
            remove: vec!["QRESYNC".into()],
            add: vec![],
        }
        .apply(&mut c);
        assert!(c.condstore() && !c.qresync());
    }
}
