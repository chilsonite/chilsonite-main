use crate::agent::{AgentConnection, AgentMap};
use crate::repository::{get_token, get_user_points, update_user_points};
use crate::websocket::send_message;
use crate::{PendingMap, PendingSender, Settings};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use common::Payload;
// Required for send_message -> lock.send
use log::{debug, error, info};
use rand::{rng, Rng};
use sqlx::PgPool;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::io::{split, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Duration};
use uuid::Uuid;

// Define SOCKS5 response constants
const SOCKS5_GENERAL_FAILURE: [u8; 10] = [0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
const SOCKS5_CONNECT_SUCCESS: [u8; 10] = [0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];

// Helper to select agent based on username pattern
fn choose_agent(
    agents: &AgentMap,
    username: Option<&str>,
) -> Option<(String, Arc<AgentConnection>)> {
    match username {
        Some(agent_id) if agent_id.starts_with("agent_") => agents
            .get(agent_id)
            .map(|e| (agent_id.to_string(), e.value().clone())),
        Some("all") | None => {
            let vec: Vec<_> = agents.iter().collect();
            if vec.is_empty() {
                return None;
            }
            let mut rng = rng();
            let idx = rng.random_range(0..vec.len());
            let e = &vec[idx];
            Some((e.key().clone(), e.value().clone()))
        }
        Some(country) if country.starts_with("country_") => {
            let codes: Vec<&str> = country[8..]
                .as_bytes()
                .chunks(2)
                .filter_map(|c| std::str::from_utf8(c).ok())
                .collect();
            let filtered: Vec<_> = agents
                .iter()
                .filter(|e| codes.contains(&e.value().metadata.country_code.as_str()))
                .collect();
            if filtered.is_empty() {
                return None;
            }
            let mut rng = rng();
            let idx = rng.random_range(0..filtered.len());
            let e = &filtered[idx];
            Some((e.key().clone(), e.value().clone()))
        }
        _ => None,
    }
}

// SOCKS5 ハンドシェイク（ユーザー名/パスワード認証）の実行
async fn perform_socks5_handshake(
    stream: &mut TcpStream,
    client_addr: &SocketAddr,
) -> Result<(Option<String>, String)> {
    info!("[SOCKS5] Handshake started with {}", client_addr);
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).await?;
    if buf[0] != 0x05 {
        return Err(anyhow!("Unsupported SOCKS version: {}", buf[0]));
    }
    let nmethods = buf[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;

    // ユーザー名/パスワード認証(0x02)をサポートしているか確認
    let use_auth = methods.contains(&0x02);
    if !use_auth {
        // 認証なし(0x00)は許可しない
        stream.write_all(&[0x05, 0xFF]).await?; // 0xFF: No acceptable methods
        return Err(anyhow!("SOCKS5: Username/Password authentication required"));
    }
    let selected_method = 0x02;
    stream.write_all(&[0x05, selected_method]).await?;

    // 認証情報の読み取り
    let mut auth_header = [0u8; 2];
    stream.read_exact(&mut auth_header).await?;
    if auth_header[0] != 0x01 {
        return Err(anyhow!("Unsupported auth version: {}", auth_header[0]));
    }
    let username_len = auth_header[1] as usize;
    let mut username = vec![0u8; username_len];
    stream.read_exact(&mut username).await?;
    let username = String::from_utf8(username)?;

    let mut password_len_buf = [0u8; 1];
    stream.read_exact(&mut password_len_buf).await?;
    let password_len = password_len_buf[0] as usize;
    let mut password = vec![0u8; password_len];
    stream.read_exact(&mut password).await?;
    let password = String::from_utf8(password)?;

    // 認証成功応答
    stream.write_all(&[0x01, 0x00]).await?;

    // 今後の処理のため、usernameとpasswordを返す
    Ok((Some(username), password))
}

// SOCKS5 の CONNECT リクエストから接続先アドレスとポートを抽出
async fn read_socks5_connect_request(
    stream: &mut TcpStream,
    client_addr: &SocketAddr,
) -> Result<(u8, String, u16)> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    if header[0] != 0x05 {
        return Err(anyhow!(
            "Unsupported SOCKS version in connect request: {}",
            header[0]
        ));
    }
    if header[1] != 0x01 {
        return Err(anyhow!("Unsupported SOCKS command: {}", header[1]));
    }
    let atyp = header[3];
    // アドレスタイプ(ATYP)に基づいてアドレスを読み取る
    let target_addr = match atyp {
        0x01 => {
            // IPv4: 4 バイト
            let mut addr_bytes = [0u8; 4];
            stream.read_exact(&mut addr_bytes).await?;
            Ipv4Addr::from(addr_bytes).to_string()
        }
        0x03 => {
            // ドメイン名: 最初の 1 バイトが長さ
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let len = len_buf[0] as usize;
            let mut domain = vec![0u8; len];
            stream.read_exact(&mut domain).await?;
            String::from_utf8(domain)?
        }
        0x04 => {
            // IPv6: 16 バイト（ここでは省略せず文字列化）
            let mut addr_bytes = [0u8; 16];
            stream.read_exact(&mut addr_bytes).await?;
            std::net::Ipv6Addr::from(addr_bytes).to_string()
        }
        _ => return Err(anyhow!("Unsupported address type: {}", atyp)),
    };
    // ポート番号を読み取る
    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf).await?;
    let target_port = u16::from_be_bytes(port_buf);
    info!(
        "[SOCKS5] CONNECT request from {} to {}:{}",
        client_addr, target_addr, target_port
    );
    Ok((atyp, target_addr, target_port))
}

