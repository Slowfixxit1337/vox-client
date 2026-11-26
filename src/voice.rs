use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use libwebrtc::{
    MediaType,
    audio_frame::AudioFrame,
    audio_source::{AudioSourceOptions, native::NativeAudioSource},
    audio_stream::native::NativeAudioStream,
    media_stream_track::MediaStreamTrack,
    native::frame_cryptor::{
        EncryptionAlgorithm, EncryptionState, FrameCryptor, KeyDerivationAlgorithm, KeyProvider,
        KeyProviderOptions,
    },
    peer_connection::PeerConnection,
    peer_connection_factory::{PeerConnectionFactory, native::PeerConnectionFactoryExt},
    rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit},
};
use std::{borrow::Cow, f32::consts::TAU, future::pending, time::Duration};
use tokio::{
    sync::mpsc,
    time::{self, MissedTickBehavior},
};

const RATE: u32 = 48_000;
const SAMPLES: usize = RATE as usize / 100;
const PARTICIPANT: &str = "vox-lab-mic";

pub enum Mode {
    Tone,
    Mic,
}

pub struct Settings {
    pub mode: Mode,
    pub send: [u8; 32],
    pub receive: [u8; 32],
    pub a_side: bool,
}

enum Input {
    Tone(NativeAudioSource),
    Mic(PlatformAudio),
}

struct PlatformAudio(PeerConnectionFactory);

impl PlatformAudio {
    fn prepare(factory: &PeerConnectionFactory) -> Result<Self> {
        ensure!(
            factory.acquire_platform_adm(),
            "Windows audio devices are unavailable"
        );
        let audio = Self(factory.clone());
        audio.0.set_adm_recording_enabled(false);
        audio.0.set_adm_playout_enabled(false);
        Ok(audio)
    }

    fn enable(&self) {
        self.0.set_adm_playout_enabled(true);
        self.0.set_adm_recording_enabled(true);
    }
}

impl Drop for PlatformAudio {
    fn drop(&mut self) {
        self.0.set_adm_recording_enabled(false);
        self.0.set_adm_playout_enabled(false);
        self.0.release_platform_adm();
    }
}

pub struct Session {
    local: MediaStreamTrack,
    input: Input,
    remote: NativeAudioStream,
    sender: FrameCryptor,
    receiver: FrameCryptor,
    sender_state: mpsc::Receiver<EncryptionState>,
    receiver_state: mpsc::Receiver<EncryptionState>,
    send_hz: f32,
    receive_hz: f32,
}

impl Session {
    pub fn prepare(
        factory: &PeerConnectionFactory,
        peer: &PeerConnection,
        settings: Settings,
    ) -> Result<Self> {
        let (local, input) = match settings.mode {
            Mode::Tone => {
                factory.set_adm_recording_enabled(false);
                // The default synthetic ADM clocks the decoder without opening speakers.
                factory.set_adm_playout_enabled(true);
                let source = NativeAudioSource::new(AudioSourceOptions::default(), RATE, 1, 0);
                (
                    factory.create_audio_track("vox-mic", source.clone()),
                    Input::Tone(source),
                )
            }
            Mode::Mic => {
                let audio = PlatformAudio::prepare(factory)?;
                (
                    factory.create_device_audio_track("vox-mic"),
                    Input::Mic(audio),
                )
            }
        };
        let local = MediaStreamTrack::from(local);
        local.set_enabled(false);
        let transceiver = peer.add_transceiver(
            local.clone(),
            RtpTransceiverInit {
                direction: RtpTransceiverDirection::SendRecv,
                stream_ids: vec![],
                send_encodings: vec![],
            },
        )?;
        let opus: Vec<_> = factory
            .get_rtp_sender_capabilities(MediaType::Audio)
            .codecs
            .into_iter()
            .filter(|codec| codec.mime_type.eq_ignore_ascii_case("audio/opus"))
            .collect();
        ensure!(!opus.is_empty(), "Opus is unavailable in this WebRTC build");
        transceiver.set_codec_preferences(opus)?;
        let remote = match transceiver.receiver().track() {
            Some(MediaStreamTrack::Audio(track)) => NativeAudioStream::new(track, RATE as i32, 1),
            _ => anyhow::bail!("microphone transceiver has no audio receiver"),
        };
        let sender = FrameCryptor::new_for_rtp_sender(
            factory,
            PARTICIPANT.into(),
            EncryptionAlgorithm::AesGcm,
            key_provider(settings.send)?,
            transceiver.sender(),
        );
        let receiver = FrameCryptor::new_for_rtp_receiver(
            factory,
            PARTICIPANT.into(),
            EncryptionAlgorithm::AesGcm,
            key_provider(settings.receive)?,
            transceiver.receiver(),
        );
        let sender_state = observe(&sender)?;
        let receiver_state = observe(&receiver)?;
        let (send_hz, receive_hz) = if settings.a_side {
            (400.0, 700.0)
        } else {
            (700.0, 400.0)
        };
        Ok(Self {
            local,
            input,
            remote,
            sender,
            receiver,
            sender_state,
            receiver_state,
            send_hz,
            receive_hz,
        })
    }

