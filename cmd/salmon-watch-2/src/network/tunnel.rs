use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::config::ServerConfig;
use crate::domain::Event;

use super::client::run_connection_loop;

const READY_MARKER: &str = "SALMON_TUNNEL_READY";
const MAX_FAILURE_OUTPUT_BYTES: usize = 1024;
const DEFAULT_RESTART_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub readiness_marker: String,
}

impl CommandSpec {
    pub fn for_server(server: &ServerConfig) -> Option<Self> {
        let ssh = &server.tunnel.as_ref()?.ssh;
        let port = if ssh.port == 0 { 22 } else { ssh.port };
        let mut args = vec![
            "-N".into(),
            "-T".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ExitOnForwardFailure=yes".into(),
            "-o".into(),
            "ConnectTimeout=15".into(),
            "-o".into(),
            "ServerAliveInterval=10".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
            "-o".into(),
            "PermitLocalCommand=yes".into(),
            "-o".into(),
            format!("LocalCommand=echo {READY_MARKER}"),
            "-p".into(),
            port.to_string(),
            "-L".into(),
            format!("{}:{}", server.addr, ssh.remote_salmon_addr),
        ];
        args.extend(ssh.extra_ssh_args.iter().cloned());
        args.push(format!("{}@{}", ssh.user, ssh.host));
        Some(Self {
            program: "ssh".into(),
            args,
            readiness_marker: READY_MARKER.into(),
        })
    }
}

pub(crate) async fn run(
    server: ServerConfig,
    events: mpsc::Sender<Event>,
    shutdown: watch::Receiver<bool>,
) {
    let spec = CommandSpec::for_server(&server).expect("tunneled server has an SSH command");
    run_with_spec(server, spec, events, shutdown, DEFAULT_RESTART_DELAY).await;
}

async fn run_with_spec(
    server: ServerConfig,
    spec: CommandSpec,
    events: mpsc::Sender<Event>,
    mut shutdown: watch::Receiver<bool>,
    restart_delay: Duration,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        log::info!(
            "server {} starting SSH tunnel with {}",
            server.id,
            spec.program
        );
        let mut child = match spawn(&spec) {
            Ok(child) => child,
            Err(error) => {
                let details = format!("Failed to start SSH tunnel command: {error}");
                log::error!("server {}: {details}", server.id);
                if !send_tunnel_failure(&events, &server.id, details).await
                    || !wait_to_restart(&mut shutdown, restart_delay).await
                {
                    return;
                }
                continue;
            }
        };

        let stdout = child.stdout.take().expect("SSH stdout is piped");
        let stderr = child.stderr.take().expect("SSH stderr is piped");
        let (ready_tx, mut ready_rx) = mpsc::channel(2);
        let stdout_task = tokio::spawn(capture_output(
            stdout,
            spec.readiness_marker.clone(),
            ready_tx.clone(),
        ));
        let stderr_task = tokio::spawn(capture_output(
            stderr,
            spec.readiness_marker.clone(),
            ready_tx,
        ));

        enum BeforeReady {
            Ready,
            Exited(io::Result<std::process::ExitStatus>),
            Shutdown,
        }
        let outcome = tokio::select! {
            marker = ready_rx.recv() => if marker.is_some() { BeforeReady::Ready } else {
                BeforeReady::Exited(child.wait().await)
            },
            status = child.wait() => BeforeReady::Exited(status),
            _ = shutdown.changed() => BeforeReady::Shutdown,
        };

        match outcome {
            BeforeReady::Ready => {
                log::info!("server {} SSH tunnel is ready", server.id);
                if events
                    .send(Event::TunnelReady {
                        server_id: server.id.clone(),
                        at: unix_now(),
                    })
                    .await
                    .is_err()
                {
                    stop_child(&mut child).await;
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    return;
                }
                let (stop_tx, stop_rx) = watch::channel(false);
                let mut connection = tokio::spawn(run_connection_loop(
                    server.clone(),
                    events.clone(),
                    stop_rx,
                    true,
                ));
                enum WhileReady {
                    Exited(io::Result<std::process::ExitStatus>),
                    ClientStopped,
                    Shutdown,
                }
                let outcome = tokio::select! {
                    status = child.wait() => WhileReady::Exited(status),
                    _ = &mut connection => WhileReady::ClientStopped,
                    _ = shutdown.changed() => WhileReady::Shutdown,
                };
                let _ = stop_tx.send(true);
                match outcome {
                    WhileReady::Exited(status) => {
                        let _ = connection.await;
                        let details = failure_details(
                            status,
                            stdout_task,
                            stderr_task,
                            &spec.readiness_marker,
                        )
                        .await;
                        log::error!("server {} SSH tunnel failed: {details}", server.id);
                        if !send_tunnel_failure(&events, &server.id, details).await
                            || !wait_to_restart(&mut shutdown, restart_delay).await
                        {
                            return;
                        }
                    }
                    WhileReady::ClientStopped => {
                        stop_child(&mut child).await;
                        let _ = stdout_task.await;
                        let _ = stderr_task.await;
                        return;
                    }
                    WhileReady::Shutdown => {
                        let _ = connection.await;
                        stop_child(&mut child).await;
                        let _ = stdout_task.await;
                        let _ = stderr_task.await;
                        return;
                    }
                }
            }
            BeforeReady::Exited(status) => {
                let details =
                    failure_details(status, stdout_task, stderr_task, &spec.readiness_marker).await;
                log::error!("server {} SSH tunnel failed: {details}", server.id);
                if !send_tunnel_failure(&events, &server.id, details).await
                    || !wait_to_restart(&mut shutdown, restart_delay).await
                {
                    return;
                }
            }
            BeforeReady::Shutdown => {
                stop_child(&mut child).await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return;
            }
        }
    }
}