// 指定のエージェントに connect-request を送信し、タイムアウト付きで connect-response を待つ
async fn send_connect_request_and_wait_for_response(
    agent_id: &str,
    agent_conn: &AgentConnection,
    request_id: &str,
    target_addr: &str,
    target_port: u16,
    address_type: u8,
    pending: PendingMap,
    settings: &Settings,
) -> Result<Payload> {
    // oneshotチャネルで応答を待機
    let (tx, rx) = oneshot::channel();
    {
        let mut pending_lock = pending.lock().await;
        // PendingMapにoneshot senderを登録
        pending_lock.insert(request_id.to_string(), PendingSender::Oneshot(tx));
    }
    // ConnectRequestペイロードを作成
    let payload = Payload::ConnectRequest {
        request_id: request_id.to_string(),
        target_addr: target_addr.to_string(),
        target_port,
        agent_id: None, // エージェントIDは接続自体に含まれるためNone
        address_type,
    };
    info!(
        "[{}] Sent connect-request (Agent: {}, Target: {}:{})",
        request_id, agent_id, target_addr, target_port
    );
    // WebSocket経由でリクエストを送信
    send_message(&agent_conn.sink, payload).await?;
    // タイムアウト付きで応答を待機
    match timeout(Duration::from_secs(settings.connect_timeout_seconds), rx).await {
        Ok(Ok(response)) => {
            if let Payload::ConnectResponse { success, .. } = &response {
                info!(
                    "[{}] Received connect-response from agent {}. Success: {}",
                    request_id, agent_id, success
                );
            }
            Ok(response)
        }
        Ok(Err(e)) => Err(anyhow!("Oneshot receiver error: {:?}", e)),
        Err(_) => {
            let mut pending_lock = pending.lock().await;
            // PendingMapから該当リクエストIDのエントリを削除
            pending_lock.remove(request_id);
            Err(anyhow!(
                "Timeout waiting for connect response from agent {}",
                agent_id
            ))
        }
    }
}

