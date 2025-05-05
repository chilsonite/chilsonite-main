use crate::agent::{AgentConnection, AgentMap, AgentMetadata};
use crate::{CommandResponseMap, PendingMap, WsSink, WsStream};
use crate::{PendingSender, Settings};
use anyhow::{anyhow, Result};
use common::Payload;
use futures::{SinkExt, StreamExt};
use log::{debug, error, info};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message};

// WebSocket経由でペイロードを送信するヘルパー関数
// JSONシリアライズ → Textメッセージとして送信
pub(crate) async fn send_message(sink: &Mutex<WsSink>, payload: Payload) -> Result<()> {
    let json = serde_json::to_string(&payload)?;
    let mut lock = sink.lock().await;
    lock.send(Message::text(json)).await?; // Use Message::text()
    Ok(())
}

// エージェントからの初期化リクエストを処理し、AgentMap に登録して InitResponse を送信
async fn handle_init_request(
    agent_id: String,
    sink: WsSink,
    agents: Arc<AgentMap>,
    metadata: AgentMetadata,
) -> Result<()> {
    info!(
        "[Init] Received init-request from agent. Agent ID: {} | Metadata: {:?}",
        agent_id, metadata
    );

    // エージェントIDのフォーマットチェック
    if !agent_id.starts_with("agent_") {
        let temp_conn = Arc::new(AgentConnection {
            sink: Mutex::new(sink),
            metadata,
        });
        send_message(
            &temp_conn.sink,
            Payload::InitResponse {
                success: false,
                message: Some("Agent ID must start with 'agent_'".to_string()),
            },
        )
        .await?;
        return Err(anyhow!("Invalid agent ID format: {}", agent_id));
    }

    let agent_conn = Arc::new(AgentConnection {
        sink: Mutex::new(sink),
        metadata,
    });
    agents.insert(agent_id.clone(), agent_conn.clone());
    // 初期化レスポンス送信
    send_message(
        &agent_conn.sink,
        Payload::InitResponse {
            success: true,
            message: None,
        },
    )
    .await?;
    info!(
        "[Init] Agent registered successfully. Agent ID: {}",
        agent_id
    );
    Ok(())
}

// エージェントから受信した ConnectResponse を、PendingMap 経由で送信元に通知
async fn handle_connect_response(payload: Payload, pending: PendingMap) -> Result<()> {
    if let Payload::ConnectResponse { request_id, .. } = &payload {
        let mut lock = pending.lock().await;
        if let Some(pending_sender) = lock.remove(request_id) {
            match pending_sender {
                PendingSender::Oneshot(sender) => {
                    if let Err(e) = sender.send(payload) {
                        error!("Failed to send oneshot response: {:?}", e);
                    }
                }
                _ => {
                    error!("Unexpected pending sender type for connect response");
                }
            }
        }
    } else {
        error!(
            "Received unexpected message in handle_connect_response: {:?}",
            payload
        );
    }
    Ok(())
}

// エージェントからの DataResponseChunk または DataResponseTransferComplete を処理
async fn handle_data_response(payload: Payload, pending: PendingMap) -> Result<()> {
    match &payload {
        Payload::DataResponseChunk { request_id, .. }
        | Payload::DataResponseTransferComplete { request_id, .. } => {
            let mut lock = pending.lock().await;
            if let Some(pending_sender) = lock.get_mut(request_id) {
                match pending_sender {
                    PendingSender::Mpsc(sender) => {
                        if let Err(e) = sender.send(payload).await {
                            error!("Failed to send data response: {:?}", e);
                        }
                    }
                    _ => {
                        error!("Unexpected pending sender type for data response");
                    }
                }
            } else {
                error!(
                    "No pending sender found for data response with request_id: {}",
                    request_id
                );
            }
        }
        _ => {
            debug!("Received unexpected message: {:?}", payload);
        }
    }
    Ok(())
}

// エージェントからの CommandResponseChunk または CommandResponseTransferComplete を処理
async fn handle_command_response(
    payload: Payload,
    command_responses: CommandResponseMap,
) -> Result<()> {
    match &payload {
        Payload::CommandResponseChunk { request_id, .. }
        | Payload::CommandResponseTransferComplete { request_id, .. } => {
            let mut lock = command_responses.lock().await;
            if let Some(sender) = lock.get_mut(request_id) {
                if sender.send(payload.clone()).await.is_err() {
                    // 受信側（SSE接続）が切断された場合、senderを削除
                    error!(
                        "[{}] SSE receiver dropped for command response. Removing sender.",
                        request_id
                    );
                    lock.remove(request_id);
                }
            } else {
                // サーバーがコマンド応答を待機中にタイムアウトした場合や、
                // コマンド完了前にSSE接続が閉じられた場合に発生する可能性
                debug!(
                    "[{}] No pending command response sender found for request_id.",
                    request_id
                );
            }
            // 転送完了時にsenderを削除
            if let Payload::CommandResponseTransferComplete { .. } = payload {
                lock.remove(request_id);
                debug!("[{}] Removed command response sender.", request_id);
            }
        }
        _ => {
            error!(
                "Received unexpected message in handle_command_response: {:?}",
                payload
            );
        }
    }
    Ok(())
}

