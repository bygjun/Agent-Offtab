use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;

pub const MAX_TOTAL_BYTES: usize = 20 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 4;
const MAX_TEXT_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
pub struct Upload {
    name: String,
    mime: String,
    data: String,
}

pub enum Attachment {
    Image {
        name: String,
        mime: &'static str,
        bytes: Vec<u8>,
    },
    Text {
        name: String,
        content: String,
    },
}

impl Attachment {
    fn size(&self) -> usize {
        match self {
            Self::Image { bytes, .. } => bytes.len(),
            Self::Text { content, .. } => content.len(),
        }
    }
}

pub fn validate_uploads(uploads: impl IntoIterator<Item = Upload>) -> Option<Vec<Attachment>> {
    let mut total_bytes = 0;
    let mut attachments = Vec::new();
    for upload in uploads {
        if attachments.len() == MAX_ATTACHMENTS {
            return None;
        }
        let attachment = upload.validate()?;
        total_bytes += attachment.size();
        if total_bytes > MAX_TOTAL_BYTES {
            return None;
        }
        attachments.push(attachment);
    }
    Some(attachments)
}

impl Upload {
    fn validate(self) -> Option<Attachment> {
        if self.name.trim().is_empty()
            || self.name.chars().count() > 128
            || self.name.contains(['/', '\\'])
            || self.name.chars().any(char::is_control)
        {
            return None;
        }
        let limit = if self.mime == "text/plain" {
            MAX_TEXT_BYTES
        } else {
            MAX_IMAGE_BYTES
        };
        if self.data.len() > limit.div_ceil(3) * 4 {
            return None;
        }
        let bytes = STANDARD.decode(self.data).ok()?;
        if bytes.is_empty() || bytes.len() > limit {
            return None;
        }
        let mime = match self.mime.as_str() {
            "image/png" if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => "image/png",
            "image/jpeg" if bytes.starts_with(b"\xff\xd8\xff") => "image/jpeg",
            "image/webp" if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") => {
                "image/webp"
            }
            "text/plain" => {
                let content = String::from_utf8(bytes).ok()?;
                if content
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
                {
                    return None;
                }
                return Some(Attachment::Text {
                    name: self.name,
                    content,
                });
            }
            _ => return None,
        };
        Some(Attachment::Image {
            name: self.name,
            mime,
            bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_format_path_and_size() {
        let make = |name: &str, mime: &str, bytes: &[u8]| Upload {
            name: name.to_owned(),
            mime: mime.to_owned(),
            data: STANDARD.encode(bytes),
        };
        assert!(matches!(
            make("qa.txt", "text/plain", "한글".as_bytes()).validate(),
            Some(Attachment::Text { .. })
        ));
        assert!(make("../qa.txt", "text/plain", b"sample")
            .validate()
            .is_none());
        assert!(make("qa.png", "image/png", b"not an image")
            .validate()
            .is_none());
        assert!(
            make("qa.txt", "text/plain", &vec![b'a'; MAX_TEXT_BYTES + 1])
                .validate()
                .is_none()
        );
    }

    #[test]
    fn accepts_full_attachment_budget_and_rejects_overages() {
        let photo = |size: usize| {
            let mut bytes = vec![0; size];
            bytes[..3].copy_from_slice(b"\xff\xd8\xff");
            Upload {
                name: "fixture.jpg".into(),
                mime: "image/jpeg".into(),
                data: STANDARD.encode(bytes),
            }
        };
        assert!(validate_uploads((0..MAX_ATTACHMENTS).map(|_| photo(MAX_IMAGE_BYTES))).is_some());
        assert!(validate_uploads([photo(MAX_IMAGE_BYTES + 1)]).is_none());
        assert!(validate_uploads((0..=MAX_ATTACHMENTS).map(|_| photo(3))).is_none());
    }
}