// クライアントとエージェント間の双方向データ転送を行う
async fn handle_socks5_data_transfer(
    stream: TcpStream,
    client_addr: SocketAddr,
    request_id: String,
    agent_conn: Arc<AgentConnection>,
    pending: PendingMap,
) -> Result<()> {
    info!("[{}][{}] Data transfer started", request_id, client_addr);
    // TcpStreamをreaderとwriterに分割
    let (mut reader, mut writer) = split(stream);
    // エージェントからのデータ応答用 mpsc チャネルを作成
    let (tx, mut rx) = mpsc::channel(32);
    {
        let mut pending_lock = pending.lock().await;
        // PendingMapにmpsc senderを登録
        pending_lock.insert(request_id.clone(), PendingSender::Mpsc(tx));
    }
    // クライアントからのデータ受信タスク（エージェントへの送信）
    let agent_conn_clone = agent_conn.clone();
    let req_id_clone = request_id.clone();
    let client_addr_clone = client_addr;
    let send_task = tokio::spawn(async move {
        let mut chunk_id: u32 = 1;
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => {
                    info!(
                        "[{}][{}] Client disconnected gracefully",
                        req_id_clone, client_addr_clone
                    );
                    // ClientDisconnectメッセージをエージェントに送信
                    let payload = Payload::ClientDisconnect {
                        request_id: req_id_clone.clone(),
                    };
                    if let Err(e) = send_message(&agent_conn_clone.sink, payload).await {
                        error!(
                            "[{}][{}] Failed to send client disconnect: {:?}",
                            req_id_clone, client_addr_clone, e
                        );
                    }
                    break;
                }
                Ok(n) => {
                    debug!(
                        "[{}][{}] Read {} bytes from client",
                        req_id_clone, client_addr_clone, n
                    );
                    let data_chunk = &buf[..n];
                    // Base64エンコード
                    let encoded = STANDARD.encode(data_chunk);
                    // DataRequestChunkメッセージをエージェントに送信
                    let payload = Payload::DataRequestChunk {
                        request_id: req_id_clone.clone(),
                        chunk_id,
                        data: encoded,
                    };
                    if let Err(e) = send_message(&agent_conn_clone.sink, payload).await {
                        error!("Failed to send data request: {:?}", e);
                        break;
                    }
                    chunk_id += 1;
                }
                Err(e) => {
                    error!(
                        "[{}][{}] Read error: {}",
                        req_id_clone, client_addr_clone, e
                    );
                    break;
                }
            }
        }
    });
    // エージェントからのデータ受信タスク（クライアントへの書き込み）
    let request_id_clone = request_id.clone();
    let write_task = tokio::spawn(async move {
        while let Some(payload) = rx.recv().await {
            match payload {
                Payload::DataResponseChunk {
                    request_id: _,
                    chunk_id: _,
                    data,
                } => {
                    // Base64デコード
                    match STANDARD.decode(&data) {
                        Ok(decoded) => {
                            // クライアントに書き込み
                            if let Err(e) = writer.write_all(&decoded).await {
                                error!(
                                    "[{}][{}] Failed to write data: {:?}",
                                    request_id_clone, client_addr_clone, e
                                );
                                break;
                            } else {
                                debug!(
                                    "[{}][{}] Wrote {} bytes to client",
                                    request_id_clone,
                                    client_addr_clone,
                                    decoded.len()
                                );
                            }
                        }
                        Err(e) => {
                            error!("Failed to decode base64 data: {:?}", e);
                        }
                    }
                }
                Payload::DataResponseTransferComplete {
                    request_id: _,
                    success,
                    error_message,
                } => {
                    if success {
                        info!(
                            "[{}][{}] Data transfer completed successfully",
                            request_id_clone, client_addr_clone
                        );
                    } else {
                        error!(
                            "[{}][{}] Transfer failed: {}",
                            request_id_clone,
                            client_addr_clone,
                            error_message.as_deref().unwrap_or("unknown error")
                        );
                    }
                    info!(
                        "[{}][{}] Data receiver terminated",
                        request_id_clone, client_addr_clone
                    );
                    break;
                }
                _ => {
                    error!("Unexpected payload type in data transfer");
                }
            }
        }
    });
    // 送受信タスクの完了を待機
    let _ = tokio::join!(send_task, write_task);
    info!("[{}][{}] Data transfer terminated", request_id, client_addr);
    // 転送終了後、PendingMapからエントリを削除
    let request_id_clone = request_id.clone();
    let mut pending_lock = pending.lock().await;
    pending_lock.remove(&request_id_clone);
    Ok(())
}

