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
    let (path, sfu_url) = match args.as_slice() {
        [verb, flag, path] if verb == "init" && flag == "--out" => {
            (path, "wss://vox-lab.test:8443/v1/sfu")
        }
        [verb, flag, path, option, url]
            if verb == "init" && flag == "--out" && option == "--sfu-url" =>
        {
            (path, url.as_str())
        }
        _ => anyhow::bail!("usage: vox-lab init --out NEW_DIRECTORY [--sfu-url WSS_URL]"),
    };
    let sfu_url = DevAddress::from_sfu_url(sfu_url)?.sfu_url();
    let path = Path::new(path);
    fs::create_dir(path).context("the output directory must be new")?;
    let mut credential_hashes = BTreeMap::new();
    for device in ["a", "b"] {
        let credential = random_secret()?;
        credential_hashes.insert(device, hex::encode(Sha256::digest(credential.as_bytes())));
        write_profile(
            &path.join(format!("client-{device}.toml")),
            &ClientProfile {
                room: "media",
                device,
                credential,
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
