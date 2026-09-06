use std::collections::HashSet;
use std::fs;
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
            if server.id.is_empty()
                || !server
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            {
                bail!(
                    "wsClient.servers[{index}].id {:?} must contain only letters, digits, underscores, or hyphens",
                    server.id
                );
            }
            if server.id == "internal" {
                bail!("wsClient.servers[{index}].id \"internal\" is reserved");
            }
            if !ids.insert(&server.id) {
                bail!("wsClient.servers[{index}].id {:?} is duplicated", server.id);
            }
            if server.addr.is_empty() {
                bail!("wsClient.servers[{index}].addr is required");
            }
            if server.addr.contains("//") || server.addr.contains('/') {
                bail!("wsClient.servers[{index}].addr must be a host:port address, not a URL");
            }
        }
        Ok(())
    }
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
    }

    #[test]
    fn rejects_security_and_tunnel_options_until_supported() {
        for extra in ["tls: {}", "auth: {}", "tunnel: {}"] {
            let yaml = format!(
                "wsClient:\n  servers:\n    - id: local\n      addr: localhost:41990\n      {extra}\n"
            );
            assert!(parse(&yaml).is_err(), "accepted {extra}");
        }
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
