use std::collections::HashSet;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    pub ws_client: WsClientConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WsClientConfig {
    pub servers: Vec<ServerConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub id: String,
    pub addr: String,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub tunnel: Option<TunnelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TlsConfig {
    #[serde(default)]
    pub ca_file: String,
    #[serde(default)]
    pub server_name: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AuthConfig {
    pub bearer_token_file: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelConfig {
    pub ssh: SshTunnelConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SshTunnelConfig {
    pub host: String,
    pub user: String,
    #[serde(default)]
    pub port: u16,
    pub remote_salmon_addr: String,
    #[serde(default)]
    pub extra_ssh_args: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let data =
            fs::read(path).with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self = serde_yaml::from_slice(&data)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let mut ids = HashSet::new();
        for (index, server) in self.ws_client.servers.iter().enumerate() {
            validate_server_id(&server.id)
                .with_context(|| format!("wsClient.servers[{index}].id"))?;
            if !ids.insert(&server.id) {
                bail!("wsClient.servers[{index}].id {:?} is duplicated", server.id);
            }
            if server.addr.is_empty() {
                bail!("wsClient.servers[{index}].addr is required");
            }
            if server.addr.contains("//") || server.addr.contains('/') {
                bail!("wsClient.servers[{index}].addr must be a host:port address, not a URL");
            }
            validate_host_port(&server.addr)
                .with_context(|| format!("wsClient.servers[{index}].addr"))?;
            if server
                .auth
                .as_ref()
                .is_some_and(|auth| auth.bearer_token_file.is_empty())
            {
                bail!("wsClient.servers[{index}].auth.bearerTokenFile is required");
            }
            if let Some(tunnel) = &server.tunnel {
                validate_ssh_tunnel(server, &tunnel.ssh, index)?;
            }
        }
        Ok(())
    }
}

pub fn validate_server_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("is required");
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        bail!("{id:?} must contain only letters, digits, underscores, or hyphens");
    }
    if id == "internal" {
        bail!("{id:?} is reserved");
    }
    Ok(())
}

fn validate_ssh_tunnel(server: &ServerConfig, ssh: &SshTunnelConfig, index: usize) -> Result<()> {
    let prefix = format!("wsClient.servers[{index}].tunnel.ssh");
    if ssh.host.is_empty() {
        bail!("{prefix}.host is required");
    }
    if ssh.host.starts_with('-') {
        bail!("{prefix}.host must not start with a hyphen");
    }
    if ssh.user.is_empty() {
        bail!("{prefix}.user is required");
    }
    if ssh.user.starts_with('-') {
        bail!("{prefix}.user must not start with a hyphen");
    }
    validate_host_port(&ssh.remote_salmon_addr)
        .with_context(|| format!("{prefix}.remoteSalmonAddr"))?;
    let (local_host, _) = validate_host_port(&server.addr)
        .with_context(|| format!("wsClient.servers[{index}].addr for an SSH tunnel"))?;
    let loopback = local_host == "localhost"
        || local_host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !loopback {
        bail!("wsClient.servers[{index}].addr must use a loopback host for an SSH tunnel");
    }
    Ok(())
}

fn validate_host_port(address: &str) -> Result<(&str, u16)> {
    let (host, port) = if let Some(rest) = address.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .context("must be a valid host:port address")?;
        (host, port)
    } else {
        let (host, port) = address
            .rsplit_once(':')
            .context("must be a valid host:port address")?;
        if host.contains(':') {
            bail!("IPv6 addresses must be enclosed in brackets");
        }
        (host, port)
    };
    if host.is_empty() {
        bail!("host must not be empty");
    }
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .context("port must be between 1 and 65535")?;
    Ok((host, port))
}