fn spawn(spec: &CommandSpec) -> io::Result<Child> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    isolate_from_terminal_signals(&mut command);
    command.spawn()
}

#[cfg(unix)]
fn isolate_from_terminal_signals(command: &mut Command) {
    command.process_group(0);
}

#[cfg(windows)]
fn isolate_from_terminal_signals(command: &mut Command) {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn isolate_from_terminal_signals(_command: &mut Command) {}

async fn stop_child(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn wait_to_restart(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    log::info!("SSH tunnel will restart in {}s", delay.as_secs_f64());
    tokio::select! {
        _ = sleep(delay) => true,
        _ = shutdown.changed() => false,
    }
}

async fn send_tunnel_failure(events: &mpsc::Sender<Event>, server_id: &str, error: String) -> bool {
    events
        .send(Event::TunnelFailed {
            server_id: server_id.to_owned(),
            at: unix_now(),
            error,
        })
        .await
        .is_ok()
}

async fn failure_details(
    status: io::Result<std::process::ExitStatus>,
    stdout_task: JoinHandle<CapturedOutput>,
    stderr_task: JoinHandle<CapturedOutput>,
    readiness_marker: &str,
) -> String {
    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    let mut details = match status {
        Ok(status) if status.success() => "SSH tunnel command exited unexpectedly".to_owned(),
        Ok(status) => format!("SSH tunnel command exited: {status}"),
        Err(error) => format!("Waiting for SSH tunnel command failed: {error}"),
    };
    let output = failure_output(&stderr.text(), &stdout.text(), readiness_marker);
    if !output.is_empty() {
        details.push_str("\n\n");
        details.push_str(&output);
    }
    details
}

#[derive(Default)]
struct CapturedOutput {
    tail: TailBuffer,
}

impl CapturedOutput {
    fn text(&self) -> String {
        self.tail.text()
    }
}

async fn capture_output<R>(mut reader: R, marker: String, ready: mpsc::Sender<()>) -> CapturedOutput
where
    R: AsyncRead + Unpin,
{
    let mut output = CapturedOutput::default();
    let mut matcher = ProbeMatcher::new(marker.as_bytes());
    let mut buffer = [0; 256];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => {
                let chunk = &buffer[..count];
                output.tail.push(chunk);
                if matcher.push(chunk) {
                    let _ = ready.try_send(());
                }
            }
            Err(error) => {
                output
                    .tail
                    .push(format!("\nreading SSH output failed: {error}").as_bytes());
                break;
            }
        }
    }
    output
}

struct ProbeMatcher {
    probe: Vec<u8>,
    tail: Vec<u8>,
    complete: bool,
}

impl ProbeMatcher {
    fn new(probe: &[u8]) -> Self {
        Self {
            probe: probe.to_vec(),
            tail: Vec::new(),
            complete: probe.is_empty(),
        }
    }

