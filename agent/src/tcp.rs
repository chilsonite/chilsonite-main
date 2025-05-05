use crate::ws::send_message;
use crate::{ConnectionMap, WsSink}; // Import from main/lib
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use common::Payload;
// Required for collect on lookup_host iterator
use log::{debug, error, info, warn};
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// TCP接続の読み取り側タスク（読み出したデータをチャンクに分割してWebSocketで送信）
// TCP接続の読み取り側ハンドラ
// TCPソケットからデータを読み取り、チャンクに分割してWebSocket経由でサーバーに送信
async fn tcp_read_handler(
    request_id: String,
    mut read_half: ReadHalf<TcpStream>,
    sink: Arc<Mutex<WsSink>>,
) -> Result<()> {
    // 読み取り用バッファ
    let mut buffer = [0u8; 1024];
    // チャンクIDの初期化
    let mut chunk_id = 1;
    loop {
        match read_half.read(&mut buffer).await {
            // 接続が閉じた場合 (EOF)
            Ok(0) => {
                info!(
                    "[{}] TCP read handler terminated (connection closed)",
                    request_id
                );
                // データ転送完了(成功)メッセージを送信
                send_data_response_complete(sink.clone(), &request_id, true, None).await?;
                info!(
                    "[{}] Data response transfer completed successfully",
                    request_id
                );
                break; // ループ終了
            }
            // データ受信成功
            Ok(n) => {
                debug!("[{}] Read {} bytes from TCP connection", request_id, n);
                // 読み取ったデータをスライスとして取得
                let chunk = &buffer[..n];
                // データチャンクメッセージを作成
                send_data_response_chunk(sink.clone(), &request_id, chunk_id, chunk).await?;
                chunk_id += 1;
            }
            // 読み取りエラー
            Err(e) => {
                error!("[{}] ERROR: Read failed - {}", request_id, e);
                // データ転送完了(失敗)メッセージを送信
                send_data_response_complete(sink.clone(), &request_id, false, Some(e.to_string()))
                    .await?;
                info!("[{}] Data response transfer failed: {}", request_id, e);
                break; // ループ終了
            }
        }
    }
    Ok(())
}

/// Check if an IP address is private (to prevent SSRF attacks)
// IPアドレスがプライベートIP（RFC 1918など）かループバック等かを判定 (SSRF対策)
fn is_private_ip(ip_str: &str) -> bool {
    // 文字列をIpAddrにパース
    if let Ok(ip) = IpAddr::from_str(ip_str) {
        match ip {
            IpAddr::V4(ipv4) => {
                // IPv4のプライベート範囲、ループバック、リンクローカル、ブロードキャスト等をチェック
                ipv4.is_private()
                    || ipv4.is_loopback()
                    || ipv4.is_link_local()
                    || ipv4.is_broadcast()
                    || ipv4.is_documentation()
                    || ipv4.is_unspecified()
            }
            IpAddr::V6(ipv6) => {
                // IPv6のループバック、未指定、ユニークローカルアドレス(ULA)等をチェック
                ipv6.is_loopback() ||
                ipv6.is_unspecified() ||
                // ULA (fc00::/7) のチェック
                (ipv6.segments()[0] & 0xfe00) == 0xfc00
            }
        }
    } else {
        // パースできない場合は安全側に倒してプライベートIPとみなす
        true
    }
}

