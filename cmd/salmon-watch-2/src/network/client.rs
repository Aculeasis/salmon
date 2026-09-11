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
const RECONNECT_STEP: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(10);

#[derive(Clone, Copy)]
struct ClientOptions {
    max_message_bytes: usize,
    read_timeout: Duration,
    reconnect_step: Duration,
    max_reconnect_delay: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            max_message_bytes: MAX_MESSAGE_BYTES,
            read_timeout: READ_TIMEOUT,
            reconnect_step: RECONNECT_STEP,
            max_reconnect_delay: MAX_RECONNECT_DELAY,
        }
    }
}

pub async fn run_client(
    server: ServerConfig,
    events: mpsc::Sender<Event>,
    shutdown: watch::Receiver<bool>,
) {
    if server.tunnel.is_some() {
        super::tunnel::run(server, events, shutdown).await;
    } else {
        run_connection_loop(server, events, shutdown, false).await;
    }
}

pub(super) async fn run_connection_loop(
    server: ServerConfig,
    events: mpsc::Sender<Event>,
    shutdown: watch::Receiver<bool>,
    tunneled: bool,
) {
    run_connection_loop_with_options(server, events, shutdown, tunneled, ClientOptions::default())
        .await;
}

async fn run_connection_loop_with_options(
    server: ServerConfig,
    events: mpsc::Sender<Event>,
    mut shutdown: watch::Receiver<bool>,
    tunneled: bool,
    options: ClientOptions,
) {
    let mut reconnect_delay = Duration::ZERO;
    loop {
        if !reconnect_delay.is_zero() {
            tokio::select! {
                _ = sleep(reconnect_delay) => {}
                _ = shutdown.changed() => return,
            }
        }
        let url = format!("ws://{}/api/v1/wsconnect", server.addr);
        let connected = tokio::select! {
            result = connect_async_with_config(
                &url,
                Some(WebSocketConfig::default()
                    .read_buffer_size(4 * 1024)
                    .max_message_size(Some(options.max_message_bytes))
                    .max_frame_size(Some(options.max_message_bytes))),
                false,
            ) => result,
            _ = shutdown.changed() => return,
        };
        let mut socket = match connected {
            Ok((socket, _)) => socket,
            Err(error) => {
                if tunneled && tunnel_stopped(&mut shutdown).await {
                    return;
                }
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
                reconnect_delay =
                    (reconnect_delay + options.reconnect_step).min(options.max_reconnect_delay);
                continue;
            }
        };
        reconnect_delay = Duration::ZERO;
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
                result = timeout(options.read_timeout, socket.next()) => result,
                _ = shutdown.changed() => return,
            };
            let message = match next {
                Err(_) => {
                    break format!(
                        "no server data received for {} seconds",
                        options.read_timeout.as_secs()
                    );
                }
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
                    if text.len() > options.max_message_bytes {
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
        if tunneled && tunnel_stopped(&mut shutdown).await {
            return;
        }
        let _ = events
            .send(Event::Disconnected {
                server_id: server.id.clone(),
                at: unix_now(),
                error: disconnect_error,
            })
            .await;
        reconnect_delay =
            (reconnect_delay + options.reconnect_step).min(options.max_reconnect_delay);
    }
}

async fn tunnel_stopped(shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = sleep(Duration::from_millis(50)) => *shutdown.borrow(),
        _ = shutdown.changed() => true,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use futures_util::SinkExt;
    use tokio::net::TcpListener;
    use tokio::time::timeout;
    use tokio_tungstenite::accept_async;

    use super::*;

    fn server(address: String) -> ServerConfig {
        ServerConfig {
            id: "test".into(),
            addr: address,
            tunnel: None,
        }
    }

    fn fast_options() -> ClientOptions {
        ClientOptions {
            max_message_bytes: MAX_MESSAGE_BYTES,
            read_timeout: Duration::from_secs(2),
            reconnect_step: Duration::from_millis(10),
            max_reconnect_delay: Duration::from_millis(20),
        }
    }

    async fn next(events: &mut mpsc::Receiver<Event>) -> Event {
        timeout(Duration::from_secs(3), events.recv())
            .await
            .expect("timed out waiting for client event")
            .expect("client event channel closed")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconnects_after_the_server_closes_the_first_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut first = accept_async(stream).await.unwrap();
            first.close(None).await.unwrap();

            let (stream, _) = listener.accept().await.unwrap();
            let mut second = accept_async(stream).await.unwrap();
            second.send(Message::Binary(vec![0].into())).await.unwrap();
            let _ = second.next().await;
        });
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let client = tokio::spawn(run_connection_loop_with_options(
            server(address),
            events_tx,
            shutdown_rx,
            false,
            fast_options(),
        ));

        assert!(matches!(
            next(&mut events_rx).await,
            Event::Connected { .. }
        ));
        assert!(matches!(
            next(&mut events_rx).await,
            Event::Disconnected { .. }
        ));
        assert!(matches!(
            next(&mut events_rx).await,
            Event::Connected { .. }
        ));
        assert!(matches!(
            next(&mut events_rx).await,
            Event::Heartbeat { .. }
        ));

        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(3), client)
            .await
            .expect("client did not stop")
            .unwrap();
        timeout(Duration::from_secs(3), server_task)
            .await
            .expect("server did not observe client shutdown")
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_connection_hits_the_read_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let _ = socket.next().await;
        });
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut options = fast_options();
        options.read_timeout = Duration::from_millis(30);
        let client = tokio::spawn(run_connection_loop_with_options(
            server(address),
            events_tx,
            shutdown_rx,
            false,
            options,
        ));

        assert!(matches!(
            next(&mut events_rx).await,
            Event::Connected { .. }
        ));
        let Event::Disconnected { error, .. } = next(&mut events_rx).await else {
            panic!("expected read timeout to disconnect the client");
        };
        assert!(error.starts_with("no server data received"));

        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(3), client)
            .await
            .expect("client did not stop")
            .unwrap();
        timeout(Duration::from_secs(3), server_task)
            .await
            .expect("server did not observe timeout disconnect")
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_while_connected_is_quiet_and_prompt() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let _ = socket.next().await;
        });
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let client = tokio::spawn(run_connection_loop_with_options(
            server(address),
            events_tx,
            shutdown_rx,
            false,
            fast_options(),
        ));

        assert!(matches!(
            next(&mut events_rx).await,
            Event::Connected { .. }
        ));
        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_millis(250), client)
            .await
            .expect("client did not stop promptly")
            .unwrap();
        assert!(
            events_rx.try_recv().is_err(),
            "normal shutdown emitted a disconnect event"
        );
        timeout(Duration::from_secs(3), server_task)
            .await
            .expect("server did not observe client shutdown")
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_message_disconnects_the_client() {
        const LIMIT: usize = 64;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            socket
                .send(Message::Text("x".repeat(LIMIT + 1).into()))
                .await
                .unwrap();
        });
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut options = fast_options();
        options.max_message_bytes = LIMIT;
        let client = tokio::spawn(run_connection_loop_with_options(
            server(address),
            events_tx,
            shutdown_rx,
            false,
            options,
        ));

        assert!(matches!(
            next(&mut events_rx).await,
            Event::Connected { .. }
        ));
        let Event::Disconnected { error, .. } = next(&mut events_rx).await else {
            panic!("expected oversized message to disconnect the client");
        };
        let error = error.to_ascii_lowercase();
        assert!(
            error.contains("too long")
                || error.contains("capacity")
                || error.contains("size limit"),
            "unexpected oversized-message error: {error}"
        );

        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(3), client)
            .await
            .expect("client did not stop")
            .unwrap();
        server_task.await.unwrap();
    }
}
