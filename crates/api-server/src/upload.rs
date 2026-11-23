//! `POST /api/attachments`: `multipart/form-data` with one field `file`.
//! The part is streamed into the engine's upload directory and refused
//! with `413` as soon as it passes [`MAX_UPLOAD`], never buffered whole;
//! then [`tagmail_api_core::compose::Compose`] takes the file over.

use std::path::Path;

use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::response::Response as HttpResponse;
use multer::{Constraints, Multipart, SizeLimit};
use tagmail_api_core::Response;
use tagmail_api_core::compose::{
    MAX_UPLOAD, NewUpload, clean_content_type, clean_file_name, error_response,
};
use tagmail_api_core::dto::ApiErrorDto;
use tokio::io::AsyncWriteExt;

use crate::http::{AppState, to_http};
use crate::session;

/// Multipart framing (boundaries, part headers) allowed beyond the file.
const FRAMING: u64 = 64 * 1024;
const FIELD: &str = "file";

fn too_large() -> HttpResponse {
    to_http(Response::error(
        413,
        "too_large",
        format!("attachments are at most {MAX_UPLOAD} bytes"),
    ))
}

fn invalid(message: &str) -> HttpResponse {
    to_http(Response::json(
        422,
        &ApiErrorDto {
            error: "invalid".to_owned(),
            message: message.to_owned(),
            field: Some(FIELD.to_owned()),
        },
    ))
}

enum Failure {
    TooLarge,
    Invalid(&'static str),
    Io(std::io::Error),
}

impl From<multer::Error> for Failure {
    fn from(e: multer::Error) -> Failure {
        match e {
            multer::Error::FieldSizeExceeded { .. } | multer::Error::StreamSizeExceeded { .. } => {
                Failure::TooLarge
            }
            other => Failure::Io(std::io::Error::other(other)),
        }
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Failure {
        Failure::Io(e)
    }
}

/// Streams the `file` part into `path`; returns its name, type and size.
async fn receive(
    multipart: &mut Multipart<'static>,
    path: &Path,
) -> Result<(String, String, u64), Failure> {
    let mut field = loop {
        match multipart.next_field().await? {
            None => return Err(Failure::Invalid("a `file` part is required")),
            Some(field) if field.name() == Some(FIELD) => break field,
            Some(_) => {}
        }
    };
    let name = field
        .file_name()
        .and_then(clean_file_name)
        .ok_or(Failure::Invalid("the file needs a name"))?;
    let content_type = clean_content_type(field.content_type().map(|m| m.essence_str()));
    let mut file = tokio::fs::File::create(path).await?;
    let mut size = 0u64;
    while let Some(chunk) = field.chunk().await? {
        size += chunk.len() as u64;
        if size > MAX_UPLOAD {
            return Err(Failure::TooLarge);
        }
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    Ok((name, content_type, size))
}

pub async fn upload(State(state): State<AppState>, headers: HeaderMap, body: Body) -> HttpResponse {
    let Some(compose) = state.api.compose().cloned() else {
        return to_http(Response::error(
            503,
            "compose_unavailable",
            "sending is not available here",
        ));
    };
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_UPLOAD + FRAMING) {
        return too_large();
    }
    let boundary = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| multer::parse_boundary(ct).ok());
    let Some(boundary) = boundary else {
        return to_http(Response::error(
            415,
            "unsupported_media_type",
            "send the file as multipart/form-data",
        ));
    };
    let constraints = Constraints::new().size_limit(
        SizeLimit::new()
            .whole_stream(MAX_UPLOAD + FRAMING)
            .for_field(FIELD, MAX_UPLOAD),
    );
    let mut multipart = Multipart::with_constraints(body.into_data_stream(), boundary, constraints);
    let path = compose
        .upload_dir()
        .join(hex::encode(session::random_bytes::<16>()));
    let (name, content_type, size) = match receive(&mut multipart, &path).await {
        Ok(received) => received,
        Err(failure) => {
            let _ = tokio::fs::remove_file(&path).await;
            return match failure {
                Failure::TooLarge => too_large(),
                Failure::Invalid(message) => invalid(message),
                Failure::Io(e) => {
                    tracing::warn!(error = %e, "upload failed");
                    to_http(Response::error(400, "upload_failed", "the upload failed"))
                }
            };
        }
    };
    let upload = NewUpload {
        name,
        content_type,
        file: path,
        size,
    };
    to_http(match compose.add_upload(upload).await {
        Ok(attachment) => Response::json(201, &attachment),
        Err(e) => error_response(e),
    })
}
