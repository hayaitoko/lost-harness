//! Wave 5.1 / M5 Slice 0 — the **multimodal wire format** (PLAN §12 item 1): the
//! prerequisite that lets a message carry an image (a screenshot, later) to a
//! vision-capable model, and degrade cleanly to text on an endpoint that can't
//! see. This is the NON-native, fully-testable primitive; the screenshot SOURCE
//! (the `capture_screen` tool + a platform backend) is the on-target Slice 1.
//!
//! Two invariants, both load-bearing for privacy + honesty:
//! 1. **An image is never handed to an endpoint that can't use it.** On a
//!    text-only seat the bytes are dropped and replaced with a bracketed
//!    placeholder, so the model is TOLD an image existed rather than being fed
//!    unreadable data (and no screenshot silently rides a text request).
//! 2. **An ordinary text turn is byte-for-byte unchanged.** With no images the
//!    assembled `content` is the bare text string — no accidental array-ifying of
//!    the millions of plain messages, so this can't perturb the existing flow.
//!
//! WIRING NOTE (for the on-target Slice 1+ that makes this live): [`assemble_content`]
//! returns a `serde_json::Value` that is EITHER a string OR an array, but today's
//! `ChatMessage.content` (`client.rs`) is a plain `String`. The wiring slice must
//! bridge that deliberately — carry the `Value` through to serialization (so the
//! array reaches the endpoint intact), NOT `.to_string()` it (that would stringify
//! the JSON array into a text field and the endpoint would see literal `[{...}]`,
//! not an image).

use serde_json::{json, Value};

/// One image attached to a message, ready for the multimodal wire format. Holds
/// base64 bytes (NO `data:` prefix) + the MIME type; the `data:` URI is derived
/// only at serialization time.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageBlock {
    /// MIME type, e.g. `"image/png"`.
    pub media_type: String,
    /// Base64-encoded image bytes (no `data:` prefix).
    pub data_b64: String,
}

impl ImageBlock {
    /// A PNG image block (the format screen capture emits).
    pub fn png(data_b64: impl Into<String>) -> Self {
        Self { media_type: "image/png".to_string(), data_b64: data_b64.into() }
    }

    /// The `data:` URI form used by OpenAI-style `image_url` content parts.
    fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data_b64)
    }
}

/// Substituted for each image when the endpoint can't see them — the model still
/// learns an image was present, honestly, instead of getting bytes it can't read.
pub const TEXT_ONLY_IMAGE_PLACEHOLDER: &str = "[screenshot omitted — endpoint is text-only]";

/// Assemble a message's `content` field for the chat wire format.
///
/// - `multimodal == true` (a vision-capable seat): the OpenAI content-ARRAY form
///   — a `text` part (only if the text is non-empty) followed by one `image_url`
///   part per image (as a `data:` URI). An image is never dropped on a capable
///   endpoint.
/// - `multimodal == false` (a text-only seat): a plain STRING — the text with one
///   [`TEXT_ONLY_IMAGE_PLACEHOLDER`] appended per image. Degrades cleanly; the
///   image bytes never leave the machine toward an endpoint that can't use them.
///
/// With no images, BOTH paths return the bare text string, so a plain text turn
/// is unchanged.
pub fn assemble_content(text: &str, images: &[ImageBlock], multimodal: bool) -> Value {
    if images.is_empty() {
        return Value::String(text.to_string());
    }
    if multimodal {
        let mut parts: Vec<Value> = Vec::with_capacity(images.len() + 1);
        if !text.is_empty() {
            parts.push(json!({ "type": "text", "text": text }));
        }
        for img in images {
            parts.push(json!({
                "type": "image_url",
                "image_url": { "url": img.data_uri() }
            }));
        }
        Value::Array(parts)
    } else {
        let mut s = text.to_string();
        for _ in images {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(TEXT_ONLY_IMAGE_PLACEHOLDER);
        }
        Value::String(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_images_is_a_bare_text_string_on_either_endpoint() {
        // An ordinary text turn must be untouched — no array-ification.
        assert_eq!(assemble_content("hello", &[], true), Value::String("hello".into()));
        assert_eq!(assemble_content("hello", &[], false), Value::String("hello".into()));
    }

    #[test]
    fn multimodal_endpoint_gets_a_content_array_with_the_image() {
        let img = ImageBlock::png("QUJD"); // "ABC" b64
        let out = assemble_content("look at this", std::slice::from_ref(&img), true);
        let arr = out.as_array().expect("multimodal → array");
        assert_eq!(arr.len(), 2, "one text part + one image part");
        assert_eq!(arr[0], json!({ "type": "text", "text": "look at this" }));
        assert_eq!(
            arr[1],
            json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } }),
            "the image rides as a data: URI on a capable endpoint"
        );
    }

    #[test]
    fn multimodal_with_empty_text_omits_the_text_part() {
        let img = ImageBlock::png("QUJD");
        let out = assemble_content("", std::slice::from_ref(&img), true);
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 1, "no empty text part — just the image");
        assert_eq!(arr[0]["type"], "image_url");
    }

    #[test]
    fn text_only_endpoint_degrades_to_a_placeholder_never_bytes() {
        let img = ImageBlock::png("QUJD");
        let out = assemble_content("what is this", std::slice::from_ref(&img), false);
        let s = out.as_str().expect("text-only → string");
        assert!(s.starts_with("what is this"), "the real text is preserved");
        assert!(s.contains(TEXT_ONLY_IMAGE_PLACEHOLDER), "the model is told an image existed");
        assert!(!s.contains("QUJD"), "the image bytes NEVER reach a text-only endpoint");
    }

    #[test]
    fn text_only_appends_one_placeholder_per_image() {
        let imgs = [ImageBlock::png("QQ"), ImageBlock::png("Qg")];
        let out = assemble_content("two shots", &imgs, false);
        let s = out.as_str().unwrap();
        assert_eq!(s.matches(TEXT_ONLY_IMAGE_PLACEHOLDER).count(), 2);
    }
}
