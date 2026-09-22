//! Vision and OCR utilities for non-vision model fallback in `consolette`.

use std::path::Path;
use std::process::Command;
use tracing::debug;

/// Runs OCR against an on-disk image and returns the extracted text.
/// Abstracted so tests can inject a fake without shelling out to a real
/// `tesseract` binary (CI does not install one).
trait OcrRunner: Send {
    fn run(&self, image_path: &Path) -> Result<String, anyhow::Error>;
}

struct TesseractRunner;

impl OcrRunner for TesseractRunner {
    fn run(&self, image_path: &Path) -> Result<String, anyhow::Error> {
        let output = Command::new("tesseract")
            .arg(image_path)
            .arg("stdout")
            .output();

        match output {
            Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
            _ => {
                debug!("tesseract execution not available or failed");
                Err(anyhow::anyhow!(
                    "tesseract execution failed or not installed"
                ))
            }
        }
    }
}

/// Attempts OCR extraction on raw image bytes using system `tesseract` if available,
/// or falls back to an informative image summary block.
///
/// Runs the blocking subprocess/file-I/O work on a `spawn_blocking` thread
/// since this is reached from async request-handling paths (see
/// `providers::openai`/`providers::openrouter`'s `send()`).
#[must_use]
pub async fn extract_text_from_image_bytes(image_bytes: &[u8], media_type: Option<&str>) -> String {
    ocr_or_fallback(image_bytes, media_type, TesseractRunner).await
}

/// Helper to decode base64 image data and run OCR text extraction.
#[must_use]
pub async fn extract_text_from_base64_image(base64_data: &str, media_type: Option<&str>) -> String {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;
    let clean_b64 = base64_data.trim().replace(['\r', '\n', ' '], "");
    match engine.decode(clean_b64) {
        Ok(bytes) => extract_text_from_image_bytes(&bytes, media_type).await,
        Err(_) => format!(
            "[Attached Image: unparseable base64 data, type={}]",
            media_type.unwrap_or("image/png")
        ),
    }
}

/// Shared implementation behind `extract_text_from_image_bytes`, generic
/// over the `OcrRunner` so tests can exercise the OCR-success formatting
/// without a real `tesseract` binary.
async fn ocr_or_fallback<R: OcrRunner + 'static>(
    image_bytes: &[u8],
    media_type: Option<&str>,
    runner: R,
) -> String {
    if image_bytes.is_empty() {
        return "[Image Content: empty image]".to_string();
    }

    let bytes = image_bytes.to_vec();
    let extracted = tokio::task::spawn_blocking(move || try_tesseract_ocr(&bytes, &runner))
        .await
        .ok()
        .and_then(Result::ok);

    if let Some(extracted) = extracted {
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

/// Writes `bytes` to a temp PNG (removed via RAII on drop, including on an
/// early `?` return) and runs `runner` against it.
fn try_tesseract_ocr(bytes: &[u8], runner: &dyn OcrRunner) -> Result<String, anyhow::Error> {
    use std::io::Write;

    let mut tmp = tempfile::Builder::new().suffix(".png").tempfile()?;
    tmp.write_all(bytes)?;
    tmp.flush()?;

    runner.run(tmp.path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    struct FakeRunner {
        canned: &'static str,
    }

    impl OcrRunner for FakeRunner {
        fn run(&self, _image_path: &Path) -> Result<String, anyhow::Error> {
            Ok(self.canned.to_string())
        }
    }

    struct FailingRunner;

    impl OcrRunner for FailingRunner {
        fn run(&self, _image_path: &Path) -> Result<String, anyhow::Error> {
            Err(anyhow::anyhow!("ocr unavailable"))
        }
    }

    #[tokio::test]
    async fn extract_text_from_empty_image_bytes() {
        let res = extract_text_from_image_bytes(&[], Some("image/png")).await;
        assert!(res.contains("empty image"));
    }

    #[tokio::test]
    async fn extract_text_from_base64_fallback() {
        let engine = base64::engine::general_purpose::STANDARD;
        let encoded = engine.encode(b"fake image data");
        let res = extract_text_from_base64_image(&encoded, Some("image/jpeg")).await;
        assert!(res.contains("Attached Image"));
        assert!(res.contains("type=image/jpeg"));
    }

    #[tokio::test]
    async fn extract_text_from_invalid_base64() {
        let res = extract_text_from_base64_image("!!!not base64!!!", None).await;
        assert!(res.contains("unparseable base64 data"));
    }

    #[tokio::test]
    async fn ocr_success_path_formats_transcribed_text() {
        let res = ocr_or_fallback(
            b"not really a png but the runner is faked",
            Some("image/png"),
            FakeRunner {
                canned: "Hello, World!",
            },
        )
        .await;
        assert_eq!(res, "[Image Content (OCR Transcribed)]:\nHello, World!");
    }

    #[tokio::test]
    async fn ocr_failure_falls_back_to_summary() {
        let res = ocr_or_fallback(b"bytes", Some("image/jpeg"), FailingRunner).await;
        assert!(res.contains("Attached Image"));
        assert!(res.contains("type=image/jpeg"));
        assert!(res.contains("OCR binary not available"));
    }
}
