// モジュールの宣言
mod command;
mod init;
mod tcp;
mod ws;

use anyhow::Result;
use env_logger;
use futures::stream::{SplitSink, SplitStream};
use futures::StreamExt;
use log::{info, warn};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use sysinfo::System;
use tokio::io::WriteHalf;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};
use url::Url;
use whoami;

// 接続先マスターサーバーのデフォルトURL
const DEFAULT_MASTER_URL: &str = "ws://127.0.0.1:3005";

// 型エイリアス: WebSocket送信用シンク
type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
// 型エイリアス: WebSocket受信用ストリーム
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

// グローバルなTCP接続マップ: リクエストIDとTCP書き込みストリーム(WriteHalf)を関連付け
// Arc<Mutex<...>> でスレッドセーフな共有アクセスを実現
type ConnectionMap = Arc<Mutex<HashMap<String, WriteHalf<TcpStream>>>>;

// メインエントリーポイント: エージェントの起動とマスターサーバーへの接続処理
#[tokio::main]
async fn main() -> Result<()> {
    // ロガーの初期化 (環境変数 RUST_LOG またはデフォルト "info" レベル)
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // エージェントIDの基となるマシン固有IDを取得
    let base_id = {
        // Android環境向けの条件付きコンパイル
        #[cfg(target_os = "android")]
        {
            warn!("Target OS is Android. Using sysinfo fallback for agent ID.");
            // machine-uid が使えないため、sysinfoから取得した情報で代替IDを生成
            let mut sys = System::new_all();
            sys.refresh_all();
            let hostname = System::host_name().unwrap_or_else(|| "unknown_host".to_string());
            let username = whoami::username();
            let os_type = System::name().unwrap_or_else(|| "Android".to_string());
            let combined_info = format!("{}-{}-{}", hostname, username, os_type);
            let mut hasher = DefaultHasher::new();
            combined_info.hash(&mut hasher);
            let hash_val = hasher.finish();
            format!("{:x}", hash_val) // ハッシュ値を16進数文字列化
        }
        // Raspberry Pi (Linux aarch64) 環境向けの条件付きコンパイル
        #[cfg(all(
            not(target_os = "android"),
            target_os = "linux",
            target_arch = "aarch64"
        ))]
        {
            warn!("Detected Raspberry Pi environment. Using sysinfo fallback for agent ID.");
            // machine-uid が使えないため、sysinfoから取得した情報で代替IDを生成
            let mut sys = System::new_all();
            sys.refresh_all();
            let hostname = System::host_name().unwrap_or_else(|| "unknown_host".to_string());
            let username = whoami::username();
            let os_type = System::name().unwrap_or_else(|| "unknown_os".to_string());
            let combined_info = format!("{}-{}-{}", hostname, username, os_type);
            let mut hasher = DefaultHasher::new();
            combined_info.hash(&mut hasher);
            let hash_val = hasher.finish();
            format!("{:x}", hash_val)
        }
        // 上記以外の環境 (machine-uid が利用可能なはず)
        #[cfg(all(
            not(target_os = "android"),
            not(all(target_os = "linux", target_arch = "aarch64"))
        ))]
        {
            // machine-uid クレートを使用してマシン固有IDを取得試行
            match machine_uid::get() {
                Ok(id) => {
                    info!("Using machine-uid for agent ID.");
                    id.replace("-", "") // ハイフンを除去
                }
                Err(e) => {
                    // machine-uid 取得失敗時はsysinfoフォールバック
                    warn!(
                        "Failed to get machine ID via machine-uid: {}. Falling back to sysinfo.",
                        e
                    );
                    let mut sys = System::new_all();
                    sys.refresh_all();
                    let hostname =
                        System::host_name().unwrap_or_else(|| "unknown_host".to_string());
                    let username = whoami::username();
                    let os_type = System::name().unwrap_or_else(|| "unknown_os".to_string());
                    let combined_info = format!("{}-{}-{}", hostname, username, os_type);
                    let mut hasher = DefaultHasher::new();
                    combined_info.hash(&mut hasher);
                    let hash_val = hasher.finish();
                    format!("{:x}", hash_val)
                }
            }
        }
    };

    // 取得したIDを12文字に整形 (パディングまたは切り捨て)
    let padded_id = format!("{:<12}", base_id)
        .chars()
        .take(12)
        .collect::<String>();
    // 最終的なエージェントIDを "agent_" プレフィックス付きで生成
    let agent_id = format!("agent_{}", padded_id);

    // コマンドライン引数を取得
    let args: Vec<String> = std::env::args().collect();
    // 引数があればそれをマスターURLとして使用、なければデフォルトURLを使用
    let master_url = if args.len() > 1 {
        args[1].clone()
    } else {
        DEFAULT_MASTER_URL.to_string()
    };

    info!("Starting AGENT with ID: {}", agent_id);
    info!("Connecting to master at: {}", master_url);

    // マスターURLをパース
    let url = Url::parse(&master_url)?;
    // WebSocket接続を非同期に確立
    let (ws_stream, _) = connect_async(url.as_str()).await?;
    info!("WebSocket connection established with master");

    // WebSocketストリームを送受信に分割
    let (sink, stream): (WsSink, WsStream) = ws_stream.split();
    // 送信シンクをArc<Mutex<>>でラップして共有可能に
    let sink = Arc::new(Mutex::new(sink));
    // TCP接続マップを初期化
    let connections: ConnectionMap = Arc::new(Mutex::new(HashMap::new()));

    // 初期化リクエストを送信 (initモジュールの関数を使用)
    init::handle_init_request(&agent_id, sink.clone()).await?;

    // WebSocketイベントループを開始 (wsモジュールの関数を使用)
    ws::event_loop(stream, sink, connections).await?;

    Ok(())
}
