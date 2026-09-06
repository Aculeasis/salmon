use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

use crate::config::ServerConfig;
use crate::domain::Event;

const MAX_MESSAGE_BYTES: usize = 1 << 20;
const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run_client(
    server: ServerConfig,
    events: mpsc::Sender<Event>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut reconnect_seconds = 0u64;
    loop {
        if reconnect_seconds > 0 {
            tokio::select! {
                _ = sleep(Duration::from_secs(reconnect_seconds)) => {}
                _ = shutdown.changed() => return,
            }
        }
        let url = format!("ws://{}/api/v1/wsconnect", server.addr);
        let connected = tokio::select! {
            result = connect_async_with_config(
                &url,
                Some(WebSocketConfig::default()
                    .read_buffer_size(4 * 1024)
                    .max_message_size(Some(MAX_MESSAGE_BYTES))
                    .max_frame_size(Some(MAX_MESSAGE_BYTES))),
                false,
            ) => result,
            _ = shutdown.changed() => return,
        };
        let mut socket = match connected {
            Ok((socket, _)) => socket,
            Err(error) => {
                eprintln!(
                    "salmon-watch-2: server {} connection failed: {error}",
                    server.id
                );
                let _ = events
                    .send(Event::Disconnected {
                        server_id: server.id.clone(),
                        at: unix_now(),
                        error: error.to_string(),
                    })
                    .await;
                reconnect_seconds = (reconnect_seconds + 1).min(10);
                continue;
            }
        };
        reconnect_seconds = 0;
        if events
            .send(Event::Connected {
                server_id: server.id.clone(),
                at: unix_now(),
            })
            .await
            .is_err()
        {
            return;
        }

        let disconnect_error = loop {
            let next = tokio::select! {
                result = timeout(READ_TIMEOUT, socket.next()) => result,
                _ = shutdown.changed() => return,
            };
            let message = match next {
                Err(_) => break "no server data received for 30 seconds".to_owned(),
                Ok(None) => break "WebSocket connection closed".to_owned(),
                Ok(Some(Err(error))) => break error.to_string(),
                Ok(Some(Ok(message))) => message,
            };
            match message {
                Message::Binary(bytes) if bytes.as_ref() == [0] => {
                    if events
                        .send(Event::Heartbeat {
                            server_id: server.id.clone(),
                            at: unix_now(),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Message::Text(text) => {
                    if text.len() > MAX_MESSAGE_BYTES {
                        break "server message exceeds 1 MiB".to_owned();
                    }
                    match super::decode_text_message(text.as_ref()) {
                        Ok(Some(data)) => {
                            if events
                                .send(Event::Notification {
                                    server_id: server.id.clone(),
                                    data,
                                    at: unix_now(),
                                })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => break format!("{error:#}"),
                    }
                }
                Message::Binary(bytes) => {
                    let text = match std::str::from_utf8(&bytes) {
                        Ok(text) => text,
                        Err(error) => break format!("decoding server message: {error}"),
                    };
                    match super::decode_text_message(text) {
                        Ok(Some(data)) => {
                            if events
                                .send(Event::Notification {
                                    server_id: server.id.clone(),
                                    data,
                                    at: unix_now(),
                                })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => break format!("{error:#}"),
                    }
                }
                Message::Close(_) => break "WebSocket connection closed".to_owned(),
                _ => {}
            }
        };
        eprintln!(
            "salmon-watch-2: server {} connection lost: {disconnect_error}",
            server.id
        );
        let _ = events
            .send(Event::Disconnected {
                server_id: server.id.clone(),
                at: unix_now(),
                error: disconnect_error,
            })
            .await;
        reconnect_seconds = (reconnect_seconds + 1).min(10);
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
