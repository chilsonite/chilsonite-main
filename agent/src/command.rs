use crate::ws::send_message;
use crate::WsSink; // Import WsSink from main/lib
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use common::Payload;
use log::{error, info};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

// Helper to send a command chunk over WebSocket
async fn send_command_chunk(
    sink: Arc<Mutex<WsSink>>,
    request_id: &str,
    chunk_id: u32,
    stream_type: u8,
    data: String,
) -> Result<()> {
    let payload = Payload::CommandResponseChunk {
        request_id: request_id.to_string(),
        chunk_id,
        stream_type,
        data,
    };
    send_message(sink, payload).await?;
    Ok(())
}

// Helper to send command completion over WebSocket
async fn send_command_complete(
    sink: Arc<Mutex<WsSink>>,
    request_id: &str,
    success: bool,
    exit_code: Option<i32>,
    error_message: Option<String>,
) -> Result<()> {
    let payload = Payload::CommandResponseTransferComplete {
        request_id: request_id.to_string(),
        success,
        exit_code,
        error_message,
    };
    send_message(sink, payload).await?;
    Ok(())
}

// サーバーからのコマンド実行リクエストを処理
pub(crate) async fn handle_command_request(
    request_id: String,
    command: String,
    sink: Arc<Mutex<WsSink>>,
) -> Result<()> {
    info!("[{}] Received command request: {}", request_id, command);

    // OSに応じてコマンド実行方法を決定
    #[cfg(target_os = "windows")]
    // Windowsの場合: cmd /C を使用
    let mut cmd = {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    };
    #[cfg(not(target_os = "windows"))]
    // Windows以外の場合: sh -c を使用
    let mut cmd = {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };

    // 標準出力と標準エラー出力をパイプに設定
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // コマンドを非同期プロセスとして起動
    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            error!("[{}] Failed to spawn command: {}", request_id, e);
            // send error completion
            send_command_complete(
                sink.clone(),
                &request_id,
                false,
                None,
                Some(format!("Failed to spawn command: {}", e)),
            )
            .await?;
            return Err(e.into());
        }
    };

    // 標準出力(stdout)のハンドルを取得
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let err_msg = "Failed to capture command stdout pipe.";
            error!("[{}] {}", request_id, err_msg);
            send_command_complete(
                sink.clone(),
                &request_id,
                false,
                None,
                Some(err_msg.to_string()),
            )
            .await?;
            return Err(anyhow::anyhow!(err_msg));
        }
    };
    // 標準エラー出力(stderr)のハンドルを取得
    let mut stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let err_msg = "Failed to capture command stderr pipe.";
            error!("[{}] {}", request_id, err_msg);
            send_command_complete(
                sink.clone(),
                &request_id,
                false,
                None,
                Some(err_msg.to_string()),
            )
            .await?;
            return Err(anyhow::anyhow!(err_msg));
        }
    };

    // チャンクIDの初期化
    let mut chunk_id: u32 = 1;
    // stdout/stderr読み取り用バッファ
    let mut stdout_buf = [0u8; 1024];
    let mut stderr_buf = [0u8; 1024];

    // 非同期タスク用にリクエストIDとSinkをクローン
    let req_id_clone = request_id.clone();
    let sink_clone = sink.clone();

    // stdoutとstderrを並行して読み取るタスク
    let read_streams_task = async {
        loop {
            tokio::select! {
                // stdoutから読み取り
                Ok(n) = stdout.read(&mut stdout_buf) => {
                    if n == 0 { break; } // stdoutが閉じた場合ループ終了
                    // 読み取ったデータをBase64エンコード
                    let data = STANDARD.encode(&stdout_buf[..n]);
                    // WebSocketで送信
                    if let Err(e) = send_command_chunk(sink_clone.clone(), &req_id_clone, chunk_id, 1, data).await {
                        error!("[{}] Failed to send stdout chunk: {}", req_id_clone, e);
                        break;
                    }
                    chunk_id += 1;
                },
                // stderrから読み取り
                Ok(n) = stderr.read(&mut stderr_buf) => {
                    if n == 0 { continue; }
                    let data = STANDARD.encode(&stderr_buf[..n]);
                    // WebSocketで送信
                    if let Err(e) = send_command_chunk(sink_clone.clone(), &req_id_clone, chunk_id, 2, data).await {
                        error!("[{}] Failed to send stderr chunk: {}", req_id_clone, e);
                        break;
                    }
                    chunk_id += 1;
                },
                else => { break; }
            }
        }
    };

    // プロセスの終了待機とストリーム読み取りを並行実行
    let (status_res, _) = tokio::join!(child.wait(), read_streams_task);

    // プロセスの終了ステータスを処理
    match status_res {
        Ok(status) => {
            info!("[{}] Command finished with status: {}", request_id, status);
            // 成功完了を通知
            send_command_complete(
                sink.clone(),
                &request_id,
                status.success(),
                status.code(),
                None,
            )
            .await?;
        }
        Err(e) => {
            error!("[{}] Failed to wait for command: {}", request_id, e);
            // エラー完了を通知
            send_command_complete(
                sink.clone(),
                &request_id,
                false,
                None,
                Some(format!("Failed to wait for command: {}", e)),
            )
            .await?;
        }
    }

    Ok(())
}
