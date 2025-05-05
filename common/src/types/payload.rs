use serde::{Deserialize, Serialize};

/// エージェントとクライアント間でやり取りするメッセージのペイロード定義
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Payload {
    #[serde(rename = "init-request")]
    InitRequest {
        agent_id: String,
        ip: String,
        remote_host: String,
        country_code: String,
        city: String,
        region: String,
        asn: String,
        asn_org: String,
        // Add new system info fields
        os_type: String,
        os_version: String,
        hostname: String,
        kernel_version: String,
        username: String,
    },
    #[serde(rename = "init-response")]
    InitResponse {
        success: bool,
        message: Option<String>,
    },
    #[serde(rename = "init-error")]
    InitError { error_message: String },
    #[serde(rename = "connect-request")]
    ConnectRequest {
        request_id: String,
        target_addr: String,
        target_port: u16,
        agent_id: Option<String>,
        address_type: u8,
    },
    #[serde(rename = "connect-response")]
    ConnectResponse { request_id: String, success: bool },
    #[serde(rename = "data-chunk-request")]
    DataRequestChunk {
        request_id: String,
        chunk_id: u32,
        data: String,
    },
    #[serde(rename = "data-chunk-response")]
    DataResponseChunk {
        request_id: String,
        chunk_id: u32,
        data: String,
    },
    #[serde(rename = "data-response-transfer-complete")]
    DataResponseTransferComplete {
        request_id: String,
        success: bool,
        error_message: Option<String>,
    },
    #[serde(rename = "data-request-transfer-complete")]
    DataRequestTransferComplete {
        request_id: String,
        success: bool,
        error_message: Option<String>,
    },
    #[serde(rename = "client-disconnect")]
    ClientDisconnect { request_id: String },

    // Renamed from Command, added request_id
    #[serde(rename = "command-request")]
    CommandRequest { request_id: String, command: String },
    // Added new variant for command response chunks
    #[serde(rename = "command-response-chunk")]
    CommandResponseChunk {
        request_id: String,
        chunk_id: u32,
        /// 1: stdout, 2: stderr
        stream_type: u8,
        /// Base64 encoded data
        data: String,
    },
    // Added new variant for command response completion
    #[serde(rename = "command-response-transfer-complete")]
    CommandResponseTransferComplete {
        request_id: String,
        success: bool,
        exit_code: Option<i32>,
        error_message: Option<String>,
    },
}