    /// Starts the source only after the caller has confirmed both ICE/DTLS endpoints.
    pub async fn run(&mut self) -> Result<()> {
        ensure!(
            self.sender.enabled() && self.receiver.enabled(),
            "audio frame cryptors are inactive"
        );
        if let Input::Mic(audio) = &self.input {
            audio.enable();
        }
        self.local.set_enabled(true);
        let tone = matches!(self.input, Input::Tone(_));
        tokio::select! {
            input = send_audio(&self.input, self.send_hz) => input,
            output = receive_audio(&mut self.remote, self.receive_hz, tone) => output,
            error = monitor_crypto(&mut self.sender_state, &mut self.receiver_state) => error,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.local.set_enabled(false);
    }
}

fn key_provider(key: [u8; 32]) -> Result<KeyProvider> {
    let provider = KeyProvider::new(KeyProviderOptions {
        shared_key: false,
        ratchet_window_size: 0,
        ratchet_salt: b"vox-lab/mic/v1".to_vec(),
        failure_tolerance: 0,
        key_ring_size: 1,
        key_derivation_algorithm: KeyDerivationAlgorithm::HKDF,
    });
    ensure!(
        provider.set_key(PARTICIPANT.into(), 0, key.to_vec()),
        "WebRTC refused the media key"
    );
    Ok(provider)
}

fn observe(cryptor: &FrameCryptor) -> Result<mpsc::Receiver<EncryptionState>> {
    let (tx, rx) = mpsc::channel(16);
    cryptor.on_state_change(Some(Box::new(move |_, state| {
        let _ = tx.try_send(state);
    })));
    cryptor.set_key_index(0);
    cryptor.set_enabled(true);
    ensure!(cryptor.enabled(), "audio frame cryptor failed to start");
    Ok(rx)
}

async fn send_audio(input: &Input, hz: f32) -> Result<()> {
    let Input::Tone(source) = input else {
        return pending().await;
    };
    let samples: Vec<i16> = (0..SAMPLES)
        .map(|sample| (6000.0 * (TAU * hz * sample as f32 / RATE as f32).sin()) as i16)
        .collect();
    let frame = AudioFrame {
        data: Cow::Borrowed(&samples),
        sample_rate: RATE,
        num_channels: 1,
        samples_per_channel: SAMPLES as u32,
    };
    let mut tick = time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        source
            .capture_frame(&frame)
            .await
            .context("feeding E2EE test tone")?;
    }
}

async fn receive_audio(stream: &mut NativeAudioStream, frequency: f32, tone: bool) -> Result<()> {
    let mut announced = false;
    while let Some(frame) = stream.next().await {
        if announced {
            continue;
        }
        let heard = if tone {
            let (sin, cos) =
                frame
                    .data
                    .iter()
                    .enumerate()
                    .fold((0.0, 0.0), |(sin, cos), (i, sample)| {
                        let (s, c) = (TAU * frequency * i as f32 / RATE as f32).sin_cos();
                        (sin + s * f32::from(*sample), cos + c * f32::from(*sample))
                    });
            frame.sample_rate == RATE
                && frame.data.len() == SAMPLES
                && 2.0 * sin.hypot(cos) / SAMPLES as f32 > 500.0
        } else {
            frame.data.iter().any(|sample| sample.unsigned_abs() > 1000)
        };
        if heard {
            announced = true;
            println!("E2EE {} decoded", if tone { "tone" } else { "voice" });
        }
    }
    anyhow::bail!("E2EE audio receiver closed")
}

async fn monitor_crypto(
    sender: &mut mpsc::Receiver<EncryptionState>,
    receiver: &mut mpsc::Receiver<EncryptionState>,
) -> Result<()> {
    loop {
        tokio::select! {
            message = sender.recv() => {
                let state = message.context("audio sender cryptor closed")?;
                ensure!(!matches!(state, EncryptionState::EncryptionFailed | EncryptionState::MissingKey | EncryptionState::InternalError),
                    "E2EE audio sender failed: {state:?}");
            }
            message = receiver.recv() => {
                let state = message.context("audio receiver cryptor closed")?;
                if matches!(state, EncryptionState::DecryptionFailed | EncryptionState::MissingKey | EncryptionState::InternalError) {
                    eprintln!("E2EE receiver rejected audio: {state:?}");
                }
            }
        }
    }
}
