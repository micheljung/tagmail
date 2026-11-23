//! The in-memory pending overlay of every account (plan §3.4 Writes): the
//! open ops from `pending_op`, shared by the API (optimistic apply), the
//! executor and every cache write, which projects it over server state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tagmail_core::AccountId;
use tagmail_core::ops::{ArchiveMode, Op, Overlay};

/// One account's overlay as of one moment; cache writes hold a snapshot.
#[derive(Debug, Clone, Default)]
pub struct AccountOverlay {
    /// `account.archive_mode`; `None` lets the mailboxes decide.
    pub archive_mode: Option<ArchiveMode>,
    pub ops: Overlay,
}

#[derive(Debug, Default)]
pub struct Overlays {
    accounts: Mutex<HashMap<AccountId, Arc<AccountOverlay>>>,
}

impl Overlays {
    pub fn get(&self, account: AccountId) -> Arc<AccountOverlay> {
        self.accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&account)
            .cloned()
            .unwrap_or_default()
    }

    /// Replaces the account's overlay with `f` applied to a copy.
    pub fn update(&self, account: AccountId, f: impl FnOnce(&mut AccountOverlay)) {
        let mut accounts = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = accounts
            .get(&account)
            .map(|a| (**a).clone())
            .unwrap_or_default();
        f(&mut next);
        accounts.insert(account, Arc::new(next));
    }

    pub fn insert(&self, account: AccountId, op: Op) {
        self.update(account, |a| a.ops.insert(op));
    }

    pub fn remove(&self, account: AccountId, id: &str) -> Option<Op> {
        let mut removed = None;
        self.update(account, |a| removed = a.ops.remove(id));
        removed
    }
}
