use super::{Client, protect};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rand::{TryRng, rngs::SysRng};
use ring::signature::{Ed25519KeyPair, KeyPair};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
};
use vox_protocol::auth::{DeviceRecord, Operation, Outcome, device_enrollment_message};
use zeroize::Zeroizing;

const IDENTITY_SIZE: usize = 48;
const MAX_LOCAL_BLOB: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    Local,
    Unverified,
    Verified,
    Changed,
    Revoked,
    Missing,
}

pub struct DeviceEntry {
    pub id: [u8; 16],
    pub fingerprint: String,
    pub state: TrustState,
}

pub struct DeviceSnapshot {
    pub entries: Vec<DeviceEntry>,
}

struct DeviceStore {
    base: PathBuf,
    entropy: Vec<u8>,
}

struct Identity {
    id: [u8; 16],
    key: Ed25519KeyPair,
}

impl Client {
    pub async fn register_device(&self, username: &str, session: &str) -> Result<()> {
        let identity = self.device_store(username)?.identity()?;
        let public_key = identity.public_key();
        let message = Zeroizing::new(device_enrollment_message(
            username,
            session,
            &identity.id,
            &public_key,
        ));
        let signature = identity.key.sign(&message).as_ref().to_vec();
        match self
            .call(Operation::EnrollDevice {
                session: session.into(),
                device_id: identity.id,
                public_key,
                signature,
            })
            .await?
        {
            Outcome::DeviceEnrolled => Ok(()),
            Outcome::Rejected { code, .. } => bail!("Vox rejected device enrollment: {code:?}"),
            _ => bail!("Unexpected Vox enrollment response"),
        }
    }

    pub async fn device_catalog(&self, username: &str, session: &str) -> Result<DeviceSnapshot> {
        self.register_device(username, session).await?;
        let records = match self
            .call(Operation::ListDevices {
                session: session.into(),
            })
            .await?
        {
            Outcome::DeviceDirectory { devices } => devices,
            Outcome::Rejected { code, .. } => bail!("Vox rejected the device directory: {code:?}"),
            _ => bail!("Unexpected Vox directory response"),
        };
        let store = self.device_store(username)?;
        store.snapshot(username, &store.identity()?, records)
    }

    pub async fn verify_device(
        &self,
        username: &str,
        session: &str,
        id: [u8; 16],
        supplied: &str,
    ) -> Result<DeviceSnapshot> {
        let mut snapshot = self.device_catalog(username, session).await?;
        let entry = snapshot
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .context("Device disappeared from the directory")?;
        ensure!(
            matches!(entry.state, TrustState::Unverified | TrustState::Verified),
            "Device key changed or the device is unavailable"
        );
        let expected = normalize_fingerprint(&entry.fingerprint)?;
        ensure!(
            normalize_fingerprint(supplied)? == expected,
            "Fingerprint differs from the selected device"
        );
        self.device_store(username)?.pin(id, &expected)?;
        entry.state = TrustState::Verified;
        Ok(snapshot)
    }

    pub async fn revoke_device(
        &self,
        username: &str,
        session: &str,
        id: [u8; 16],
    ) -> Result<DeviceSnapshot> {
        ensure!(
            self.device_store(username)?.identity()?.id != id,
            "Use another device to revoke this PC"
        );
        match self
            .call(Operation::RevokeDevice {
                session: session.into(),
                device_id: id,
            })
            .await?
        {
            Outcome::DeviceRevoked => self.device_catalog(username, session).await,
            Outcome::Rejected { code, .. } => bail!("Vox rejected device revocation: {code:?}"),
            _ => bail!("Unexpected Vox revocation response"),
        }
    }

    fn device_store(&self, username: &str) -> Result<DeviceStore> {
        let mut entropy = b"vox/windows/account-devices/v1\0".to_vec();
        entropy.extend_from_slice(self.address.authority().as_bytes());
        entropy.push(0);
        entropy.extend_from_slice(username.as_bytes());
        let base = self
            .session
            .parent()
            .context("Vox profile directory is missing")?
            .join("devices")
            .join(hex::encode(Sha256::digest(&entropy)));
        Ok(DeviceStore { base, entropy })
    }
}

