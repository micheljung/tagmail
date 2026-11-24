//! `POST /api/attachments` on the desktop: the protocol hands over the whole
//! body (responders cannot stream, spike 4), so the `file` part is parsed
//! from memory and written into [`Compose::upload_dir`].

use std::sync::Arc;

use tagmail_api_core::api::Response;
use tagmail_api_core::compose::{
    Compose, MAX_UPLOAD, NewUpload, clean_content_type, clean_file_name, error_response,
};
use tagmail_api_core::dto::ApiErrorDto;

const FIELD: &str = "file";
const NAME_BYTES: usize = 16;

fn invalid(message: &str) -> Response {
    Response::json(
        422,
        &ApiErrorDto {
            error: "invalid".to_owned(),
            message: message.to_owned(),
            field: Some(FIELD.to_owned()),
        },
    )
}

fn too_large() -> Response {
    Response::error(
        413,
        "too_large",
        format!("attachments are at most {MAX_UPLOAD} bytes"),
    )
}

fn temp_name() -> String {
    let mut bytes = [0u8; NAME_BYTES];
    getrandom::fill(&mut bytes).expect("the OS has randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn upload(
    compose: &Arc<dyn Compose>,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Response {
    let Some(boundary) = content_type.and_then(|ct| multer::parse_boundary(ct).ok()) else {
        return Response::error(
            415,
            "unsupported_media_type",
            "send the file as multipart/form-data",
        );
    };
    let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(body) });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let field = loop {
        match multipart.next_field().await {
            Ok(None) => return invalid("a `file` part is required"),
            Ok(Some(field)) if field.name() == Some(FIELD) => break field,
            Ok(Some(_)) => {}
            Err(_) => return Response::error(400, "upload_failed", "the upload failed"),
        }
    };
    let Some(name) = field.file_name().and_then(clean_file_name) else {
        return invalid("the file needs a name");
    };
    let content_type = clean_content_type(field.content_type().map(|m| m.essence_str()));
    let Ok(bytes) = field.bytes().await else {
        return Response::error(400, "upload_failed", "the upload failed");
    };
    if bytes.len() as u64 > MAX_UPLOAD {
        return too_large();
    }
    let dir = compose.upload_dir();
    let path = dir.join(temp_name());
    let written = tokio::fs::create_dir_all(&dir).await;
    if let Err(e) = match written {
        Ok(()) => tokio::fs::write(&path, &bytes).await,
        Err(e) => Err(e),
    } {
        tracing::warn!(error = %e, "storing an upload failed");
        return Response::error(500, "internal", "internal error");
    }
    let upload = NewUpload {
        name,
        content_type,
        file: path,
        size: bytes.len() as u64,
    };
    match compose.add_upload(upload).await {
        Ok(attachment) => Response::json(201, &attachment),
        Err(e) => error_response(e),
    }
}
