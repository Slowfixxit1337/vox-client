use anyhow::{Context, Result, ensure};
use rand::{TryRng, rngs::SysRng};
use std::{
    fs,
    io::{ErrorKind, Read, Write},
    path::PathBuf,
    slice,
    time::Duration,
};
use tokio::time::timeout;
use tokio_tungstenite::Connector;
use vox_client::DevAddress;
use vox_protocol::{
    CAP_AUTH, CAP_FRAME_E2EE, CAP_TWO_FACTOR,
    auth::{AuthReply, AuthRequest, Operation, Outcome},
};
use windows::{
    Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    },
    core::PCWSTR,
};

mod devices;
pub use devices::{DeviceSnapshot, TrustState};

const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const SESSION_FILE_LIMIT: usize = 16 * 1024;

#[derive(Clone)]
pub struct Client {
    address: DevAddress,
    tls: Option<Connector>,
    session: PathBuf,
}

impl Client {
    pub fn from_options(mut options: &[String]) -> Result<Self> {
        let mut address = None;
        let mut ca = None;
        let mut profile_dir = None;
        while !options.is_empty() {
            options = match options {
                [flag, value, tail @ ..] if flag == "--server" && address.is_none() => {
                    address = Some(DevAddress::parse(value)?);
                    tail
                }
                [flag, value, tail @ ..] if flag == "--ca" && ca.is_none() => {
                    ca = Some(super::test_connector(std::path::Path::new(value))?);
                    tail
                }
                [flag, value, tail @ ..] if flag == "--profile-dir" && profile_dir.is_none() => {
                    profile_dir = Some(PathBuf::from(value));
                    tail
                }
                _ => anyhow::bail!(
                    "usage: vox-client --auth [--server HOST[:PORT]] [--ca PEM] [--profile-dir DIR]"
                ),
            };
        }
        let address = match address {
            Some(address) => address,
            None => DevAddress::parse(
                &std::env::var("VOX_SERVER").unwrap_or_else(|_| "localhost:8443".into()),
            )?,
        };
        let base = match profile_dir {
            Some(base) => base,
            None => PathBuf::from(
                std::env::var_os("LOCALAPPDATA").context("Windows profile folder unavailable")?,
            )
            .join("Vox")
            .join("accounts"),
        };
        let session = base.join(address.profile_key()).join("session.dpapi");
        Ok(Self {
            address,
            tls: ca,
            session,
        })
    }

    pub async fn call(&self, operation: Operation) -> Result<Outcome> {
        timeout(AUTH_TIMEOUT, async {
            let capabilities = CAP_FRAME_E2EE | CAP_AUTH | operation.required_capability();
            let mut socket = super::open_with_optional(
                format!("wss://{}/v1/auth", self.address.authority()),
                self.tls.clone(),
                capabilities,
                CAP_TWO_FACTOR,
            )
            .await?;
            let result: AuthReply =
                super::exchange(&mut socket, 2, &AuthRequest { operation }).await?;
            Ok(result.outcome)
        })
        .await
        .context("Vox account request timed out")?
    }

    pub fn saved_session(&self) -> Result<Option<String>> {
        let file = match fs::File::open(&self.session) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("opening the Vox session profile"),
        };
        let mut ciphertext = Vec::new();
        file.take((SESSION_FILE_LIMIT + 1) as u64)
            .read_to_end(&mut ciphertext)?;
        ensure!(
            !ciphertext.is_empty() && ciphertext.len() <= SESSION_FILE_LIMIT,
            "invalid Vox session profile"
        );
        let plaintext = protect(&ciphertext, self.address.authority().as_bytes(), false)?;
        let token = std::str::from_utf8(plaintext.bytes()?)?;
        validate_token(token)?;
        Ok(Some(token.to_owned()))
    }

    pub fn remember_session(&self, token: &str) -> Result<()> {
        validate_token(token)?;
        let ciphertext = protect(token.as_bytes(), self.address.authority().as_bytes(), true)?;
        let parent = self
            .session
            .parent()
            .context("missing Vox session folder")?;
        fs::create_dir_all(parent)?;
        let mut nonce = [0; 12];
        SysRng
            .try_fill_bytes(&mut nonce)
            .map_err(|error| anyhow::anyhow!("Windows random source failed: {error}"))?;
        let temporary = parent.join(format!("{}.tmp", hex::encode(nonce)));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let written = file
            .write_all(ciphertext.bytes()?)
            .and_then(|()| file.sync_all());
        drop(file);
        let result = written.and_then(|()| fs::rename(&temporary, &self.session));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.context("saving the Vox session")
    }

    pub fn clear_session(&self) -> Result<()> {
        match fs::remove_file(&self.session) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("removing the Vox session"),
        }
    }
}

fn validate_token(token: &str) -> Result<()> {
    ensure!(
        (32..=512).contains(&token.len())
            && token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid Vox session token"
    );
    Ok(())
}

struct DpapiBlob {
    blob: CRYPT_INTEGER_BLOB,
    sensitive: bool,
}