// 各エージェントとの WebSocket 接続のイベントループ
async fn handle_agent_connection(
    stream: TcpStream,
    agents: Arc<AgentMap>,
    pending: PendingMap,
    command_responses: CommandResponseMap,
) {
    // ハンドシェイク実施
    // let peer_addr = stream.peer_addr().ok();
    if let Err(e) = async {
        let ws_stream = accept_async(stream).await.map_err(|e| {
            error!("[Control] WebSocket handshake failed: {:?}", e);
            e
        })?;
        let (sink, mut stream): (WsSink, WsStream) = ws_stream.split();
        // 初回メッセージとして init-request を待機
        let init_msg = stream.next().await;
        if (init_msg).is_none() {
            error!("WebSocket connection closed before init-request");
            return Ok(());
        }
        let init_msg = init_msg.unwrap();
        let init_payload: Payload = match init_msg {
            Ok(Message::Text(text)) => serde_json::from_str(&text).map_err(|e| {
                error!("Failed to parse JSON from agent: {}", text);
                e
            })?,
            _ => {
                error!("Expected init-request but got non-text message");
                return Ok(());
            }
        };
        // InitRequestペイロードから各フィールドを抽出
        let (
            agent_id,
            ip,
            remote_host,
            country_code,
            city,
            region,
            asn,
            asn_org,
            os_type,
            os_version,
            hostname,
            kernel_version,
            username,
        ) = if let Payload::InitRequest {
            agent_id,
            ip,
            remote_host,
            country_code,
            city,
            region,
            asn,
            asn_org,
            os_type,
            os_version,
            hostname,
            kernel_version,
            username,
        } = init_payload
        {
            (
                agent_id,
                ip,
                remote_host,
                country_code,
                city,
                region,
                asn,
                asn_org,
                os_type,
                os_version,
                hostname,
                kernel_version,
                username,
            )
        } else {
            error!("Expected init-request but got: {:?}", init_payload);
            return Ok(());
        };

        info!(
            "[Init] Received init-request from agent. Agent ID: {}",
            agent_id
        );
        // エージェント登録＆初期化レスポンス送信
        let metadata = AgentMetadata {
            ip,
            remote_host,
            country_code,
            city,
            region,
            asn,
            asn_org,
            os_type,
            os_version,
            hostname,
            kernel_version,
            username,
        };
        handle_init_request(agent_id.clone(), sink, agents.clone(), metadata).await?;

        // その後のメッセージを処理するループ
        while let Some(message) = stream.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    let payload: Payload = match serde_json::from_str(&text) {
                        Ok(p) => p,
                        Err(e) => {
                            error!("Failed to parse JSON from agent: {}, {:?}", text, e);
                            continue;
                        }
                    };
                    // 受信したペイロードの種類に応じて処理を分岐
                    match payload {
                        Payload::ConnectResponse { .. } => {
                            if let Err(e) = handle_connect_response(payload, pending.clone()).await
                            {
                                error!("Error handling connect-response: {:?}", e);
                            }
                        }
                        Payload::DataResponseChunk { .. }
                        | Payload::DataResponseTransferComplete { .. } => {
                            if let Err(e) = handle_data_response(payload, pending.clone()).await {
                                error!("Error handling data-response: {:?}", e);
                            }
                        }
                        // コマンド応答の処理
                        Payload::CommandResponseChunk { .. }
                        | Payload::CommandResponseTransferComplete { .. } => {
                            if let Err(e) =
                                handle_command_response(payload, command_responses.clone()).await
                            {
                                error!("Error handling command-response: {:?}", e);
                            }
                        }
                        _ => {
                            debug!("Received unexpected message from agent: {:?}", payload);
                        }
                    }
                }
                Ok(other) => {
                    error!("Unexpected WebSocket message: {:?}", other);
                }
                Err(e) => {
                    error!("[{}] WebSocket error: {:?}", agent_id, e);
                    break;
                }
            }
        }
        info!("[{}] Connection closed", agent_id);
        // エージェント切断時は AgentMap から削除
        agents.remove(&agent_id);
        Ok::<(), anyhow::Error>(())
    }
    .await
    {
        error!("Error in handle_agent_connection: {:?}", e);
    }
}

// WebSocket サーバーを起動し、エージェントからの接続を待ち受ける
pub(crate) async fn run_websocket_server(
    agents: Arc<AgentMap>,
    pending: PendingMap,
    command_responses: CommandResponseMap,
    settings: Arc<Settings>,
) -> Result<()> {
    let addr = format!("{}:{}", settings.bind_address, settings.websocket_port);
    let listener = TcpListener::bind(&addr).await?;
    info!("[Control] WebSocket server started on {}", addr);
    loop {
        let (stream, addr) = listener.accept().await?;
        info!("[Control] New WebSocket connection from {}", addr);
        let agents_clone = agents.clone();
        let pending_clone = pending.clone();
        let command_responses_clone = command_responses.clone();
        let _settings_clone = settings.clone(); // 現在未使用だが将来のためにクローン
        tokio::spawn(async move {
            handle_agent_connection(stream, agents_clone, pending_clone, command_responses_clone)
                .await;
        });
    }
}
