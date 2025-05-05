use crate::{command, init, tcp, ConnectionMap, WsSink, WsStream}; // Import from main/lib and other modules
use anyhow::Result;
use common::Payload;
use futures::{SinkExt, StreamExt};
use log::{debug, error, info};
use std::sync::Arc;
use tokio::io::AsyncWriteExt; // For shutdown
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::protocol::Message;
use futures::Future;

// WebSocket送信シンクへペイロードをJSON化して送信
pub(crate) async fn send_message(sink: Arc<Mutex<WsSink>>, payload: Payload) -> Result<()> {
    // ペイロードをJSON文字列にシリアライズ
    let json = serde_json::to_string(&payload)?;
    // Sinkをロックしてメッセージを送信
    let mut guard = sink.lock().await;
    guard.send(Message::text(json)).await?; // Use Message::text()
    Ok(())
}

// Helper to spawn a WebSocket handler future with error logging
fn spawn_ws<Fut>(id: String, fut: Fut)
where
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = fut.await {
            error!("[{}] WebSocket handler error: {:?}", id, e);
        }
    });
}

// WebSocketの受信ループ（イベントループ）: マスターからのメッセージを処理
// 受信したペイロードの種類に応じて対応するハンドラを非同期に実行
pub(crate) async fn event_loop(
    mut stream: WsStream,
    sink: Arc<Mutex<WsSink>>,
    connections: ConnectionMap,
) -> Result<()> {
    // WebSocketストリームからメッセージを順次受信
    while let Some(msg) = stream.next().await {
        match msg {
            // テキストメッセージの場合
            Ok(Message::Text(text)) => {
                // 受信したテキストをJSONペイロードとしてパース
                match serde_json::from_str::<Payload>(&text) {
                    Ok(payload) => match payload {
                        // InitResponseペイロードの処理
                        Payload::InitResponse { success, message } => {
                            info!("[Control] Received init-response");
                            // initモジュールのハンドラを呼び出し
                            let _ = init::handle_init_response(success, message);
                        }
                        // ConnectRequestペイロードの処理
                        Payload::ConnectRequest { request_id, target_addr, target_port, address_type, .. } => {
                            info!("[Control] Received connect-request");
                            let req_id = request_id.clone();
                            let target = target_addr.clone();
                            spawn_ws(
                                req_id.clone(),
                                tcp::handle_connect_request(
                                    req_id.clone(),
                                    target,
                                    target_port,
                                    address_type,
                                    connections.clone(),
                                    sink.clone(),
                                ),
                            );
                        }
                        // DataRequestChunkペイロードの処理
                        Payload::DataRequestChunk { request_id, chunk_id, data } => {
                            debug!("[Control] Received data-chunk-request");
                            let req_id = request_id.clone();
                            let chunk_data = data.clone();
                            spawn_ws(
                                req_id.clone(),
                                tcp::handle_data_request(
                                    req_id.clone(),
                                    chunk_id,
                                    chunk_data,
                                    connections.clone(),
                                ),
                            );
                        }
                        // DataRequestTransferCompleteペイロードの処理 (サーバー -> エージェント方向のデータ転送完了通知)
                        Payload::DataRequestTransferComplete {
                            request_id,
                            success,
                            error_message,
                        } => {
                            info!("[Control] Received data-request-transfer-complete");
                            // ConnectionMapをロック
                            let mut conns = connections.lock().await;
                            // 対応する接続のWriteHalfを取得して削除
                            if let Some(mut write_half) = conns.remove(&request_id) {
                                if !success {
                                    // 転送失敗時のログ出力
                                    error!(
                                        "[{}] Transfer failed: {}",
                                        request_id,
                                        error_message.unwrap_or_default()
                                    );
                                }
                                // TCP接続の書き込み側をシャットダウン
                                if let Err(e) = write_half.shutdown().await {
                                    error!(
                                        "[{}] Error shutting down connection: {}",
                                        request_id, e
                                    );
                                }
                                info!(
                                    "[{}] Connection closed after transfer complete (success: {})",
                                    request_id, success
                                );
                                // 完了ログ
                                info!("[{}] Data request transfer completed", request_id,);
                            }
                        }
                        // ClientDisconnectペイロードの処理 (SOCKSクライアント切断通知)
                        Payload::ClientDisconnect { request_id } => {
                            info!("[Control] Received client-disconnect for {}", request_id);
                            // ConnectionMapをロック
                            let mut conns = connections.lock().await;
                            // 対応する接続のWriteHalfを取得して削除
                            if let Some(mut write_half) = conns.remove(&request_id) {
                                // TCP接続の書き込み側をシャットダウン
                                if let Err(e) = write_half.shutdown().await {
                                    error!(
                                        "[{}] Error shutting down connection: {}",
                                        request_id, e
                                    );
                                }
                                info!("[{}] Connection closed and removed", request_id);
                            } else {
                                // 接続が見つからない場合（既に閉じられている可能性）
                                info!("[{}] No active connection found", request_id);
                            }
                        }
                        // CommandRequestペイロードの処理
                        Payload::CommandRequest { request_id, command } => {
                            info!("[Control] Received command-request");
                            let req_id = request_id.clone();
                            let cmd = command.clone();
                            spawn_ws(
                                req_id.clone(),
                                command::handle_command_request(req_id.clone(), cmd, sink.clone()),
                            );
                        }
                        // 上記以外のペイロードタイプ
                        _ => {
                            debug!("[Control] Received unhandled message type");
                        }
                    },
                    // JSONパース失敗
                    Err(e) => {
                        error!(
                            "[Control] ERROR: Failed to parse message: {}, {:?}",
                            text, e
                        );
                    }
                }
            }
            // テキスト以外のメッセージ（バイナリなど）
            Ok(_) => {
                debug!("[Control] Received non-text message");
            }
            // WebSocketエラー
            Err(e) => {
                error!("[Control] ERROR: WebSocket error - {}", e);
                break; // WebSocketエラー発生時はループを抜ける
            }
        }
    }
    // WebSocket接続が切断された場合
    info!("[Control] Disconnected from master program");
    Ok(())
}
