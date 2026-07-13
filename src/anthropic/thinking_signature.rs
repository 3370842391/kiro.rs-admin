use sha2::{Digest, Sha256};

/// Issue an opaque, request-scoped thinking replay token.
///
/// This is deliberately not presented as an Anthropic cryptographic
/// signature. Native upstream signatures are still passed through unchanged;
/// this token only prevents clients from receiving a fixed placeholder when
/// Kiro did not provide one.
pub(crate) fn issue_signature(request_id: &str, thinking: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(request_id.as_bytes());
    digest.update([0]);
    digest.update(thinking.as_bytes());
    let digest = digest.finalize();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    format!("krs1_{}_{}", nonce, hex::encode(&digest[..8]))
}

#[cfg(test)]
mod tests {
    use super::issue_signature;

    #[test]
    fn generated_signatures_are_request_scoped_and_not_a_fixed_placeholder() {
        let first = issue_signature("request-a", "thinking-a");
        let second = issue_signature("request-b", "thinking-b");
        assert_ne!(first, second);
        assert_ne!(first, "kiro-rs-thinking-signature");
        assert!(first.starts_with("krs1_"));
    }
}
