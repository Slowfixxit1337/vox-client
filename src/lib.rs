//! Vox desktop client contracts.

use sha2::{Digest, Sha256};
use std::{net::Ipv6Addr, str::FromStr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevAddress(String);

impl DevAddress {
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        let (host, port) = if let Some(bracketed) = input.strip_prefix('[') {
            let (host, suffix) = bracketed
                .split_once(']')
                .ok_or_else(|| anyhow::anyhow!("IPv6 must end with ]"))?;
            let host = Ipv6Addr::from_str(host)?;
            let port = if suffix.is_empty() {
                None
            } else {
                Some(
                    suffix
                        .strip_prefix(':')
                        .ok_or_else(|| anyhow::anyhow!("invalid IPv6 port"))?,
                )
            };
            (format!("[{host}]"), port)
        } else {
            let (host, port) = input
                .split_once(':')
                .map_or((input, None), |(h, p)| (h, Some(p)));
            anyhow::ensure!(
                host.len() <= 253 && host.split('.').all(valid_label),
                "expected a DNS name, IPv4, or bracketed IPv6"
            );
            (host.to_ascii_lowercase(), port)
        };
        let port = match port {
            None => 443,
            Some(port) => {
                anyhow::ensure!(
                    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
                    "invalid port"
                );
                let port = port.parse::<u16>()?;
                anyhow::ensure!(port != 0, "port must be non-zero");
                port
            }
        };
        Ok(Self(format!("{host}:{port}")))
    }

    pub fn authority(&self) -> &str {
        &self.0
    }

    pub fn profile_key(&self) -> String {
        hex::encode(Sha256::digest(self.0.as_bytes()))
    }
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}
