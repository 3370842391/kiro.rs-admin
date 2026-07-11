//! Anthropic `document` 内容块的本地 PDF 文本提取。

use thiserror::Error;

use pdf_extract::Document;

pub(crate) const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const MAX_PDF_PAGES: usize = 100;
pub(crate) const MAX_PDF_CHARS: usize = 200_000;

#[derive(Debug, Error)]
pub(crate) enum DocumentError {
    #[error("{location}: invalid base64 PDF data")]
    InvalidBase64 { location: String },
    #[error("{location}: PDF exceeds 10 MiB")]
    TooLarge { location: String },
    #[error("{location}: encrypted PDF is not supported")]
    Encrypted { location: String },
    #[error("{location}: PDF has {pages} pages; maximum is 100")]
    TooManyPages { location: String, pages: usize },
    #[error(
        "{location}: PDF contains no extractable text; scanned PDFs require OCR and are not supported"
    )]
    NoText { location: String },
    #[error("{location}: extracted PDF text exceeds 200000 characters")]
    TooMuchText { location: String },
    #[error("{location}: invalid PDF: {message}")]
    InvalidPdf { location: String, message: String },
    #[error("PDF parser task failed: {0}")]
    TaskFailed(String),
}

fn extract_pdf_text(bytes: &[u8], location: &str) -> Result<String, DocumentError> {
    if bytes.len() > MAX_PDF_BYTES {
        return Err(DocumentError::TooLarge {
            location: location.to_string(),
        });
    }

    let document = Document::load_mem(bytes).map_err(|error| DocumentError::InvalidPdf {
        location: location.to_string(),
        message: error.to_string(),
    })?;
    if document.is_encrypted() {
        return Err(DocumentError::Encrypted {
            location: location.to_string(),
        });
    }
    validate_page_count(location, document.get_pages().len())?;

    let page_text =
        pdf_extract::extract_text_from_mem_by_pages(bytes).map_err(|error| {
            DocumentError::InvalidPdf {
                location: location.to_string(),
                message: error.to_string(),
            }
        })?;
    let text = page_text.join("\n\n");
    validate_extracted_text(location, &text)?;
    Ok(text)
}

fn validate_page_count(location: &str, pages: usize) -> Result<(), DocumentError> {
    if pages > MAX_PDF_PAGES {
        return Err(DocumentError::TooManyPages {
            location: location.to_string(),
            pages,
        });
    }
    Ok(())
}

fn validate_extracted_text(location: &str, text: &str) -> Result<(), DocumentError> {
    if text.trim().is_empty() {
        return Err(DocumentError::NoText {
            location: location.to_string(),
        });
    }
    if text.chars().count() > MAX_PDF_CHARS {
        return Err(DocumentError::TooMuchText {
            location: location.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    use super::*;

    const TEXT_PDF_B64: &str = "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2JqCjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCA2MTIgNzkyXSAvUmVzb3VyY2VzIDw8IC9Gb250IDw8IC9GMSA0IDAgUiA+PiA+PiAvQ29udGVudHMgNSAwIFIgPj4KZW5kb2JqCjQgMCBvYmoKPDwgL1R5cGUgL0ZvbnQgL1N1YnR5cGUgL1R5cGUxIC9CYXNlRm9udCAvSGVsdmV0aWNhID4+CmVuZG9iago1IDAgb2JqCjw8IC9MZW5ndGggNTQgPj4Kc3RyZWFtCkJUIC9GMSAxMiBUZiA3MiA3MjAgVGQgKFBERi1DT01QQVRJQklMSVRZLVRPS0VOKSBUaiBFVAplbmRzdHJlYW0KZW5kb2JqCnhyZWYKMCA2CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAwMCBuIAowMDAwMDAwMDU4IDAwMDAwIG4gCjAwMDAwMDAxMTUgMDAwMDAgbiAKMDAwMDAwMDI0MSAwMDAwMCBuIAowMDAwMDAwMzExIDAwMDAwIG4gCnRyYWlsZXIKPDwgL1NpemUgNiAvUm9vdCAxIDAgUiA+PgpzdGFydHhyZWYKNDE1CiUlRU9GCg==";

    #[test]
    fn extracts_text_from_valid_pdf() {
        let bytes = STANDARD.decode(TEXT_PDF_B64).unwrap();
        let text = extract_pdf_text(&bytes, "messages[0].content[0]").unwrap();
        assert!(text.contains("PDF-COMPATIBILITY-TOKEN"));
    }

    #[test]
    fn rejects_oversized_pdf_before_parsing() {
        let bytes = vec![b'x'; MAX_PDF_BYTES + 1];
        assert!(matches!(
            extract_pdf_text(&bytes, "messages[0].content[0]"),
            Err(DocumentError::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_invalid_pdf_without_silently_returning_empty_text() {
        assert!(matches!(
            extract_pdf_text(b"not a pdf", "messages[1].content[2]"),
            Err(DocumentError::InvalidPdf { .. })
        ));
    }

    #[test]
    fn rejects_page_count_above_limit() {
        assert!(matches!(
            validate_page_count("messages[0].content[0]", MAX_PDF_PAGES + 1),
            Err(DocumentError::TooManyPages { .. })
        ));
    }

    #[test]
    fn rejects_extracted_text_above_character_limit() {
        let text = "x".repeat(MAX_PDF_CHARS + 1);
        assert!(matches!(
            validate_extracted_text("messages[0].content[0]", &text),
            Err(DocumentError::TooMuchText { .. })
        ));
    }
}
