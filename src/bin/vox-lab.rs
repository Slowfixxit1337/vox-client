use anyhow::{Context, Result};
use rand::{TryRng, rngs::SysRng};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Write, path::Path};
use vox_client::DevAddress;

#[derive(Serialize)]
struct ClientProfile<'a> {
    room: &'a str,
    device: &'a str,
    credential: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<MediaKeys<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    video: Option<MediaKeys<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    share_audio: Option<MediaKeys<'a>>,
}

#[derive(Serialize)]
struct MediaKeys<'a> {
    send_key: &'a str,
    receive_key: &'a str,
}

#[derive(Serialize)]
struct AdmissionFile<'a> {
    room: &'a str,
    generation: u64,
    sfu_url: String,
    credential_hashes: BTreeMap<&'a str, String>,
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [verb, flag, path, rest @ ..] = args.as_slice() else {
        anyhow::bail!(
            "usage: vox-lab init --out NEW_DIRECTORY [--sfu-url WSS_URL] [--voice] [--video]"
        );
    };
    anyhow::ensure!(
        verb == "init" && flag == "--out",
        "expected init --out NEW_DIRECTORY"
    );
    let mut options = rest;
    let mut sfu_url = None;
    let mut voice = false;
    let mut video = false;
    while !options.is_empty() {
        options = match options {
            [flag, value, tail @ ..] if flag == "--sfu-url" && sfu_url.is_none() => {
                sfu_url = Some(value.as_str());
                tail
            }
            [flag, tail @ ..] if flag == "--voice" && !voice => {
                voice = true;
                tail
            }
            [flag, tail @ ..] if flag == "--video" && !video => {
                video = true;
                tail
            }
            _ => anyhow::bail!("expected [--sfu-url WSS_URL] [--voice] [--video]"),
        };
    }
    let sfu_url =
        DevAddress::from_sfu_url(sfu_url.unwrap_or("wss://vox-lab.test:8443/v1/sfu"))?.sfu_url();
    let path = Path::new(path);
    fs::create_dir(path).context("the output directory must be new")?;
    let voice_keys = if voice {
        Some([random_secret()?, random_secret()?])
    } else {
        None
    };
    let video_keys = if video {
        Some([random_secret()?, random_secret()?])
    } else {
        None
    };
    let share_audio_keys = if video {
        Some([random_secret()?, random_secret()?])
    } else {
        None
    };
    let mut credential_hashes = BTreeMap::new();
    for (local, device) in ["a", "b"].into_iter().enumerate() {
        let credential = random_secret()?;
        credential_hashes.insert(device, hex::encode(Sha256::digest(credential.as_bytes())));
        write_profile(
            &path.join(format!("client-{device}.toml")),
            &ClientProfile {
                room: "media",
                device,
                credential,
                voice: voice_keys.as_ref().map(|keys| MediaKeys {
                    send_key: &keys[local],
                    receive_key: &keys[1 - local],
                }),
                video: video_keys.as_ref().map(|keys| MediaKeys {
                    send_key: &keys[local],
                    receive_key: &keys[1 - local],
                }),
                share_audio: share_audio_keys.as_ref().map(|keys| MediaKeys {
                    send_key: &keys[local],
                    receive_key: &keys[1 - local],
                }),
            },
        )?;
    }
    write_profile(
        &path.join("admission.toml"),
        &AdmissionFile {
            room: "media",
            generation: 1,
            sfu_url,
            credential_hashes,
        },
    )?;
    write_new(&path.join("lab-sfu.key"), random_secret()?.as_bytes())?;
    Ok(())
}

fn random_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| anyhow::anyhow!("OS random source failed: {e}"))?;
    Ok(hex::encode(bytes))
}

fn write_profile(path: &Path, value: &impl Serialize) -> Result<()> {
    let text = toml::to_string(value)?;
    write_new(path, text.as_bytes())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)?;
    Ok(())
}
