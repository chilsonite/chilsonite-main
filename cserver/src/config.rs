use anyhow::{anyhow, Result};
use config::Config;
use serde::Deserialize;

// 設定ファイルの内容を保持する構造体
#[derive(Debug, Deserialize, Clone)]
pub(crate) struct Settings {
    pub websocket_port: u16,
    pub socks5_port: u16,
    pub bind_address: String,
    pub connect_timeout_seconds: u64,
}

// 設定ファイル（例: cserver.toml）を読み込む関数
pub(crate) async fn load_config() -> Result<Settings> {
    let config = Config::builder()
        // "chilsonite" という名前のファイル（拡張子なし）を読み込み元として追加
        .add_source(config::File::with_name("chilsonite"))
        .build()?;

    // 読み込んだ設定を Settings 構造体にデシリアライズ
    config
        .try_deserialize()
        .map_err(|e| anyhow!("Failed to parse config: {}", e))
}
