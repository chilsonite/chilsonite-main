use crate::ws::send_message;
use crate::WsSink;
use anyhow::Result;
use common::Payload;
use log::{error, info};
use std::sync::Arc;
use sysinfo::System;
use tokio::sync::Mutex;
use ureq::{self, Agent};
use whoami;

// ifconfig.co から取得する地理情報・IP情報の構造体
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)] // 未使用フィールドの警告抑制
pub(crate) struct GeoData {
    ip: String,
    country: String,
    country_iso: String, // 国コード (ISO)
    region_name: String, // 地域名
    city: String,        // 都市名
    latitude: f64,
    longitude: f64,
    asn: String,     // ASN番号
    asn_org: String, // ASN組織名
    #[serde(rename = "user_agent")]
    user_agent: UserAgent, // ifconfig.co が認識したUser-Agent情報
}

// ifconfig.co レスポンス内のUser-Agent情報
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct UserAgent {
    product: String,
    version: String,
    raw_value: String,
}

// エージェント起動時に初期化リクエスト(InitRequest)をマスターに送信
// 地理情報とシステム情報を収集してペイロードに含める
pub(crate) async fn handle_init_request(agent_id: &str, sink: Arc<Mutex<WsSink>>) -> Result<()> {
    info!("[Init] Determining geo data...");

    // ureqエージェントの設定 (IPv4のみ使用)
    let config = Agent::config_builder()
        .ip_family(ureq::config::IpFamily::Ipv4Only)
        .build();

    let agent: Agent = config.into();

    // ifconfig.co にリクエストを送信し、JSONレスポンスを取得・パース
    let geo_data = match agent
        .get("https://ifconfig.co/json")
        .call()?
        .body_mut()
        .read_json::<GeoData>()
    {
        Ok(data) => data,
        Err(e) => return Err(anyhow::anyhow!("JSON parse failed: {}", e)),
    };

    // 取得した地理情報をログ出力
    info!("[Init] IP: {}", geo_data.ip);
    info!("[Init] Country Code: {}", geo_data.country_iso);
    info!("[Init] Region: {}", geo_data.region_name);
    info!("[Init] City: {}", geo_data.city);
    info!("[Init] ASN: {}", geo_data.asn);
    info!("[Init] ASN Org: {}", geo_data.asn_org);

    // システム情報を収集
    info!("[Init] Gathering system information...");
    let mut sys = System::new_all();
    sys.refresh_all(); // 最新情報に更新

    // OSタイプ、バージョン、ホスト名、カーネルバージョンを取得
    let os_type = System::name().unwrap_or_else(|| "Unknown".to_string());
    let os_version = System::os_version().unwrap_or_else(|| "Unknown".to_string());
    let hostname = System::host_name().unwrap_or_else(|| "Unknown".to_string());
    let kernel_version = System::kernel_version().unwrap_or_else(|| "Unknown".to_string());
    // ユーザー名を取得 (whoamiクレート使用)
    let username = whoami::username();

    // 取得したシステム情報をログ出力
    info!("[Init] OS Type: {}", os_type);
    info!("[Init] OS Version: {}", os_version);
    info!("[Init] Hostname: {}", hostname);
    info!("[Init] Kernel Version: {}", kernel_version);
    info!("[Init] Username: {}", username);

    // InitRequestペイロードを作成 (地理情報 + システム情報)
    let payload = Payload::InitRequest {
        agent_id: agent_id.to_string(),
        ip: geo_data.ip,
        remote_host: geo_data.region_name.clone(), // remote_host は一旦 region_name を使用
        country_code: geo_data.country_iso,
        city: geo_data.city,
        region: geo_data.region_name.clone(),
        asn: geo_data.asn,
        asn_org: geo_data.asn_org,
        // システム情報をペイロードに追加
        os_type,
        os_version,
        hostname,
        kernel_version,
        username,
    };

    // 作成したペイロードをWebSocketで送信
    send_message(sink.clone(), payload).await?;
    info!("[{}] Sent init-request to master", agent_id);
    Ok(())
}

// マスターからの初期化レスポンス(InitResponse)を処理
// 結果をログに出力する
pub(crate) fn handle_init_response(success: bool, message: Option<String>) -> Result<()> {
    if success {
        // 初期化成功
        info!("[Init] Initialization succeeded");
        if let Some(msg) = message {
            info!("[Init] Server message: {}", msg);
        }
    } else {
        // 初期化失敗
        error!("[Init] ERROR: Initialization failed");
        if let Some(msg) = message {
            error!("[Init] Server error: {}", msg);
        }
    }
    Ok(())
}
