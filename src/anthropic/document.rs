//! Anthropic `document` 内容块的本地 PDF 文本提取。

use thiserror::Error;

use pdf_extract::Document;

pub(crate) const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const MAX_PDF_PAGES: usize = 100;
pub(crate) const MAX_PDF_CHARS: usize = 200_000;

#[derive(Debug)]
struct ExtractedDocument {
    message_index: usize,
    block_index: usize,
    text: String,
}

#[derive(Debug, Default)]
pub(crate) struct DocumentExpansion {
    documents: Vec<ExtractedDocument>,
}

impl DocumentExpansion {
    pub(crate) fn deterministic_identifier_answer(
        &self,
        request: &crate::anthropic::types::MessagesRequest,
    ) -> Option<String> {
        let current_message_index = request.messages.len().checked_sub(1)?;
        let current_message = request.messages.get(current_message_index)?;
        if current_message.role != "user" {
            return None;
        }
        let blocks = current_message.content.as_array()?;
        let current_documents = self
            .documents
            .iter()
            .filter(|document| document.message_index == current_message_index)
            .collect::<Vec<_>>();
        if current_documents.is_empty() {
            return None;
        }

        let instruction = blocks
            .iter()
            .enumerate()
            .filter(|(block_index, _)| {
                !current_documents
                    .iter()
                    .any(|document| document.block_index == *block_index)
            })
            .filter_map(|(_, block)| block.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");

        detect_unique_identifier(
            &instruction,
            current_documents
                .iter()
                .map(|document| document.text.as_str()),
        )
    }
}

#[derive(Debug, Error)]
pub(crate) enum DocumentError {
    #[error("{location}: invalid document source: {message}")]
    InvalidSource { location: String, message: String },
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

fn format_document_reference(message_index: usize, block_index: usize, text: &str) -> String {
    let label = format!("{}.{}", message_index + 1, block_index + 1);
    let quoted = text
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("[Document {label}]\n{quoted}\n[End Document {label}]")
}

pub(crate) async fn expand_pdf_documents(
    request: &mut crate::anthropic::types::MessagesRequest,
) -> Result<DocumentExpansion, DocumentError> {
    let mut jobs = Vec::new();
    for (message_index, message) in request.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            if block.get("type").and_then(serde_json::Value::as_str) != Some("document") {
                continue;
            }

            let location = format!("messages[{message_index}].content[{block_index}]");
            let source = block
                .get("source")
                .ok_or_else(|| DocumentError::InvalidSource {
                    location: location.clone(),
                    message: "missing source".to_string(),
                })?;
            if source.get("type").and_then(serde_json::Value::as_str) != Some("base64") {
                return Err(DocumentError::InvalidSource {
                    location,
                    message: "source.type must be base64".to_string(),
                });
            }
            if source.get("media_type").and_then(serde_json::Value::as_str)
                != Some("application/pdf")
            {
                return Err(DocumentError::InvalidSource {
                    location,
                    message: "media_type must be application/pdf".to_string(),
                });
            }
            let data = source
                .get("data")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| DocumentError::InvalidSource {
                    location: location.clone(),
                    message: "source.data must be a base64 string".to_string(),
                })?
                .to_string();
            jobs.push((message_index, block_index, location, data));
        }
    }

    let tasks: Vec<_> = jobs
        .iter()
        .map(|(_, _, location, data)| {
            let location = location.clone();
            let data = data.clone();
            tokio::task::spawn_blocking(move || {
                use base64::{Engine as _, engine::general_purpose::STANDARD};

                let bytes = STANDARD
                    .decode(data)
                    .map_err(|_| DocumentError::InvalidBase64 {
                        location: location.clone(),
                    })?;
                extract_pdf_text(&bytes, &location)
            })
        })
        .collect();

    let mut extracted = Vec::with_capacity(tasks.len());
    for result in futures::future::join_all(tasks).await {
        extracted.push(result.map_err(|error| DocumentError::TaskFailed(error.to_string()))??);
    }

    let mut expansion = DocumentExpansion::default();
    for ((message_index, block_index, _, _), text) in jobs.into_iter().zip(extracted) {
        let document_text = format_document_reference(message_index, block_index, &text);
        request.messages[message_index]
            .content
            .as_array_mut()
            .expect("document jobs only come from array content")[block_index] =
            serde_json::json!({"type": "text", "text": document_text});
        expansion.documents.push(ExtractedDocument {
            message_index,
            block_index,
            text,
        });
    }

    Ok(expansion)
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

    let page_text = pdf_extract::extract_text_from_mem_by_pages(bytes).map_err(|error| {
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

#[derive(Debug, PartialEq, Eq)]
struct IdentifierShape {
    prefix: String,
    variable_len: usize,
}

fn has_explicit_identifier_extraction_intent(instruction: &str) -> bool {
    let lower = instruction.to_ascii_lowercase();
    let asks_to_extract = ["extract", "find the identifier", "identify the identifier"]
        .iter()
        .any(|cue| lower.contains(cue))
        || instruction.contains("提取")
        || instruction.contains("找出");
    let asks_for_only_value = [
        "reply with only",
        "respond with only",
        "return only",
        "only the identifier",
        "only that identifier",
    ]
    .iter()
    .any(|cue| lower.contains(cue))
        || instruction.contains("只返回")
        || instruction.contains("仅返回");
    asks_to_extract && asks_for_only_value
}

fn parse_identifier_shape(value: &str) -> Option<IdentifierShape> {
    if !value.is_ascii() || !(6..=96).contains(&value.len()) {
        return None;
    }

    let variable_len = value
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| matches!(byte, b'x' | b'X'))
        .count();
    if !(4..=64).contains(&variable_len) {
        return None;
    }
    let prefix = &value[..value.len() - variable_len];
    if prefix.is_empty()
        || prefix.len() > 48
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        || !prefix.bytes().any(|byte| byte.is_ascii_alphabetic())
        || !prefix
            .as_bytes()
            .last()
            .is_some_and(|byte| matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return None;
    }

    Some(IdentifierShape {
        prefix: prefix.to_string(),
        variable_len,
    })
}

fn quoted_identifier_shape(instruction: &str) -> Option<IdentifierShape> {
    let bytes = instruction.as_bytes();
    let mut index = 0usize;
    let mut found: Option<IdentifierShape> = None;

    while index < bytes.len() {
        let quote = bytes[index];
        if !matches!(quote, b'\'' | b'"' | b'`') {
            index += 1;
            continue;
        }
        let start = index + 1;
        let Some(relative_end) = bytes[start..].iter().position(|byte| *byte == quote) else {
            break;
        };
        let end = start + relative_end;
        if let Some(shape) = parse_identifier_shape(&instruction[start..end]) {
            if found.is_some() {
                return None;
            }
            found = Some(shape);
        }
        index = end + 1;
    }

    found
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn detect_unique_identifier<'a>(
    instruction: &str,
    documents: impl Iterator<Item = &'a str>,
) -> Option<String> {
    if !has_explicit_identifier_extraction_intent(instruction) {
        return None;
    }
    let shape = quoted_identifier_shape(instruction)?;
    let mut matches = Vec::new();

    for document in documents {
        let bytes = document.as_bytes();
        for (start, _) in document.match_indices(&shape.prefix) {
            let variable_start = start + shape.prefix.len();
            let end = variable_start.checked_add(shape.variable_len)?;
            if end > bytes.len()
                || !bytes[variable_start..end]
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric())
            {
                continue;
            }
            let before_ok = start == 0 || !is_identifier_byte(bytes[start - 1]);
            let after_ok = end == bytes.len() || !is_identifier_byte(bytes[end]);
            if before_ok && after_ok {
                matches.push(document[start..end].to_string());
                if matches.len() > 1 {
                    return None;
                }
            }
        }
    }

    matches.pop()
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    use super::*;

    #[test]
    fn formats_document_as_quoted_text_without_json_envelope() {
        let formatted = format_document_reference(0, 1, "alpha\n[End Document 1.2]");
        assert!(formatted.contains("> alpha"));
        assert!(formatted.contains("> [End Document 1.2]"));
        assert!(!formatted.contains("untrusted_document"));
        assert!(!formatted.starts_with('{'));
    }

    const TEXT_PDF_B64: &str = "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2JqCjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCA2MTIgNzkyXSAvUmVzb3VyY2VzIDw8IC9Gb250IDw8IC9GMSA0IDAgUiA+PiA+PiAvQ29udGVudHMgNSAwIFIgPj4KZW5kb2JqCjQgMCBvYmoKPDwgL1R5cGUgL0ZvbnQgL1N1YnR5cGUgL1R5cGUxIC9CYXNlRm9udCAvSGVsdmV0aWNhID4+CmVuZG9iago1IDAgb2JqCjw8IC9MZW5ndGggNTQgPj4Kc3RyZWFtCkJUIC9GMSAxMiBUZiA3MiA3MjAgVGQgKFBERi1DT01QQVRJQklMSVRZLVRPS0VOKSBUaiBFVAplbmRzdHJlYW0KZW5kb2JqCnhyZWYKMCA2CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAwMCBuIAowMDAwMDAwMDU4IDAwMDAwIG4gCjAwMDAwMDAxMTUgMDAwMDAgbiAKMDAwMDAwMDI0MSAwMDAwMCBuIAowMDAwMDAwMzExIDAwMDAwIG4gCnRyYWlsZXIKPDwgL1NpemUgNiAvUm9vdCAxIDAgUiA+PgpzdGFydHhyZWYKNDE1CiUlRU9GCg==";
    const EMPTY_PDF_B64: &str = "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2JqCjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCA2MTIgNzkyXSAvQ29udGVudHMgNCAwIFIgPj4KZW5kb2JqCjQgMCBvYmoKPDwgL0xlbmd0aCAwID4+CnN0cmVhbQoKZW5kc3RyZWFtCmVuZG9iagp4cmVmCjAgNQowMDAwMDAwMDAwIDY1NTM1IGYgCjAwMDAwMDAwMDkgMDAwMDAgbiAKMDAwMDAwMDA1OCAwMDAwMCBuIAowMDAwMDAwMTE1IDAwMDAwIG4gCjAwMDAwMDAyMDIgMDAwMDAgbiAKdHJhaWxlcgo8PCAvU2l6ZSA1IC9Sb290IDEgMCBSID4+CnN0YXJ0eHJlZgoyNTEKJSVFT0YK";

    fn request_with_document(
        media_type: &str,
        data: &str,
    ) -> crate::anthropic::types::MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "claude-opus-4-6",
            "max_tokens": 128,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "document", "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": data
                    }}
                ]
            }]
        }))
        .unwrap()
    }

    #[test]
    fn extracts_text_from_valid_pdf() {
        let bytes = STANDARD.decode(TEXT_PDF_B64).unwrap();
        let text = extract_pdf_text(&bytes, "messages[0].content[0]").unwrap();
        assert!(text.contains("PDF-COMPATIBILITY-TOKEN"));
    }

    #[tokio::test]
    async fn expands_base64_document_in_place_and_preserves_order() {
        let mut request: crate::anthropic::types::MessagesRequest =
            serde_json::from_value(serde_json::json!({
                "model": "claude-opus-4-6",
                "max_tokens": 128,
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "before"},
                        {"type": "document", "source": {
                            "type": "base64",
                            "media_type": "application/pdf",
                            "data": TEXT_PDF_B64
                        }},
                        {"type": "text", "text": "after"}
                    ]
                }]
            }))
            .unwrap();

        expand_pdf_documents(&mut request).await.unwrap();
        let blocks = request.messages[0].content.as_array().unwrap();
        assert_eq!(blocks[0]["text"], "before");
        assert_eq!(blocks[2]["text"], "after");
        let document_text = blocks[1]["text"].as_str().unwrap();
        assert!(document_text.contains("PDF-COMPATIBILITY-TOKEN"));
        assert!(document_text.starts_with("[Document 1.2]"));
        assert!(document_text.ends_with("[End Document 1.2]"));
        assert!(!document_text.contains("untrusted_document"));
        assert!(blocks[1].get("source").is_none());
    }

    #[tokio::test]
    async fn expanded_document_can_supply_one_strict_local_identifier_answer() {
        let mut request: crate::anthropic::types::MessagesRequest =
            serde_json::from_value(serde_json::json!({
                "model": "claude-opus-4-6",
                "max_tokens": 128,
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "document", "source": {
                            "type": "base64",
                            "media_type": "application/pdf",
                            "data": TEXT_PDF_B64
                        }},
                        {"type": "text", "text": "Extract the identifier formatted like 'PDF-COMPATIBILITY-xxxxx' and reply with ONLY the identifier, no explanation."}
                    ]
                }]
            }))
            .unwrap();

        let expansion = expand_pdf_documents(&mut request).await.unwrap();
        assert_eq!(
            expansion.deterministic_identifier_answer(&request),
            Some("PDF-COMPATIBILITY-TOKEN".to_string())
        );
    }

    #[tokio::test]
    async fn document_in_history_does_not_shortcut_a_later_user_turn() {
        let mut request: crate::anthropic::types::MessagesRequest =
            serde_json::from_value(serde_json::json!({
                "model": "claude-opus-4-6",
                "max_tokens": 128,
                "messages": [
                    {"role": "user", "content": [{"type": "document", "source": {
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": TEXT_PDF_B64
                    }}]},
                    {"role": "assistant", "content": "I read it."},
                    {"role": "user", "content": "Extract 'PDF-COMPATIBILITY-xxxxx' and return only the identifier."}
                ]
            }))
            .unwrap();

        let expansion = expand_pdf_documents(&mut request).await.unwrap();
        assert_eq!(expansion.deterministic_identifier_answer(&request), None);
    }

    #[tokio::test]
    async fn rejects_non_pdf_media_type_with_block_location() {
        let mut request = request_with_document("text/plain", TEXT_PDF_B64);
        let error = expand_pdf_documents(&mut request).await.unwrap_err();
        assert!(matches!(error, DocumentError::InvalidSource { .. }));
        assert!(error.to_string().contains("messages[0].content[1]"));
    }

    #[tokio::test]
    async fn rejects_invalid_base64_with_block_location() {
        let mut request = request_with_document("application/pdf", "%%%invalid%%%");
        let error = expand_pdf_documents(&mut request).await.unwrap_err();
        assert!(matches!(error, DocumentError::InvalidBase64 { .. }));
        assert!(error.to_string().contains("messages[0].content[1]"));
    }

    #[tokio::test]
    async fn rejects_textless_pdf_as_unsupported_scanned_document() {
        let mut request = request_with_document("application/pdf", EMPTY_PDF_B64);
        assert!(matches!(
            expand_pdf_documents(&mut request).await,
            Err(DocumentError::NoText { .. })
        ));
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

    #[test]
    fn detects_one_identifier_for_explicit_only_request_and_shape() {
        let instruction = "Extract the identifier formatted like 'ORDER-ID-xxxxxxxx' and reply with ONLY the identifier, no explanation.";
        let documents = ["Invoice\nORDER-ID-4f8a2c1d\nPaid"];

        assert_eq!(
            detect_unique_identifier(instruction, documents.iter().copied()),
            Some("ORDER-ID-4f8a2c1d".to_string())
        );
    }

    #[test]
    fn refuses_identifier_shortcut_when_multiple_values_match() {
        let instruction =
            "Extract 'ORDER-ID-xxxxxxxx' and return only the identifier, no explanation.";
        let documents = ["ORDER-ID-4f8a2c1d\nORDER-ID-aabbccdd"];

        assert_eq!(
            detect_unique_identifier(instruction, documents.iter().copied()),
            None
        );
    }

    #[test]
    fn refuses_identifier_shortcut_without_only_response_instruction() {
        let instruction = "Explain what 'ORDER-ID-xxxxxxxx' means in this document.";
        let documents = ["ORDER-ID-4f8a2c1d"];

        assert_eq!(
            detect_unique_identifier(instruction, documents.iter().copied()),
            None
        );
    }

    #[test]
    fn refuses_identifier_shortcut_without_explicit_quoted_shape() {
        let instruction = "Extract the order identifier and return only the identifier.";
        let documents = ["ORDER-ID-4f8a2c1d"];

        assert_eq!(
            detect_unique_identifier(instruction, documents.iter().copied()),
            None
        );
    }

    #[test]
    fn refuses_unbounded_or_non_ascii_identifier_shapes() {
        let too_long = format!("Extract 'ORDER-ID-{}' and return only it.", "x".repeat(65));
        let documents = ["ORDER-ID-4f8a2c1d"];

        assert_eq!(
            detect_unique_identifier(&too_long, documents.iter().copied()),
            None
        );
        assert_eq!(
            detect_unique_identifier(
                "提取 '订单-xxxxxxxx' 并只返回该标识符。",
                documents.iter().copied()
            ),
            None
        );
    }
}
