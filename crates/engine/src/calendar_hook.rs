//! What mail sync tells the calendar about invitations (appointments plan
//! §3.1): it only records the ids of recent messages with a calendar part
//! in `cal_inbox`, in the message's own cache transaction, and wakes the
//! calendar runner after the commit. Without the `calendar` feature no hook
//! is installed and nothing is recorded.

/// Invitations received longer ago than this (by INTERNALDATE, which the
/// sender cannot choose) are not applied.
pub(crate) const INVITE_MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60;

pub(crate) trait InviteHook: Send + Sync {
    /// New `cal_inbox` rows were committed.
    fn queued(&self);
}