    fn push(&mut self, bytes: &[u8]) -> bool {
        if self.complete {
            return false;
        }
        let mut combined = Vec::with_capacity(self.tail.len() + bytes.len());
        combined.extend_from_slice(&self.tail);
        combined.extend_from_slice(bytes);
        if combined
            .windows(self.probe.len())
            .any(|window| window == self.probe)
        {
            self.complete = true;
            self.tail.clear();
            return true;
        }
        let keep = (self.probe.len() - 1).min(combined.len());
        self.tail = combined[combined.len() - keep..].to_vec();
        false
    }
}

#[derive(Default)]
struct TailBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

impl TailBuffer {
    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= MAX_FAILURE_OUTPUT_BYTES {
            self.bytes = bytes[bytes.len() - MAX_FAILURE_OUTPUT_BYTES..].to_vec();
            self.truncated = true;
            return;
        }
        let overflow = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(MAX_FAILURE_OUTPUT_BYTES);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.truncated = true;
        }
        self.bytes.extend_from_slice(bytes);
    }

    fn text(&self) -> String {
        let text = String::from_utf8_lossy(&self.bytes).trim().to_owned();
        if self.truncated && !text.is_empty() {
            format!("…{text}")
        } else {
            text
        }
    }
}

fn failure_output(stderr: &str, stdout: &str, readiness_marker: &str) -> String {
    fn clean(output: &str, marker: &str) -> String {
        output
            .lines()
            .filter(|line| line.trim() != marker)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned()
    }
    let stderr = clean(stderr, readiness_marker);
    if stderr.is_empty() {
        clean(stdout, readiness_marker)
    } else {
        stderr
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
    use super::*;
    use crate::config::{SshTunnelConfig, TunnelConfig};
    use futures_util::StreamExt;
    use tokio::net::TcpListener;
    use tokio::time::timeout;

    fn plain_server(addr: String) -> ServerConfig {
        ServerConfig {
            id: "remote".into(),
            addr,
            tunnel: None,
        }
    }

    #[test]
    fn builds_the_same_ssh_forward_as_the_old_client() {
        let server = ServerConfig {
            id: "remote".into(),
            addr: "127.0.0.1:41992".into(),
            tunnel: Some(TunnelConfig {
                ssh: SshTunnelConfig {
                    host: "salmon.example.com".into(),
                    user: "monitor".into(),
                    port: 2222,
                    remote_salmon_addr: "127.0.0.1:41990".into(),
                    extra_ssh_args: vec![
                        "-i".into(),
                        "/etc/salmon-watch/key".into(),
                        "-J".into(),
                        "bastion.example.com".into(),
                    ],
                },
            }),
        };
        let command = CommandSpec::for_server(&server).unwrap();
        assert_eq!(command.program, "ssh");
        assert_eq!(
            command.args,
            [
                "-N",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ConnectTimeout=15",
                "-o",
                "ServerAliveInterval=10",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "PermitLocalCommand=yes",
                "-o",
                "LocalCommand=echo SALMON_TUNNEL_READY",
                "-p",
                "2222",
                "-L",
                "127.0.0.1:41992:127.0.0.1:41990",
                "-i",
                "/etc/salmon-watch/key",
                "-J",
                "bastion.example.com",
                "monitor@salmon.example.com",
            ]
        );
        assert_eq!(command.readiness_marker, READY_MARKER);
    }

    #[test]
    fn defaults_the_ssh_port_to_22() {
        let server = ServerConfig {
            id: "remote".into(),
            addr: "localhost:41992".into(),
            tunnel: Some(TunnelConfig {
                ssh: SshTunnelConfig {
                    host: "host".into(),
                    user: "user".into(),
                    port: 0,
                    remote_salmon_addr: "localhost:41990".into(),
                    extra_ssh_args: Vec::new(),
                },
            }),
        };
        let command = CommandSpec::for_server(&server).unwrap();
        assert!(command.args.windows(2).any(|args| args == ["-p", "22"]));
    }

    #[test]
    fn readiness_probe_matches_across_output_chunks() {
        let mut matcher = ProbeMatcher::new(b"tunnel-ready");
        assert!(!matcher.push(b"ignored tun"));
        assert!(!matcher.push(b"nel-"));
        assert!(matcher.push(b"ready trailing"));
        assert!(!matcher.push(b"tunnel-ready"));
    }

    #[test]
    fn failure_output_prefers_stderr_and_removes_protocol_marker() {
        assert_eq!(
            failure_output(
                "SALMON_TUNNEL_READY\nssh failed",
                "less useful",
                READY_MARKER
            ),
            "ssh failed"
        );
        assert_eq!(
            failure_output("", "SALMON_TUNNEL_READY\nstdout failed", READY_MARKER),
            "stdout failed"
        );
    }

    #[test]
    fn failure_output_tail_is_bounded() {
        let mut tail = TailBuffer::default();
        tail.push(&vec![b'x'; MAX_FAILURE_OUTPUT_BYTES]);
        tail.push(b"useful ending");
        let text = tail.text();
        assert!(text.starts_with('…'));
        assert!(text.ends_with("useful ending"));
        assert!(text.len() <= MAX_FAILURE_OUTPUT_BYTES + '…'.len_utf8());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn websocket_dial_waits_for_marker_from_stderr() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = plain_server(listener.local_addr().unwrap().to_string());
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                format!("sleep 0.15; echo {READY_MARKER} >&2; exec sleep 30"),
            ],
            readiness_marker: READY_MARKER.into(),
        };
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_with_spec(
            server,
            spec,
            events_tx,
            shutdown_rx,
            Duration::from_secs(60),
        ));

        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "WebSocket TCP connection was attempted before tunnel readiness"
        );
        let first_event = timeout(Duration::from_secs(3), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first_event, Event::TunnelReady { .. }));
        timeout(Duration::from_secs(3), listener.accept())
            .await
            .expect("WebSocket TCP connection was not attempted after readiness")
            .unwrap();

        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(3), task)
            .await
            .expect("tunnel task did not stop")
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn failed_tunnel_reports_output_then_restarts_and_becomes_ready() {
        let directory = tempfile::tempdir().unwrap();
        let attempt = directory.path().join("first-attempt");
        let server = plain_server("127.0.0.1:9".into());
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "if [ -e \"$1\" ]; then echo {READY_MARKER}; exec sleep 30; fi; : > \"$1\"; echo useful-ssh-error >&2; exit 7"
                ),
                "sh".into(),
                attempt.to_string_lossy().into_owned(),
            ],
            readiness_marker: READY_MARKER.into(),
        };
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_with_spec(
            server,
            spec,
            events_tx,
            shutdown_rx,
            Duration::from_millis(10),
        ));

        let failure = timeout(Duration::from_secs(3), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            failure,
            Event::TunnelFailed { error, .. } if error.contains("useful-ssh-error")
        ));
        let ready = timeout(Duration::from_secs(3), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ready, Event::TunnelReady { .. }));

        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(3), task)
            .await
            .expect("tunnel task did not stop")
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn tunnel_death_closes_an_active_websocket_and_reports_only_tunnel_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = plain_server(listener.local_addr().unwrap().to_string());
        let websocket_server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.next().await
        });

        let directory = tempfile::tempdir().unwrap();
        let stop = directory.path().join("stop-tunnel");
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "echo {READY_MARKER}; while [ ! -e \"$1\" ]; do sleep 0.01; done; echo tunnel-process-died >&2; exit 7"
                ),
                "sh".into(),
                stop.to_string_lossy().into_owned(),
            ],
            readiness_marker: READY_MARKER.into(),
        };
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_with_spec(
            server,
            spec,
            events_tx,
            shutdown_rx,
            Duration::from_secs(60),
        ));

        let ready = timeout(Duration::from_secs(3), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ready, Event::TunnelReady { .. }));
        let connected = timeout(Duration::from_secs(3), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(connected, Event::Connected { .. }));

        std::fs::write(&stop, b"stop").unwrap();
        let failure = timeout(Duration::from_secs(3), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            failure,
            Event::TunnelFailed { error, .. } if error.contains("tunnel-process-died")
        ));
        assert!(
            events_rx.try_recv().is_err(),
            "tunnel death emitted a redundant WebSocket disconnect"
        );
        timeout(Duration::from_secs(3), websocket_server)
            .await
            .expect("server did not observe the WebSocket closing")
            .unwrap();

        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(3), task)
            .await
            .expect("tunnel task did not stop")
            .unwrap();
    }
}