pub fn default_path() -> Result<PathBuf> {
    let directory =
        dirs::config_dir().context("could not determine the user configuration directory")?;
    Ok(directory.join("salmon-watch").join("salmon-watch.yml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<Config> {
        let config: Config = serde_yaml::from_str(yaml)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn accepts_plain_servers() {
        let config =
            parse("wsClient:\n  servers:\n    - id: local\n      addr: localhost:41990\n").unwrap();
        assert_eq!(config.ws_client.servers[0].id, "local");
        assert!(config.ws_client.servers[0].tunnel.is_none());
        assert!(config.ws_client.servers[0].tls.is_none());
        assert!(config.ws_client.servers[0].auth.is_none());
    }

    #[test]
    fn accepts_tls_and_bearer_auth() {
        let config = parse(
            "wsClient:\n  servers:\n    - id: remote\n      addr: 127.0.0.1:41990\n      tls:\n        caFile: /etc/salmon/ca.pem\n        serverName: salmon.example.com\n      auth:\n        bearerTokenFile: /etc/salmon/remote.token\n",
        )
        .unwrap();
        let server = &config.ws_client.servers[0];
        let tls = server.tls.as_ref().unwrap();
        assert_eq!(tls.ca_file, "/etc/salmon/ca.pem");
        assert_eq!(tls.server_name, "salmon.example.com");
        assert_eq!(
            server.auth.as_ref().unwrap().bearer_token_file,
            "/etc/salmon/remote.token"
        );

        let empty_tls = parse(
            "wsClient:\n  servers:\n    - id: remote\n      addr: salmon.example.com:41990\n      tls: {}\n",
        )
        .unwrap();
        assert!(empty_tls.ws_client.servers[0].tls.is_some());
    }

    #[test]
    fn accepts_ssh_tunnel() {
        let config = parse(
            "wsClient:\n  servers:\n    - id: remote\n      addr: 127.0.0.1:42990\n      tunnel:\n        ssh:\n          host: salmon.example.com\n          user: monitor\n          port: 2222\n          remoteSalmonAddr: 127.0.0.1:41990\n          extraSshArgs: ['-i', '/tmp/key']\n",
        )
        .unwrap();
        let ssh = &config.ws_client.servers[0].tunnel.as_ref().unwrap().ssh;
        assert_eq!(ssh.host, "salmon.example.com");
        assert_eq!(ssh.user, "monitor");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.remote_salmon_addr, "127.0.0.1:41990");
        assert_eq!(ssh.extra_ssh_args, ["-i", "/tmp/key"]);
    }

    #[test]
    fn rejects_unsupported_tunnel_options() {
        for extra in ["tunnel: {}", "tunnel: { customCommand: {} }"] {
            let yaml = format!(
                "wsClient:\n  servers:\n    - id: local\n      addr: localhost:41990\n      {extra}\n"
            );
            assert!(parse(&yaml).is_err(), "accepted {extra}");
        }
    }

    #[test]
    fn rejects_missing_bearer_token_file() {
        for auth in ["{}", "{ bearerTokenFile: '' }"] {
            let yaml = format!(
                "wsClient:\n  servers:\n    - id: remote\n      addr: localhost:41990\n      auth: {auth}\n"
            );
            assert!(parse(&yaml).is_err(), "accepted invalid auth: {auth}");
        }
    }

    #[test]
    fn rejects_invalid_ssh_tunnels() {
        for ssh in [
            "host: ''\n          user: user\n          remoteSalmonAddr: localhost:41990",
            "host: -option\n          user: user\n          remoteSalmonAddr: localhost:41990",
            "host: host\n          user: ''\n          remoteSalmonAddr: localhost:41990",
            "host: host\n          user: -option\n          remoteSalmonAddr: localhost:41990",
            "host: host\n          user: user\n          remoteSalmonAddr: missing-port",
            "host: host\n          user: user\n          remoteSalmonAddr: localhost:0",
        ] {
            let yaml = format!(
                "wsClient:\n  servers:\n    - id: remote\n      addr: localhost:42990\n      tunnel:\n        ssh:\n          {ssh}\n"
            );
            assert!(parse(&yaml).is_err(), "accepted invalid SSH config:\n{ssh}");
        }

        let non_loopback = "wsClient:\n  servers:\n    - id: remote\n      addr: example.com:42990\n      tunnel:\n        ssh:\n          host: host\n          user: user\n          remoteSalmonAddr: localhost:41990\n";
        assert!(parse(non_loopback).is_err());
    }

    #[test]
    fn rejects_invalid_duplicate_and_reserved_ids() {
        for yaml in [
            "wsClient:\n  servers:\n    - { id: 'bad.id', addr: localhost:1 }\n",
            "wsClient:\n  servers:\n    - { id: internal, addr: localhost:1 }\n",
            "wsClient:\n  servers:\n    - { id: same, addr: localhost:1 }\n    - { id: same, addr: localhost:2 }\n",
        ] {
            assert!(parse(yaml).is_err());
        }
    }
}
