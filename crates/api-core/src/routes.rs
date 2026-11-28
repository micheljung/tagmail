//! The route table of [`crate::Api`]: dispatch and the server's
//! route-protection test both read it.

use crate::api::Method;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteId {
    Version,
    Status,
    ViewIndex,
    SyncView,
    Threads,
    Changes,
    Mailboxes,
    Labels,
    Accounts,
    CreateAccount,
    TestNewAccount,
    TestAccount,
    UpdateAccount,
    DeleteAccount,
    Thread,
    Message,
    RenderMessage,
    Invitation,
    Part,
    ProxyImage,
    ImageSenders,
    SetImageSender,
    Identities,
    CreateIdentity,
    UpdateIdentity,
    DeleteIdentity,
    CreateDraft,
    Draft,
    UpdateDraft,
    DeleteDraft,
    DeleteAttachment,
    Send,
    Outbox,
    CancelOutbox,
    RetryOutbox,
    Reply,
    SubmitOp,
    Ops,
    Op,
    RetryOp,
    DiscardOp,
    CreateLabel,
    UpdateLabel,
    DeleteLabel,
    LabelJobs,
    ExportLabels,
    ImportLabels,
    CaldavSettings,
    UpdateCaldavSettings,
    DiscoverCaldav,
    ConfirmCaldavHost,
    Calendars,
    UpdateCalendar,
    CalendarEvents,
    CalendarRsvp,
    CalendarImport,
    CalendarApply,
    CalendarEvent,
    CalendarEdit,
}

#[derive(Debug, Clone, Copy)]
pub struct Route {
    pub method: Method,
    /// `{name}` matches one percent-decoded path segment.
    pub path: &'static str,
    pub id: RouteId,
}

const fn route(method: Method, path: &'static str, id: RouteId) -> Route {
    Route { method, path, id }
}

pub const ROUTES: &[Route] = &[
    route(Method::Get, "/api/version", RouteId::Version),
    route(Method::Get, "/api/status", RouteId::Status),
    route(Method::Get, "/api/views/{view}/index", RouteId::ViewIndex),
    route(Method::Post, "/api/views/{view}/sync", RouteId::SyncView),
    route(Method::Get, "/api/threads", RouteId::Threads),
    route(Method::Get, "/api/changes", RouteId::Changes),
    route(Method::Get, "/api/mailboxes", RouteId::Mailboxes),
    route(Method::Get, "/api/labels", RouteId::Labels),
    route(Method::Get, "/api/accounts", RouteId::Accounts),
    route(Method::Post, "/api/accounts", RouteId::CreateAccount),
    route(Method::Post, "/api/accounts/test", RouteId::TestNewAccount),
    route(
        Method::Post,
        "/api/accounts/{id}/test",
        RouteId::TestAccount,
    ),
    route(Method::Patch, "/api/accounts/{id}", RouteId::UpdateAccount),
    route(Method::Delete, "/api/accounts/{id}", RouteId::DeleteAccount),
    route(Method::Get, "/api/threads/{id}", RouteId::Thread),
    route(Method::Get, "/api/messages/{id}", RouteId::Message),
    route(
        Method::Get,
        "/api/messages/{id}/render",
        RouteId::RenderMessage,
    ),
    route(
        Method::Get,
        "/api/messages/{id}/invitation",
        RouteId::Invitation,
    ),
    route(Method::Get, "/api/parts/{token}", RouteId::Part),
    route(Method::Get, "/api/proxy/img/{token}", RouteId::ProxyImage),
    route(Method::Get, "/api/image-senders", RouteId::ImageSenders),
    route(Method::Post, "/api/image-senders", RouteId::SetImageSender),
    route(Method::Get, "/api/identities", RouteId::Identities),
    route(Method::Post, "/api/identities", RouteId::CreateIdentity),
    route(
        Method::Patch,
        "/api/identities/{id}",
        RouteId::UpdateIdentity,
    ),
    route(
        Method::Delete,
        "/api/identities/{id}",
        RouteId::DeleteIdentity,
    ),
    route(Method::Post, "/api/drafts", RouteId::CreateDraft),
    route(Method::Get, "/api/drafts/{id}", RouteId::Draft),
    route(Method::Put, "/api/drafts/{id}", RouteId::UpdateDraft),
    route(Method::Delete, "/api/drafts/{id}", RouteId::DeleteDraft),
    route(
        Method::Delete,
        "/api/attachments/{id}",
        RouteId::DeleteAttachment,
    ),
    route(Method::Post, "/api/send", RouteId::Send),
    route(Method::Get, "/api/outbox", RouteId::Outbox),
    route(Method::Delete, "/api/outbox/{id}", RouteId::CancelOutbox),
    route(Method::Post, "/api/outbox/{id}/retry", RouteId::RetryOutbox),
    route(Method::Get, "/api/messages/{id}/reply", RouteId::Reply),
    route(Method::Post, "/api/ops", RouteId::SubmitOp),
    route(Method::Get, "/api/ops", RouteId::Ops),
    route(Method::Get, "/api/ops/{id}", RouteId::Op),
    route(Method::Post, "/api/ops/{id}/retry", RouteId::RetryOp),
    route(Method::Delete, "/api/ops/{id}", RouteId::DiscardOp),
    route(Method::Post, "/api/labels", RouteId::CreateLabel),
    route(Method::Patch, "/api/labels/{key}", RouteId::UpdateLabel),
    route(Method::Delete, "/api/labels/{key}", RouteId::DeleteLabel),
    route(Method::Get, "/api/label-jobs", RouteId::LabelJobs),
    route(Method::Get, "/api/labels/export", RouteId::ExportLabels),
    route(Method::Post, "/api/labels/import", RouteId::ImportLabels),
    route(
        Method::Get,
        "/api/accounts/{id}/caldav",
        RouteId::CaldavSettings,
    ),
    route(
        Method::Put,
        "/api/accounts/{id}/caldav",
        RouteId::UpdateCaldavSettings,
    ),
    route(
        Method::Post,
        "/api/accounts/{id}/caldav/discover",
        RouteId::DiscoverCaldav,
    ),
    route(
        Method::Post,
        "/api/accounts/{id}/caldav/confirm-host",
        RouteId::ConfirmCaldavHost,
    ),
    route(Method::Get, "/api/calendars", RouteId::Calendars),
    route(Method::Patch, "/api/calendars", RouteId::UpdateCalendar),
    route(Method::Get, "/api/calendar/events", RouteId::CalendarEvents),
    route(Method::Post, "/api/calendar/rsvp", RouteId::CalendarRsvp),
    route(
        Method::Post,
        "/api/calendar/import",
        RouteId::CalendarImport,
    ),
    route(Method::Post, "/api/calendar/apply", RouteId::CalendarApply),
    route(
        Method::Get,
        "/api/calendar/events/{accountId}/{href}",
        RouteId::CalendarEvent,
    ),
    route(Method::Post, "/api/calendar/ops", RouteId::CalendarEdit),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miss {
    NotFound,
    MethodNotAllowed,
}

fn params<'a>(pattern: &str, segments: &[&'a str]) -> Option<Vec<&'a str>> {
    let pattern = pattern.strip_prefix("/api/")?;
    let parts: Vec<&str> = pattern.split('/').collect();
    if parts.len() != segments.len() {
        return None;
    }
    let mut out = Vec::new();
    for (part, segment) in parts.iter().zip(segments) {
        if part.starts_with('{') {
            out.push(*segment);
        } else if part != segment {
            return None;
        }
    }
    Some(out)
}