impl Identity {
    fn public_key(&self) -> [u8; 32] {
        self.key
            .public_key()
            .as_ref()
            .try_into()
            .expect("Ed25519 has a 32-byte public key")
    }
}

impl DeviceStore {
    fn identity(&self) -> Result<Identity> {
        let path = self.base.join("identity.dpapi");
        if self.read(&path, b"identity")?.is_none() {
            let mut fresh = Zeroizing::new([0; IDENTITY_SIZE]);
            SysRng
                .try_fill_bytes(fresh.as_mut())
                .map_err(|error| anyhow!("Windows random source failed: {error}"))?;
            self.create(&path, b"identity", fresh.as_slice())?;
        }
        let secret = self
            .read(&path, b"identity")?
            .context("Vox device identity disappeared")?;
        ensure!(secret.len() == IDENTITY_SIZE, "Invalid Vox device identity");
        let key = Ed25519KeyPair::from_seed_unchecked(&secret[16..])
            .map_err(|_| anyhow!("Invalid Vox device signing key"))?;
        Ok(Identity {
            id: secret[..16].try_into()?,
            key,
        })
    }

    fn snapshot(
        &self,
        username: &str,
        identity: &Identity,
        devices: Vec<DeviceRecord>,
    ) -> Result<DeviceSnapshot> {
        ensure!(
            devices.len() <= 24,
            "Vox device directory exceeds its limit"
        );
        let mut entries = Vec::with_capacity(devices.len());
        let mut seen = HashSet::new();
        for record in devices {
            ensure!(
                seen.insert(record.device_id),
                "Duplicate device in Vox directory"
            );
            let digest = fingerprint(username, &record.device_id, &record.public_key);
            let state = if record.revoked {
                TrustState::Revoked
            } else if record.device_id == identity.id {
                if record.public_key == identity.public_key() {
                    TrustState::Local
                } else {
                    TrustState::Changed
                }
            } else {
                match self.read_pin(record.device_id)? {
                    Some(previous) if previous == digest => TrustState::Verified,
                    Some(_) => TrustState::Changed,
                    None => TrustState::Unverified,
                }
            };
            entries.push(DeviceEntry {
                id: record.device_id,
                fingerprint: display_fingerprint(&digest),
                state,
            });
        }
        let pins = self.base.join("pins");
        match fs::read_dir(pins) {
            Ok(files) => {
                for file in files {
                    let file = file?;
                    let name = file.file_name();
                    let Some(id) = name
                        .to_str()
                        .and_then(|name| hex::decode(name).ok())
                        .and_then(|bytes| bytes.try_into().ok())
                    else {
                        continue;
                    };
                    if !seen.contains(&id)
                        && let Some(digest) = self.read_pin(id)?
                    {
                        entries.push(DeviceEntry {
                            id,
                            fingerprint: display_fingerprint(&digest),
                            state: TrustState::Missing,
                        });
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        entries.sort_by_key(|entry| (entry.id != identity.id, entry.id));
        Ok(DeviceSnapshot { entries })
    }

    fn pin(&self, id: [u8; 16], digest: &[u8; 32]) -> Result<()> {
        let mut purpose = b"fingerprint\0".to_vec();
        purpose.extend_from_slice(&id);
        self.create(
            &self.base.join("pins").join(hex::encode(id)),
            &purpose,
            digest,
        )?;
        ensure!(
            self.read_pin(id)? == Some(*digest),
            "A different fingerprint was previously verified"
        );
        Ok(())
    }

    fn read_pin(&self, id: [u8; 16]) -> Result<Option<[u8; 32]>> {
        let mut purpose = b"fingerprint\0".to_vec();
        purpose.extend_from_slice(&id);
        self.read(&self.base.join("pins").join(hex::encode(id)), &purpose)?
            .map(|digest| {
                digest
                    .as_slice()
                    .try_into()
                    .context("Invalid verified Vox fingerprint")
            })
            .transpose()
    }

    fn protection_entropy(&self, purpose: &[u8]) -> Vec<u8> {
        [&self.entropy[..], b"\0", purpose].concat()
    }

    fn read(&self, path: &Path, purpose: &[u8]) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut encrypted = Vec::new();
        file.take((MAX_LOCAL_BLOB + 1) as u64)
            .read_to_end(&mut encrypted)?;
        ensure!(
            encrypted.len() <= MAX_LOCAL_BLOB,
            "Vox device profile is too large"
        );
        let clear = protect(&encrypted, &self.protection_entropy(purpose), false)?;
        Ok(Some(Zeroizing::new(clear.bytes()?.to_vec())))
    }

    fn create(&self, path: &Path, purpose: &[u8], bytes: &[u8]) -> Result<()> {
        let directory = path.parent().context("Vox device folder is missing")?;
        fs::create_dir_all(directory)?;
        let encrypted = protect(bytes, &self.protection_entropy(purpose), true)?;
        let mut random = [0; 12];
        SysRng
            .try_fill_bytes(&mut random)
            .map_err(|error| anyhow!("Windows random source failed: {error}"))?;
        let temporary = directory.join(format!("{}.tmp", hex::encode(random)));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let written = file
            .write_all(encrypted.bytes()?)
            .and_then(|()| file.sync_all());
        drop(file);
        let installed = written.and_then(|()| fs::hard_link(&temporary, path));
        let _ = fs::remove_file(&temporary);
        match installed {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn fingerprint(username: &str, id: &[u8; 16], public_key: &[u8; 32]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"vox/device-fingerprint/v1\0");
    digest.update(username.as_bytes());
    digest.update([0]);
    digest.update(id);
    digest.update(public_key);
    digest.finalize().into()
}

fn display_fingerprint(digest: &[u8; 32]) -> String {
    hex::encode_upper(digest)
        .as_bytes()
        .as_chunks::<8>()
        .0
        .iter()
        .map(|part| std::str::from_utf8(part).expect("hex is ASCII"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_fingerprint(supplied: &str) -> Result<[u8; 32]> {
    ensure!(supplied.len() <= 128, "Enter the complete fingerprint");
    let normalized: String = supplied
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '-')
        .collect();
    let mut digest = [0; 32];
    hex::decode_to_slice(normalized, &mut digest)
        .context("Enter all 64 hexadecimal digits of the fingerprint")?;
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_pins_survive_restart_and_reject_key_substitution() -> Result<()> {
        let folder = tempfile::tempdir()?;
        let client = Client::from_options(&[
            "--server".into(),
            "localhost:8443".into(),
            "--profile-dir".into(),
            folder.path().display().to_string(),
        ])?;
        let store = client.device_store("Nova")?;
        let first = store.identity()?;
        let reopened = client.device_store("Nova")?.identity()?;
        assert!(first.id == reopened.id && first.public_key() == reopened.public_key());
        let another_account = client.device_store("Second")?.identity()?;
        assert_ne!(first.public_key(), another_account.public_key());
        let id = [4; 16];
        let key = [7; 32];
        let device = |public_key, revoked| DeviceRecord {
            device_id: id,
            public_key,
            revoked,
        };
        let state = |records| -> Result<TrustState> {
            Ok(store.snapshot("Nova", &first, records)?.entries[0].state)
        };
        assert_eq!(state(vec![device(key, false)])?, TrustState::Unverified);
        let digest = fingerprint("Nova", &id, &key);
        assert_eq!(
            normalize_fingerprint(&display_fingerprint(&digest))?,
            digest
        );
        store.pin(id, &digest)?;
        assert_eq!(state(vec![device(key, false)])?, TrustState::Verified);
        let replacement = [8; 32];
        assert_eq!(
            state(vec![device(replacement, false)])?,
            TrustState::Changed
        );
        assert!(
            store
                .pin(id, &fingerprint("Nova", &id, &replacement))
                .is_err()
        );
        assert_eq!(state(vec![device(key, true)])?, TrustState::Revoked);
        assert_eq!(state(Vec::new())?, TrustState::Missing);
        Ok(())
    }
}