// SOCKS5サーバーメイン処理
// 1. ハンドシェイク処理
// 2. 接続要求解析
// 3. 利用可能なエージェントの選択
// 4. エージェントへの接続要求転送
// 5. 双方向データ転送の開始
async fn handle_socks5_connection(
    pool: PgPool,
    mut stream: TcpStream,
    client_addr: SocketAddr,
    agents: Arc<AgentMap>,
    pending: PendingMap,
    settings: Arc<Settings>,
) {
    info!("[Control] New SOCKS5 connection from {}", client_addr);
    // ハンドシェイク実行
    let (username, token) = match perform_socks5_handshake(&mut stream, &client_addr).await {
        Ok(v) => v,
        Err(e) => {
            error!("SOCKS5 handshake failed from {}: {:?}", client_addr, e);
            return;
        }
    };

    // token に紐づくレコード取得と有効期限チェック
    let token_rec = match get_token(&pool, &token).await {
        Ok(Some(rec)) if rec.expires_at > Utc::now() => rec,
        _ => {
            error!("Invalid or expired token: {}", token);
            let _ = stream
                .write_all(&[0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
            return;
        }
    };
    let user_id = token_rec.user_id;

    // ユーザのポイント取得
    let pts_rec = match get_user_points(&pool, user_id).await {
        Ok(Some(pr)) if pr.points >= 10 => pr,
        _ => {
            error!("Insufficient points for user {}", user_id);
            let _ = stream
                .write_all(&[0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
            return;
        }
    };

    // SOCKS5 CONNECTリクエストを読み取り、接続先情報を取得
    let (atyp, target_addr, target_port) =
        match read_socks5_connect_request(&mut stream, &client_addr).await {
            Ok(v) => v,
            Err(e) => {
                error!(
                    "Failed to read SOCKS5 request header from {}: {:?}",
                    client_addr, e
                );
                return;
            }
        };

    // Agent selection based on username
    let (agent_id, agent_conn) = match choose_agent(&agents, username.as_deref()) {
        Some(sel) => sel,
        None => {
            error!("Invalid or no agent available for username: {:?}", username);
            let _ = stream.write_all(&SOCKS5_GENERAL_FAILURE).await;
            return;
        }
    };

    // リクエストIDを生成
    let request_id = Uuid::now_v7().to_string(); // Use now_v7() for current time

    info!(
        "[{}] Selected agent {} for SOCKS5 CONNECT request from {}",
        request_id, agent_id, client_addr
    );
    // 選択されたエージェントにConnectRequestを送信し、応答を待つ
    let connect_response = match send_connect_request_and_wait_for_response(
        &agent_id,
        &agent_conn,
        &request_id,
        &target_addr,
        target_port,
        atyp,
        pending.clone(),
        &settings,
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => {
            error!(
                "Failed to send connect-request or timed out waiting for response from agent {}: {:?}",
                agent_id, e
            );
            // SOCKS5エラー応答を送信
            let response = [0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
            let _ = stream.write_all(&response).await;
            return;
        }
    };
    // エージェントからの応答を確認
    if let Payload::ConnectResponse { success, .. } = connect_response {
        if !success {
            // SOCKS5エラー応答を送信
            let response = [0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
            if let Err(e) = stream.write_all(&response).await {
                error!(
                    "[{}] Failed to send SOCKS5 CONNECT response to {}: {:?}",
                    request_id, client_addr, e
                );
            } else {
                info!(
                    "[{}] Sent SOCKS5 CONNECT response to {}. Success: false",
                    request_id, client_addr
                );
            }
            return;
        }
    } else {
        error!(
            "Unexpected payload type in response from agent {}",
            agent_id
        );
        let response = [0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        let _ = stream.write_all(&response).await;
        return;
    }
    // SOCKS5 接続成功応答をクライアントに送信
    if let Err(e) = stream.write_all(&SOCKS5_CONNECT_SUCCESS).await {
        error!(
            "[{}] Failed to send SOCKS5 CONNECT response: {:?}",
            request_id, e
        );
        return;
    }

    info!(
        "[{}] Sent SOCKS5 CONNECT response to {}. Success: true",
        request_id, client_addr
    );
    // 双方向のデータ転送を開始
    let transfer_result = handle_socks5_data_transfer(
        stream,
        client_addr,
        request_id.clone(),
        agent_conn,
        pending.clone(),
    )
    .await;
    if let Err(e) = transfer_result {
        error!("SOCKS5 data transfer error: {:?}, user: {}", e, user_id);
    } else {
        // 転送成功時に10ポイント消費
        let new_points = pts_rec.points - 10;
        if let Err(e) = update_user_points(&pool, user_id, new_points, Utc::now()).await {
            error!("Failed to update user points for {}: {}", user_id, e);
        } else {
            info!(
                "Consumed 10 points for user {}: {}->{}",
                user_id, pts_rec.points, new_points
            );
        }
    }
}

// SOCKS5 サーバーを起動し、クライアントからの接続を待ち受ける（トークン認証付き）
pub(crate) async fn run_socks5_server_with_token(
    pool: PgPool,
    agents: Arc<AgentMap>,
    pending: PendingMap,
    settings: Arc<Settings>,
) -> Result<()> {
    let addr = format!("{}:{}", settings.bind_address, settings.socks5_port);
    let listener = TcpListener::bind(&addr).await?;
    info!("[Control] SOCKS5 server started on {}", addr);
    loop {
        let (stream, client_addr) = listener.accept().await?;
        let agents_clone = agents.clone();
        let pending_clone = pending.clone();
        let settings_clone = settings.clone();
        let pool_clone = pool.clone();
        // 新しい接続ごとに非同期タスクを起動
        tokio::spawn(async move {
            handle_socks5_connection(
                pool_clone,
                stream,
                client_addr,
                agents_clone,
                pending_clone,
                settings_clone,
            )
            .await;
        });
    }
}
