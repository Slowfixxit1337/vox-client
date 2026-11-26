use anyhow::{Context, Result};
use rand::{TryRng, rngs::SysRng};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Write, path::Path};

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
    sfu_url: &'a str,
    credential_hashes: BTreeMap<&'a str, String>,
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [verb, flag, path] = args.as_slice() else {
        anyhow::bail!("usage: vox-lab init --out NEW_DIRECTORY")
    };
    anyhow::ensure!(
        verb == "init" && flag == "--out",
        "usage: vox-lab init --out NEW_DIRECTORY"
    );
    let path = Path::new(path);
    fs::create_dir(path).context("the output directory must be new")?;
    let mut credential_hashes = BTreeMap::new();
    for device in ["a", "b"] {
        let mut bytes = [0u8; 32];
        SysRng
            .try_fill_bytes(&mut bytes)
            .map_err(|e| anyhow::anyhow!("OS random source failed: {e}"))?;
        let credential = hex::encode(bytes);
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
            sfu_url: "wss://vox-lab.test:8444/v1/sfu",
            credential_hashes,
        },
    )?;
    Ok(())
}

fn write_profile(path: &Path, value: &impl Serialize) -> Result<()> {
    let text = toml::to_string(value)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(text.as_bytes())?;
    Ok(())
}
