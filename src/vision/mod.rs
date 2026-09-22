//! Vision and OCR utilities for non-vision model fallback in `consolette`.

use std::process::Command;
use tracing::debug;

/// Attempts OCR extraction on raw image bytes using system `tesseract` if available,
/// or falls back to an informative image summary block.
#[must_use]
pub fn extract_text_from_image_bytes(image_bytes: &[u8], media_type: Option<&str>) -> String {
    if image_bytes.is_empty() {
        return "[Image Content: empty image]".to_string();
    }

    if let Ok(extracted) = try_tesseract_ocr(image_bytes) {
        if !extracted.trim().is_empty() {
            return format!("[Image Content (OCR Transcribed)]:\n{}", extracted.trim());
        }
    }

    let mt = media_type.unwrap_or("image/png");
    format!(
        "[Attached Image: type={}, size={} bytes. Non-vision model mode: OCR binary not available, ask user for raw text if required]",
        mt,
        image_bytes.len()
    )
}

/// Helper to decode base64 image data and run OCR text extraction.
#[must_use]
pub fn extract_text_from_base64_image(base64_data: &str, media_type: Option<&str>) -> String {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;
    let clean_b64 = base64_data.trim().replace(['\r', '\n', ' '], "");
    match engine.decode(clean_b64) {
        Ok(bytes) => extract_text_from_image_bytes(&bytes, media_type),
        Err(_) => format!(
            "[Attached Image: unparseable base64 data, type={}]",
            media_type.unwrap_or("image/png")
        ),
    }
}

fn try_tesseract_ocr(bytes: &[u8]) -> Result<String, anyhow::Error> {
    use std::io::Write;
    let temp_dir = std::env::temp_dir();
    let temp_input = temp_dir.join(format!("consolette_ocr_{}.png", uuid::Uuid::new_v4()));

    let mut file = std::fs::File::create(&temp_input)?;
    file.write_all(bytes)?;
    file.flush()?;

    let output = Command::new("tesseract")
        .arg(&temp_input)
        .arg("stdout")
        .output();

    let _ = std::fs::remove_file(&temp_input);

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            Ok(text)
        }
        _ => {
            debug!("tesseract execution not available or failed");
            Err(anyhow::anyhow!(
                "tesseract execution failed or not installed"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn extract_text_from_empty_image_bytes() {
        let res = extract_text_from_image_bytes(&[], Some("image/png"));
        assert!(res.contains("empty image"));
    }

    #[test]
    fn extract_text_from_base64_fallback() {
        let engine = base64::engine::general_purpose::STANDARD;
        let encoded = engine.encode(b"fake image data");
        let res = extract_text_from_base64_image(&encoded, Some("image/jpeg"));
        assert!(res.contains("Attached Image"));
        assert!(res.contains("type=image/jpeg"));
    }

    #[test]
    fn extract_text_from_invalid_base64() {
        let res = extract_text_from_base64_image("!!!not base64!!!", None);
        assert!(res.contains("unparseable base64 data"));
    }
}