/// マスターからの接続要求を受け、指定先へTCP接続を試行する
// マスターからの接続要求(ConnectRequest)を処理
// 指定されたターゲットへのTCP接続を試行し、結果をマスターに返す
pub(crate) async fn handle_connect_request(
    request_id: String,
    target_addr: String,
    target_port: u16,
    address_type: u8,
    connections: ConnectionMap,
    sink: Arc<Mutex<WsSink>>,
) -> Result<()> {
    info!(
        "[{}] Received connect-request for {}:{} (type: {})",
        request_id, target_addr, target_port, address_type
    );

    // ドメイン名解決が必要な場合
    // アドレスタイプに応じて接続先アドレスを決定 (DNS解決やSSRFチェックを含む)
    let resolved_addr = match address_type {
        0x03 => {
            // ドメイン名タイプ
            info!("[{}] Resolving domain: {}", request_id, target_addr);
            // ドメイン名を非同期に解決
            match tokio::net::lookup_host(format!("{}:{}", target_addr, target_port)).await {
                Ok(addrs) => {
                    // すべての解決済みアドレスを収集
                    // 解決されたアドレスリストを取得
                    let addresses: Vec<_> = addrs.collect();

                    // 最初のIPv4アドレスを探す
                    // 最初のIPv4アドレス、なければ最初のアドレスを選択
                    let selected_addr = addresses
                        .iter()
                        .find(|a| a.ip().is_ipv4())
                        .or_else(|| addresses.first());

                    match selected_addr {
                        Some(addr) => {
                            let ip_type = if addr.ip().is_ipv4() { "IPv4" } else { "IPv6" };
                            let ip_str = addr.ip().to_string();

                            // Check if the resolved IP is private
                            // 解決後のIPアドレスがプライベートIPでないかチェック (SSRF対策)
                            if is_private_ip(&ip_str) {
                                error!(
                                    "[{}] SSRF attempt detected: resolved to private IP {}",
                                    request_id, ip_str
                                );
                                let payload = Payload::ConnectResponse {
                                    request_id: request_id.clone(),
                                    success: false,
                                };
                                send_message(sink.clone(), payload).await?;
                                return Ok(());
                            }

                            info!("[{}] DNS resolved to: {} ({})", request_id, ip_str, ip_type);
                            ip_str
                        }
                        None => {
                            let msg = format!("DNS resolution failed for {}", target_addr);
                            error!("[{}] {}", request_id, msg);
                            let payload = Payload::ConnectResponse {
                                request_id: request_id.clone(),
                                success: false,
                            };
                            send_message(sink.clone(), payload).await?;
                            return Ok(());
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("DNS resolution error: {}", e);
                    error!("[{}] {}", request_id, msg);
                    let payload = Payload::ConnectResponse {
                        request_id: request_id.clone(),
                        success: false,
                    };
                    send_message(sink.clone(), payload).await?;
                    return Ok(());
                }
            }
        }
        0x01 => {
            // IPv4アドレスの場合
            info!("[{}] Using IPv4 address: {}", request_id, target_addr);

            // Check if the direct IP is private
            // 直接指定されたIPアドレスがプライベートIPでないかチェック (SSRF対策)
            if is_private_ip(target_addr.as_str()) {
                error!(
                    "[{}] SSRF attempt detected: connection to private IP {} blocked",
                    request_id, target_addr
                );
                let payload = Payload::ConnectResponse {
                    request_id: request_id.clone(),
                    success: false,
                };
                send_message(sink.clone(), payload).await?;
                return Ok(());
            }

            target_addr.to_string()
        }
        0x04 => {
            // IPv6アドレスの場合
            info!("[{}] Using IPv6 address: {}", request_id, target_addr);

            // Check if the IPv6 address is private
            // 直接指定されたIPv6アドレスがプライベートIPでないかチェック (SSRF対策)
            if is_private_ip(target_addr.as_str()) {
                error!(
                    "[{}] SSRF attempt detected: connection to private IPv6 {} blocked",
                    request_id, target_addr
                );
                let payload = Payload::ConnectResponse {
                    request_id: request_id.clone(),
                    success: false,
                };
                send_message(sink.clone(), payload).await?;
                return Ok(());
            }

            format!("[{}]", target_addr) // IPv6アドレスを[]で囲む
        }
        _ => {
            error!(
                "[{}] ERROR: Invalid address type: {}",
                request_id, address_type
            );
            let payload = Payload::ConnectResponse {
                request_id: request_id.clone(),
                success: false,
            };
            send_message(sink.clone(), payload).await?;
            return Ok(());
        }
    };

    let addr = format!("{}:{}", resolved_addr, target_port);
    match TcpStream::connect(&addr).await {
        Ok(stream) => {
            info!(
                "[{}] Established TCP connection to {}:{}",
                request_id, resolved_addr, target_port
            );
            // TCPストリームをsplitしwrite_halfを保持、read_halfは別タスクで処理
            let (read_half, write_half) = tokio::io::split(stream);
            {
                let mut map = connections.lock().await;
                map.insert(request_id.clone(), write_half);
            }
            // TCP読み取りタスクをspawn
            let sink_clone = sink.clone();
            let req_id_clone = request_id.clone();
            tokio::spawn(async move {
                if let Err(e) = tcp_read_handler(req_id_clone.clone(), read_half, sink_clone).await
                {
                    error!("[{}] TCP read handler error: {}", req_id_clone, e);
                }
            });
            // 成功レスポンスを送信
            let payload = Payload::ConnectResponse {
                request_id: request_id.clone(),
                success: true,
            };
            info!("[{}] Sent connect-response (success: true)", request_id);
            info!("[{}] Data transfer started", request_id);
            send_message(sink, payload).await?;
        }
        Err(e) => {
            error!(
                "[{}] ERROR: Failed to connect to {}:{} - {}",
                request_id, resolved_addr, target_port, e
            );
            info!("[{}] Sent connect-response (success: false)", request_id);
            let payload = Payload::ConnectResponse {
                request_id: request_id.clone(),
                success: false,
            };
            send_message(sink.clone(), payload).await?;
        }
    }
    Ok(())
}

/// マスターから送られてくるデータチャンク要求を処理しTCP接続へ書き込む
// マスターからのデータチャンク要求(DataRequestChunk)を処理
// Base64デコードし、対応するTCP接続へ書き込む
pub(crate) async fn handle_data_request(
    request_id: String,
    chunk_id: u32,
    data: String,
    connections: ConnectionMap,
) -> Result<()> {
    debug!(
        "[{}] Received data-request (chunk_id: {}, size: {} bytes)",
        request_id,
        chunk_id,
        data.len()
    );
    // Base64デコード
    let decoded = match STANDARD.decode(&data) {
        Ok(d) => d,
        Err(e) => {
            error!("[{}] ERROR: Failed to decode base64 - {}", request_id, e);
            return Ok(()); // Don't propagate decode error, just log and skip chunk
        }
    };
    // 対応するTCP接続のwrite_halfを取得して書き込み
    // ConnectionMapをロックして対応するWriteHalfを取得
    let mut conns = connections.lock().await;
    if let Some(write_half) = conns.get_mut(&request_id) {
        // TCP接続にデコードしたデータを書き込み
        if let Err(e) = write_half.write_all(&decoded).await {
            // Log error, but don't necessarily close connection or return Err immediately
            // The read handler or subsequent writes might still function or report errors.
            // 書き込みエラー発生時もログ出力のみ（接続が閉じている可能性など）
            error!(
                "[{}] Failed to write data to TCP connection: {:?}",
                request_id, e
            );
        } else {
            debug!(
                "[{}] Wrote {} bytes to TCP connection",
                request_id,
                decoded.len()
            );
        }
    } else {
        // Connection might have been closed already, log as warning or debug
        // 対応する接続が見つからない場合（既に閉じられている可能性）
        warn!(
            "[{}] ERROR: No TCP connection found for data request (chunk_id: {})",
            request_id, chunk_id
        );
    }
    Ok(())
}

// Helper to send a data response chunk over WebSocket
async fn send_data_response_chunk(
    sink: Arc<Mutex<WsSink>>,
    request_id: &str,
    chunk_id: u32,
    data: &[u8],
) -> Result<()> {
    let encoded = STANDARD.encode(data);
    let payload = Payload::DataResponseChunk {
        request_id: request_id.to_string(),
        chunk_id,
        data: encoded,
    };
    send_message(sink, payload).await?;
    Ok(())
}

// Helper to send a transfer-complete notification over WebSocket
async fn send_data_response_complete(
    sink: Arc<Mutex<WsSink>>,
    request_id: &str,
    success: bool,
    error_message: Option<String>,
) -> Result<()> {
    let payload = Payload::DataResponseTransferComplete {
        request_id: request_id.to_string(),
        success,
        error_message,
    };
    send_message(sink, payload).await?;
    Ok(())
}

// Unit tests
#[cfg(test)]
mod tests {
    use super::*; // Import items from the parent module (tcp.rs)

    #[test]
    fn test_is_private_ip_v4_private() {
        assert!(is_private_ip("10.0.0.1"));
        assert!(is_private_ip("172.16.0.1"));
        assert!(is_private_ip("172.31.255.254"));
        assert!(is_private_ip("192.168.1.100"));
    }

    #[test]
    fn test_is_private_ip_v4_loopback() {
        assert!(is_private_ip("127.0.0.1"));
    }

    #[test]
    fn test_is_private_ip_v4_link_local() {
        assert!(is_private_ip("169.254.1.1"));
    }

    #[test]
    fn test_is_private_ip_v4_unspecified() {
        assert!(is_private_ip("0.0.0.0"));
    }

    #[test]
    fn test_is_private_ip_v4_public() {
        assert!(!is_private_ip("8.8.8.8"));
        assert!(!is_private_ip("1.1.1.1"));
    }

    #[test]
    fn test_is_private_ip_v6_ula() {
        // Unique Local Addresses (fc00::/7)
        assert!(is_private_ip("fd00::1"));
        assert!(is_private_ip("fc12:3456:789a:1::1"));
    }

    #[test]
    fn test_is_private_ip_v6_loopback() {
        assert!(is_private_ip("::1"));
    }

    #[test]
    fn test_is_private_ip_v6_unspecified() {
        assert!(is_private_ip("::"));
    }

    #[test]
    fn test_is_private_ip_v6_public() {
        assert!(!is_private_ip("2001:db8::1")); // Documentation address
        assert!(!is_private_ip("2606:4700:4700::1111")); // Cloudflare DNS
    }

    #[test]
    fn test_is_private_ip_invalid() {
        // Invalid IP strings should be treated as private for safety
        assert!(is_private_ip("not an ip address"));
        assert!(is_private_ip("192.168.1.256"));
        assert!(is_private_ip(""));
    }
}
