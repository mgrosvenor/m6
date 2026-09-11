/// Multipart file upload support (feature = "multipart").
///
/// Uses the `multer` crate to parse `multipart/form-data` bodies.

/// A single uploaded file.
#[derive(Debug, Clone)]
pub struct Upload {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Parse a single named file field from a multipart body.
///
/// `content_type_header` should be the full `Content-Type` header value including the boundary,
/// e.g. `"multipart/form-data; boundary=----Boundary"`.
pub fn parse_upload(
    body: &[u8],
    content_type_header: &str,
    field_name: &str,
) -> crate::error::Result<Upload> {
    // Extract boundary from Content-Type header.
    let boundary = multer::parse_boundary(content_type_header)
        .map_err(|e| crate::error::Error::BadRequest(format!("multipart boundary: {e}")))?;

    // multer is async-first; use a single-threaded tokio runtime to drive it.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| crate::error::Error::Other(anyhow::anyhow!("tokio rt: {e}")))?;

    rt.block_on(async {
        let body_bytes = bytes::Bytes::copy_from_slice(body);
        let stream = futures_util::stream::once(async move {
            Ok::<_, std::convert::Infallible>(body_bytes)
        });
        let mut multipart = multer::Multipart::new(stream, boundary);

        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| crate::error::Error::BadRequest(format!("multipart parse: {e}")))?
        {
            let name = field.name().unwrap_or("").to_string();
            if name != field_name {
                continue;
            }
            let filename = field
                .file_name()
                .unwrap_or("upload")
                .to_string();
            let content_type = field
                .content_type()
                .map(|m| m.to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let data = field
                .bytes()
                .await
                .map_err(|e| crate::error::Error::BadRequest(format!("reading field: {e}")))?
                .to_vec();

            return Ok(Upload { filename, content_type, data });
        }

        Err(crate::error::Error::BadRequest(format!(
            "multipart field `{field_name}` not found"
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal multipart/form-data body for testing.
    fn make_multipart(boundary: &str, field_name: &str, filename: &str, data: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{filename}\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    #[test]
    fn test_parse_upload_basic() {
        let boundary = "TESTBOUNDARY";
        let data = b"fake-png-data";
        let body = make_multipart(boundary, "avatar", "photo.png", data);
        let ct = format!("multipart/form-data; boundary={boundary}");
        let upload = parse_upload(&body, &ct, "avatar").unwrap();
        assert_eq!(upload.filename, "photo.png");
        assert_eq!(upload.data, data);
        assert!(upload.content_type.contains("image/png"));
    }

    #[test]
    fn test_parse_upload_missing_field() {
        let boundary = "TESTBOUNDARY";
        let data = b"data";
        let body = make_multipart(boundary, "avatar", "photo.png", data);
        let ct = format!("multipart/form-data; boundary={boundary}");
        let err = parse_upload(&body, &ct, "nonexistent");
        assert!(err.is_err());
    }
}
