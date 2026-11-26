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

    pub fn from_sfu_url(url: &str) -> anyhow::Result<Self> {
        let authority = url
            .strip_prefix("wss://")
            .and_then(|rest| rest.strip_suffix("/v1/sfu"))
            .ok_or_else(|| anyhow::anyhow!("expected a WSS URL for /v1/sfu"))?;
        Self::parse(authority)
    }

    pub fn sfu_url(&self) -> String {
        format!("wss://{}/v1/sfu", self.authority())
    }

    pub fn verified_sfu_url(&self, url: &str) -> anyhow::Result<String> {
        anyhow::ensure!(
            &Self::from_sfu_url(url)? == self,
            "SFU URL is outside the dev server origin"
        );
        Ok(self.sfu_url())
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