/// The route for decoded `segments` after `/api/`, with its `{…}` values in
/// order.
pub fn resolve<'a>(method: Method, segments: &[&'a str]) -> Result<(RouteId, Vec<&'a str>), Miss> {
    let mut miss = Miss::NotFound;
    for route in ROUTES {
        if let Some(values) = params(route.path, segments) {
            if route.method == method {
                return Ok((route.id, values));
            }
            miss = Miss::MethodNotAllowed;
        }
    }
    Err(miss)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_parameters_and_methods() {
        assert_eq!(
            resolve(Method::Get, &["messages", "7", "render"]),
            Ok((RouteId::RenderMessage, vec!["7"]))
        );
        assert_eq!(
            resolve(Method::Get, &["views", "label:a/b", "index"]),
            Ok((RouteId::ViewIndex, vec!["label:a/b"]))
        );
        assert_eq!(
            resolve(Method::Post, &["views", "inbox", "sync"]),
            Ok((RouteId::SyncView, vec!["inbox"]))
        );
        assert_eq!(
            resolve(Method::Get, &["views", "inbox", "sync"]),
            Err(Miss::MethodNotAllowed)
        );
        assert_eq!(
            resolve(Method::Post, &["image-senders"]),
            Ok((RouteId::SetImageSender, vec![]))
        );
        assert_eq!(
            resolve(Method::Post, &["status"]),
            Err(Miss::MethodNotAllowed)
        );
        assert_eq!(
            resolve(Method::Other, &["threads", "1"]),
            Err(Miss::MethodNotAllowed)
        );
        assert_eq!(
            resolve(Method::Get, &["threads", "1", "x"]),
            Err(Miss::NotFound)
        );
        assert_eq!(resolve(Method::Get, &[""]), Err(Miss::NotFound));
        assert_eq!(
            resolve(Method::Get, &["labels", "export"]),
            Ok((RouteId::ExportLabels, vec![]))
        );
        assert_eq!(
            resolve(Method::Patch, &["labels", "export"]),
            Ok((RouteId::UpdateLabel, vec!["export"]))
        );
        assert_eq!(
            resolve(Method::Post, &["accounts", "test"]),
            Ok((RouteId::TestNewAccount, vec![]))
        );
        assert_eq!(
            resolve(Method::Post, &["accounts", "3", "test"]),
            Ok((RouteId::TestAccount, vec!["3"]))
        );
    }

    #[test]
    fn routes_are_unique() {
        for (i, a) in ROUTES.iter().enumerate() {
            for b in &ROUTES[i + 1..] {
                assert!(
                    a.method != b.method || a.path != b.path,
                    "duplicate route {:?} {}",
                    a.method,
                    a.path
                );
            }
        }
    }
}
