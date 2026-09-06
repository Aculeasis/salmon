use std::time::Duration;

use futures_util::SinkExt;
use salmon_watch_2::config::ServerConfig;
use salmon_watch_2::domain::Event;
use salmon_watch_2::network::run_client;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test(flavor = "current_thread")]
async fn plain_client_reports_connection_snapshot_and_heartbeat() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket.send(Message::Text(r#"{"event":"OngoingIncidentsSnapshot","data":{"time":"2026-09-07T10:00:00Z","ongoingIncidents":{"total":[{"key":"disk","state":"error","details":"full","incidentStartedAt":"2026-09-07T09:00:00Z"}],"added":[],"removed":[],"updated":[],"numItemsOK":4}}}"#.into())).await.unwrap();
        socket.send(Message::Binary(vec![0].into())).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let (events_tx, mut events_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let client = tokio::spawn(run_client(
        ServerConfig {
            id: "local".into(),
            addr: address.to_string(),
        },
        events_tx,
        shutdown_rx,
    ));

    assert!(
        matches!(next(&mut events_rx).await, Event::Connected { server_id, .. } if server_id == "local")
    );
    assert!(
        matches!(next(&mut events_rx).await, Event::Notification { server_id, data, .. } if server_id == "local" && data.total[0].key == "disk")
    );
    assert!(
        matches!(next(&mut events_rx).await, Event::Heartbeat { server_id, .. } if server_id == "local")
    );

    let _ = shutdown_tx.send(true);
    client.await.unwrap();
    server.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_snapshot_reports_the_full_decode_error_chain() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                r#"{"event":"OngoingIncidentsSnapshot","data":{"ongoingIncidents":{"total":"not-an-array"}}}"#
                    .into(),
            ))
            .await
            .unwrap();
    });

    let (events_tx, mut events_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let client = tokio::spawn(run_client(
        ServerConfig {
            id: "local".into(),
            addr: address.to_string(),
        },
        events_tx,
        shutdown_rx,
    ));

    assert!(matches!(
        next(&mut events_rx).await,
        Event::Connected { .. }
    ));
    let Event::Disconnected { error, .. } = next(&mut events_rx).await else {
        panic!("expected the malformed snapshot to disconnect the client");
    };
    assert!(error.contains("decoding OngoingIncidentsSnapshot data"));
    assert!(error.contains("invalid type: string \"not-an-array\", expected a sequence"));

    let _ = shutdown_tx.send(true);
    client.await.unwrap();
    server.await.unwrap();
}

async fn next(events: &mut mpsc::Receiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .expect("timed out waiting for client event")
        .expect("client event channel closed")
}
