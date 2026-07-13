use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotPayloadKind {
    ClientRequest,
    KiroRequest,
    UpstreamResponse,
    ToolDiagnostics,
    StreamTail,
    InternalError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodedPayloadPart {
    pub seq: u32,
    pub kind: SnapshotPayloadKind,
    pub attempt: Option<u32>,
    pub codec: String,
    pub content_type: String,
    pub part_index: u32,
    pub part_count: u32,
    pub original_bytes: u64,
    pub sha256: String,
    pub data: Vec<u8>,
}