impl DpapiBlob {
    fn bytes(&self) -> Result<&[u8]> {
        ensure!(
            !self.blob.pbData.is_null() && self.blob.cbData != 0,
            "Windows returned an empty session blob"
        );
        Ok(unsafe { slice::from_raw_parts(self.blob.pbData, self.blob.cbData as usize) })
    }
}

impl Drop for DpapiBlob {
    fn drop(&mut self) {
        if !self.blob.pbData.is_null() {
            if self.sensitive {
                for index in 0..self.blob.cbData as usize {
                    unsafe { self.blob.pbData.add(index).write_volatile(0) };
                }
            }
            let _ = unsafe { LocalFree(Some(HLOCAL(self.blob.pbData.cast()))) };
        }
    }
}

fn protect(input: &[u8], origin: &[u8], encrypt: bool) -> Result<DpapiBlob> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: input.len().try_into()?,
        pbData: input.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: origin.len().try_into()?,
        pbData: origin.as_ptr().cast_mut(),
    };
    let mut output = DpapiBlob {
        blob: CRYPT_INTEGER_BLOB::default(),
        sensitive: !encrypt,
    };
    unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.blob,
            )?;
        } else {
            CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.blob,
            )?;
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_protocol::auth::AuthError;

    #[test]
    fn session_tokens_are_bounded() {
        assert!(super::validate_token(&"00".repeat(32)).is_ok());
        assert!(super::validate_token("short").is_err());
        assert!(super::validate_token(&"0".repeat(513)).is_err());
        assert!(super::validate_token(&format!("{}\n", "0".repeat(63))).is_err());
    }

    #[test]
    fn dpapi_binds_session_to_server_origin() -> Result<()> {
        let secret = "synthetic-token-for-dpapi-test-12345";
        let ciphertext = protect(secret.as_bytes(), b"localhost:8443", true)?;
        assert!(
            !ciphertext
                .bytes()?
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes())
        );
        assert!(protect(ciphertext.bytes()?, b"localhost:8444", false).is_err());
        assert_eq!(
            protect(ciphertext.bytes()?, b"localhost:8443", false)?.bytes()?,
            secret.as_bytes()
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires a local Vox API and test mail outbox"]
    async fn register_confirm_sign_in_and_revoke() -> Result<()> {
        let root = tempfile::tempdir()?;
        let server = std::env::var("VOX_TEST_AUTH_SERVER")?;
        let ca = std::env::var("VOX_TEST_AUTH_CA")?;
        let outbox =
            PathBuf::from(std::env::var_os("VOX_TEST_AUTH_OUTBOX").context("missing test outbox")?);
        let client = Client::from_options(&[
            "--server".into(),
            server,
            "--ca".into(),
            ca,
            "--profile-dir".into(),
            root.path().display().to_string(),
        ])?;
        let mut random = [0_u8; 6];
        SysRng
            .try_fill_bytes(&mut random)
            .map_err(|error| anyhow::anyhow!("RNG failed: {error}"))?;
        let username = format!("smoke_{}", hex::encode(random));
        let email = format!("{username}@example.test");
        let password = "only-for-vox-auth-smoke-65!".to_owned();
        let outcome = client
            .call(Operation::Register {
                username: username.clone(),
                email: email.clone(),
                password: password.clone(),
            })
            .await?;
        let Outcome::VerificationRequired {
            verification,
            retry_after_seconds,
            ..
        } = outcome
        else {
            anyhow::bail!("registration did not return a pending challenge");
        };
        ensure!(
            retry_after_seconds > 0,
            "registration returned no resend delay"
        );
        assert!(matches!(
            client
                .call(Operation::ResendCode {
                    verification: verification.clone()
                })
                .await?,
            Outcome::Rejected {
                code: AuthError::RateLimited,
                ..
            }
        ));
        let code = timeout(Duration::from_secs(7), async {
            loop {
                for entry in fs::read_dir(&outbox)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        let message = fs::read_to_string(entry.path())?;
                        if message
                            .lines()
                            .any(|line| line.trim() == format!("To: {email}"))
                            && let Some(code) = message.lines().rev().map(str::trim).find(|line| {
                                line.len() == 6 && line.bytes().all(|byte| byte.is_ascii_digit())
                            })
                        {
                            return Ok::<_, anyhow::Error>(code.to_owned());
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(75)).await;
            }
        })
        .await
        .context("verification email was not written")??;
        let outcome = client
            .call(Operation::ConfirmEmail { verification, code })
            .await?;
        let Outcome::Authenticated {
            username: confirmed,
            session,
            ..
        } = outcome
        else {
            anyhow::bail!("confirmation did not create a session");
        };
        assert_eq!(confirmed, username);
        let directory = client.device_catalog(&username, &session).await?;
        assert_eq!(directory.entries.len(), 1);
        assert_eq!(directory.entries[0].state, TrustState::Local);
        client.remember_session(&session)?;
        assert_eq!(client.saved_session()?, Some(session.clone()));
        assert!(matches!(
            client
                .call(Operation::ResumeSession {
                    session: session.clone()
                })
                .await?,
            Outcome::Authenticated { .. }
        ));
        assert!(matches!(
            client
                .call(Operation::SignOut {
                    session: session.clone()
                })
                .await?,
            Outcome::SignedOut
        ));
        assert!(matches!(
            client.call(Operation::ResumeSession { session }).await?,
            Outcome::Rejected {
                code: AuthError::SessionExpired,
                ..
            }
        ));
        client.clear_session()?;
        assert!(client.saved_session()?.is_none());
        for login in [username.clone(), email.clone()] {
            assert!(matches!(
                client
                    .call(Operation::SignIn {
                        login,
                        password: password.clone()
                    })
                    .await?,
                Outcome::Authenticated { .. }
            ));
        }
        assert!(matches!(
            client
                .call(Operation::RequestPasswordReset {
                    email: email.clone()
                })
                .await?,
            Outcome::PasswordResetRequested
        ));
        let token = timeout(Duration::from_secs(7), async {
            let recipient = format!("To: {email}");
            loop {
                for entry in fs::read_dir(&outbox)? {
                    let entry = entry?;
                    if entry.path().extension().is_some_and(|ext| ext == "txt") {
                        let message = fs::read_to_string(entry.path())?;
                        if message.lines().any(|line| line == recipient)
                            && let Some(token) = message
                                .lines()
                                .find_map(|line| line.strip_prefix("vox://reset/"))
                        {
                            return Ok::<_, anyhow::Error>(token.to_owned());
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(75)).await;
            }
        })
        .await
        .context("password reset email was not written")??;
        let replacement = "replacement-only-for-vox-auth-smoke";
        assert!(matches!(
            client
                .call(Operation::ResetPassword {
                    token,
                    password: replacement.into()
                })
                .await?,
            Outcome::PasswordReset
        ));
        let sign_in = |password: &str| Operation::SignIn {
            login: username.clone(),
            password: password.into(),
        };
        let Outcome::Authenticated {
            session: active, ..
        } = client.call(sign_in(replacement)).await?
        else {
            anyhow::bail!("reset password did not work over WSS");
        };
        let Outcome::Authenticated { session: other, .. } =
            client.call(sign_in(replacement)).await?
        else {
            anyhow::bail!("could not start second session over WSS");
        };
        let changed = "changed-only-for-vox-auth-smoke";
        assert!(matches!(
            client
                .call(Operation::ChangePassword {
                    session: active.clone(),
                    current_password: replacement.into(),
                    new_password: changed.into(),
                })
                .await?,
            Outcome::PasswordChanged
        ));
        assert!(matches!(
            client
                .call(Operation::ResumeSession { session: active })
                .await?,
            Outcome::Authenticated { .. }
        ));
        assert!(matches!(
            client
                .call(Operation::ResumeSession { session: other })
                .await?,
            Outcome::Rejected {
                code: AuthError::SessionExpired,
                ..
            }
        ));
        let Outcome::Authenticated { session, .. } = client.call(sign_in(changed)).await? else {
            anyhow::bail!("changed password did not work over WSS");
        };
        let Outcome::TwoFactorSetup { secret, .. } = client
            .call(Operation::BeginTwoFactor {
                session: session.clone(),
                password: changed.into(),
            })
            .await?
        else {
            anyhow::bail!("authenticator setup did not start");
        };
        let code = authenticator_code(&secret)?;
        let Outcome::TwoFactorEnabled { backup_codes } = client
            .call(Operation::ConfirmTwoFactor { session, code })
            .await?
        else {
            anyhow::bail!("authenticator enrollment failed");
        };
        ensure!(backup_codes.len() == 8, "expected eight recovery codes");
        for expected_success in [true, false] {
            let Outcome::TwoFactorRequired { challenge, .. } =
                client.call(sign_in(changed)).await?
            else {
                anyhow::bail!("password-only sign-in was not challenged");
            };
            let result = client
                .call(Operation::CompleteTwoFactorSignIn {
                    challenge,
                    code: backup_codes[0].clone(),
                })
                .await?;
            if expected_success {
                assert!(matches!(result, Outcome::Authenticated { .. }));
            } else {
                assert!(matches!(
                    result,
                    Outcome::Rejected {
                        code: AuthError::InvalidCode,
                        ..
                    }
                ));
            }
        }
        Ok(())
    }

    fn authenticator_code(secret: &str) -> Result<String> {
        use hmac::{Hmac, Mac};
        use std::time::{SystemTime, UNIX_EPOCH};
        let key = data_encoding::BASE32_NOPAD.decode(secret.as_bytes())?;
        let step = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() / 30;
        let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&key)?;
        mac.update(&step.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let offset = usize::from(digest[19] & 0x0f);
        let number = u32::from_be_bytes(digest[offset..offset + 4].try_into()?) & 0x7fff_ffff;
        Ok(format!("{:06}", number % 1_000_000))
    }
}
