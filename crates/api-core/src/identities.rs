//! Identities (From addresses) and their signatures: validation of the
//! CRUD requests and the choice of the identity a reply is sent from.

use tagmail_core::AccountId;

/// A validated `POST /api/identities`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIdentity {
    pub account_id: AccountId,
    pub address: String,
    pub name: Option<String>,
    /// Sanitized HTML.
    pub signature: Option<String>,
    pub is_default: bool,
}

/// A validated `PATCH /api/identities/{id}`. `Some(None)` clears.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityPatch {
    pub address: Option<String>,
    pub name: Option<Option<String>>,
    /// Sanitized HTML.
    pub signature: Option<Option<String>>,
    /// Make this the account's default, demoting the previous one.
    pub make_default: bool,
}
