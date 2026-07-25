//! Kiro stream metadata events.

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// Terminal metadata emitted by Kiro alongside usage and metering frames.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    /// Upstream terminal reason, for example `CONTENT_FILTERED`.
    #[serde(default, alias = "stop_reason")]
    pub stop_reason: String,
}

impl EventPayload for MetadataEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_snake_case_stop_reason_alias() {
        let event: MetadataEvent =
            serde_json::from_str(r#"{"stop_reason":"CONTENT_FILTERED"}"#).unwrap();
        assert_eq!(event.stop_reason, "CONTENT_FILTERED");
    }
}
