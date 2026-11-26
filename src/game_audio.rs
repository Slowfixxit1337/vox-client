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
    rtp_parameters::RtpEncodingParameters,
    rtp_sender::RtpSender,
    rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit},
};
use std::{borrow::Cow, future::pending};
use tokio::sync::{mpsc, watch};

const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const SAMPLES_PER_CHANNEL: u32 = RATE / 100;
const TRACK_ID: &str = "vox-lab-share-audio";

pub struct Controls {
    pub target: watch::Sender<Option<u32>>,
    pub listen: watch::Sender<bool>,
    pub status: watch::Receiver<String>,
}

pub struct Settings {
    send: [u8; 32],
    receive: [u8; 32],
    target: watch::Receiver<Option<u32>>,
    listen: watch::Receiver<bool>,
    status: watch::Sender<String>,
    #[cfg(test)]
    probe: Option<mpsc::Sender<Vec<i16>>>,
    #[cfg(test)]
    capture_probe: Option<mpsc::Sender<Vec<i16>>>,
}

pub fn channel(send: [u8; 32], receive: [u8; 32]) -> (Controls, Settings) {
    let (target, requested) = watch::channel(None);
    let (listen, desired_playout) = watch::channel(true);
    let (status, observed) = watch::channel(String::from("Без звука игры"));
    (
        Controls {
            target,
            listen,
            status: observed,
        },
        Settings {
            send,
            receive,
            target: requested,
            listen: desired_playout,
            status,
            #[cfg(test)]
            probe: None,
            #[cfg(test)]
            capture_probe: None,
        },
    )
}

pub struct Session {
    factory: PeerConnectionFactory,
    local: MediaStreamTrack,
    remote_track: MediaStreamTrack,
    source: NativeAudioSource,
    remote: NativeAudioStream,
    rtp_sender: RtpSender,
    sender: FrameCryptor,
    receiver: FrameCryptor,
    errors: mpsc::Receiver<(bool, EncryptionState)>,
    settings: Settings,
}

impl Session {
    pub fn prepare(
        factory: &PeerConnectionFactory,
        peer: &PeerConnection,
        settings: Settings,
    ) -> Result<Self> {
        let source = NativeAudioSource::new(
            AudioSourceOptions {
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain_control: false,
            },
            RATE,
            CHANNELS,
            0,
        );
        let local = MediaStreamTrack::from(factory.create_audio_track(TRACK_ID, source.clone()));
        local.set_enabled(false);
        let transceiver = peer.add_transceiver(
            local.clone(),
            RtpTransceiverInit {
                direction: RtpTransceiverDirection::SendRecv,
                stream_ids: vec![],
                send_encodings: vec![RtpEncodingParameters {
                    active: false,
                    ..Default::default()
                }],
            },
        )?;
        let opus: Vec<_> = factory
            .get_rtp_sender_capabilities(MediaType::Audio)
            .codecs
            .into_iter()
            .filter(|codec| codec.mime_type.eq_ignore_ascii_case("audio/opus"))
            .collect();
        ensure!(!opus.is_empty(), "Opus is unavailable");
        transceiver.set_codec_preferences(opus)?;
        let remote = transceiver
            .receiver()
            .track()
            .context("missing ShareAudio receiver")?;
        let MediaStreamTrack::Audio(track) = &remote else {
            anyhow::bail!("ShareAudio receiver is not audio");
        };
        let stream = NativeAudioStream::new(track.clone(), RATE as i32, CHANNELS as i32);
        let rtp_sender = transceiver.sender();
        let sender = FrameCryptor::new_for_rtp_sender(
            factory,
            TRACK_ID.into(),
            EncryptionAlgorithm::AesGcm,
            keys(settings.send)?,
            rtp_sender.clone(),
        );
        let receiver = FrameCryptor::new_for_rtp_receiver(
            factory,
            TRACK_ID.into(),
            EncryptionAlgorithm::AesGcm,
            keys(settings.receive)?,
            transceiver.receiver(),
        );
        let (tx, errors) = mpsc::channel(16);
        for (sending, cryptor) in [(true, &sender), (false, &receiver)] {
            let tx = tx.clone();
            cryptor.on_state_change(Some(Box::new(move |_, state| {
                if matches!(
                    state,
                    EncryptionState::MissingKey
                        | EncryptionState::InternalError
                        | EncryptionState::EncryptionFailed
                        | EncryptionState::DecryptionFailed
                ) {
                    let _ = tx.try_send((sending, state));
                }
            })));
            cryptor.set_key_index(0);
            cryptor.set_enabled(true);
        }
        ensure!(
            sender.enabled() && receiver.enabled(),
            "ShareAudio cryptors are inactive"
        );
        Ok(Self {
            factory: factory.clone(),
            local,
            remote_track: remote,
            source,
            remote: stream,
            rtp_sender,
            sender,
            receiver,
            errors,
            settings,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        ensure!(
            self.sender.enabled() && self.receiver.enabled(),
            "ShareAudio cryptors are inactive"
        );
        self.set_active(false)?;
        #[cfg(not(test))]
        let _playout = Playout::start(&self.factory, true);
        #[cfg(test)]
        let _playout = Playout::start(&self.factory, self.settings.probe.is_none());
        let mut capture: Option<async_channel::Receiver<Result<Vec<i16>>>> = None;
        let mut publishing = false;
        self.remote_track
            .set_enabled(*self.settings.listen.borrow_and_update());
        loop {
            tokio::select! {
                changed = self.settings.target.changed() => {
                    changed.context("ShareAudio controls closed")?;
                    capture = None;
                    if publishing {
                        self.set_active(false)?;
                        publishing = false;
                    }
                    let target = *self.settings.target.borrow_and_update();
                    if let Some(pid) = target {
                        match crate::process_audio::capture(pid) {
                            Ok(reader) => {
                                capture = Some(reader);
                                self.settings.status.send_replace("Подключаем звук приложения…".into());
                            }
                            Err(error) => {
                                self.settings.status.send_replace(format!("Без звука · {error}"));
                            }
                        }
                    } else {
                        self.settings.status.send_replace("Без звука игры".into());
                    }
                }
                chunk = async {
                    match &capture {
                        Some(capture) => capture.recv().await.ok(),
                        None => pending().await,
                    }
                } => {
                    let result = chunk.context("источник звука закрыт").and_then(|chunk| chunk);
                    match result {
                        Ok(samples) if !self.settings.target.has_changed()? => {
                            let frame = pcm(&samples)?;
                            #[cfg(test)]
                            if let Some(probe) = &self.settings.capture_probe {
                                let _ = probe.try_send(samples.clone());
                            }
                            if !publishing {
                                self.set_active(true)?;
                                publishing = true;
                                self.settings.status.send_replace("Звук приложения · E2EE".into());
                            }
                            self.source.capture_frame(&frame).await.context("передача звука приложения")?;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            capture = None;
                            self.set_active(false)?;
                            publishing = false;
                            self.settings.status.send_replace(format!("Без звука · {error}"));
                        }
                    }
                }
                incoming = self.remote.next() => {
                    let frame = incoming.context("ShareAudio receiver closed")?;
                    #[cfg(test)]
                    if let Some(probe) = &self.settings.probe {
                        let _ = probe.try_send(frame.data.into_owned());
                    }
                    #[cfg(not(test))]
                    drop(frame);
                }
                changed = self.settings.listen.changed() => {
                    changed.context("ShareAudio playout controls closed")?;
                    self.remote_track.set_enabled(*self.settings.listen.borrow_and_update());
                }
                state = self.errors.recv() => {
                    let (sending, state) = state.context("ShareAudio observer closed")?;
                    if sending {
                        capture = None;
                        self.set_active(false)?;
                        publishing = false;
                        self.settings.status.send_replace("Без звука · ошибка шифрования".into());
                    }
                    eprintln!("ShareAudio cryptor: {state:?}");
                }
            }
        }
    }

    fn set_active(&self, active: bool) -> Result<()> {
        let mut params = self.rtp_sender.parameters();
        let [encoding] = params.encodings.as_mut_slice() else {
            anyhow::bail!("ShareAudio requires one Opus encoding");
        };
        encoding.active = active;
        self.rtp_sender.set_parameters(params)?;
        self.local.set_enabled(active);
        Ok(())
    }

    pub fn unavailable(&self, error: &anyhow::Error) {
        self.local.set_enabled(false);
        self.remote_track.set_enabled(false);
        let _ = self.set_active(false);
        self.settings
            .status
            .send_replace(format!("Без звука · {error}"));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.local.set_enabled(false);
        self.remote_track.set_enabled(false);
    }
}

struct Playout(PeerConnectionFactory);

impl Playout {
    fn start(factory: &PeerConnectionFactory, use_speakers: bool) -> Option<Self> {
        if !use_speakers {
            factory.set_adm_playout_enabled(true);
            return None;
        }
        if !factory.acquire_platform_adm() {
            eprintln!("Windows playout unavailable for ShareAudio");
            return None;
        }
        factory.set_adm_playout_enabled(true);
        Some(Self(factory.clone()))
    }
}

impl Drop for Playout {
    fn drop(&mut self) {
        self.0.release_platform_adm();
    }
}

fn pcm(samples: &[i16]) -> Result<AudioFrame<'_>> {
    ensure!(
        samples.len() == (CHANNELS * SAMPLES_PER_CHANNEL) as usize,
        "invalid ShareAudio PCM packet"
    );
    Ok(AudioFrame {
        data: Cow::Borrowed(samples),
        sample_rate: RATE,
        num_channels: CHANNELS,
        samples_per_channel: SAMPLES_PER_CHANNEL,
    })
}

fn keys(key: [u8; 32]) -> Result<KeyProvider> {
    let provider = KeyProvider::new(KeyProviderOptions {
        shared_key: false,
        ratchet_window_size: 0,
        ratchet_salt: b"vox-lab/share-audio/v1".to_vec(),
        failure_tolerance: 0,
        key_ring_size: 1,
        key_derivation_algorithm: KeyDerivationAlgorithm::HKDF,
    });
    ensure!(
        provider.set_key(TRACK_ID.into(), 0, key.to_vec()),
        "WebRTC refused the ShareAudio key"
    );
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{join, load_lab_profile, media::Connection, video, voice};
    use std::{path::Path, time::Duration};
    use tokio::time;
    use vox_client::DevAddress;
    use windows::{
        Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_LOOP, SND_MEMORY, SND_NODEFAULT},
        core::PCWSTR,
    };

    #[test]
    fn accepts_only_ten_milliseconds_of_stereo_pcm() -> Result<()> {
        let samples = [1; 960];
        let frame = pcm(&samples)?;
        assert_eq!(
            (
                frame.sample_rate,
                frame.num_channels,
                frame.samples_per_channel
            ),
            (48_000, 2, 480)
        );
        assert!(pcm(&samples[..480]).is_err());
        assert!(pcm(&samples[..959]).is_err());
        Ok(())
    }

    const GAME_HZ: f32 = 600.0;
    const VOX_HZ: f32 = 1000.0;
    const VIEWER_VOICE_HZ: f32 = 700.0;

    #[test]
    #[ignore = "produces a quiet synthetic render stream in a separate process"]
    fn play_game_fixture() -> Result<()> {
        let _render = Tone::start(GAME_HZ)?;
        println!("VOX_GAME_FIXTURE_READY");
        std::thread::sleep(Duration::from_secs(35));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires an interactive Windows audio device, a provisioned SFU and the separate game fixture"]
    async fn sends_only_game_process_audio_through_sfu() -> Result<()> {
        let address = DevAddress::parse(&std::env::var("VOX_TEST_SERVER")?)?;
        let profile_a = std::env::var("VOX_TEST_PROFILE_A")?;
        let profile_b = std::env::var("VOX_TEST_PROFILE_B")?;
        let game_pid = std::env::var("VOX_TEST_GAME_PID")?.parse::<u32>()?;
        let (probe, mut decoded) = mpsc::channel(32);
        let (capture_probe, mut captured) = mpsc::channel(64);
        let ((a, a_video, a_audio), (b, b_video, _b_audio)) =
            time::timeout(Duration::from_secs(16), async {
                tokio::try_join!(
                    connect(&address, &profile_a, None, Some(capture_probe)),
                    connect(&address, &profile_b, Some(probe), None)
                )
            })
            .await??;
        let _video = (a_video, b_video);
        let mut a = tokio::spawn(a.maintain());
        let mut b = tokio::spawn(b.maintain());
        let result = tokio::select! {
            result = async {
                let _other_render = Tone::start(VOX_HZ)?;
                a_audio.target.send_replace(Some(game_pid));
                let frames = time::timeout(Duration::from_secs(12), async {
                    let mut heard = 0;
                    let mut warmup = 0;
                    let mut voice_only = 0;
                    while heard < 25 {
                        let frame = decoded.recv().await.context("ShareAudio PCМ observer closed")?;
                        let game = amplitude(&frame, GAME_HZ);
                        let other = amplitude(&frame, VOX_HZ);
                        let returned_voice = amplitude(&frame, VIEWER_VOICE_HZ);
                        // A partial first game period spreads across nearby bins.
                        // Check for sustained isolated voice before the game and
                        // for any competing marker once the game tone has settled.
                        voice_only = if returned_voice >= 250.0 && game < 300.0 { voice_only + 1 } else { 0 };
                        ensure!(voice_only < 3, "viewer voice returned without a game signal");
                        if game > 300.0 && warmup < 8 {
                            warmup += 1;
                            continue;
                        }
                        if warmup < 8 { continue; }
                        ensure!(other < 160.0, "unselected Vox marker appeared in the game track ({other:.1})");
                        if returned_voice >= 160.0 {
                            let mut local = Vec::new();
                            while let Ok(samples) = captured.try_recv() {
                                local.push((amplitude(&samples, GAME_HZ), amplitude(&samples, VIEWER_VOICE_HZ)));
                            }
                            anyhow::bail!("viewer voice marker in received PCM ({returned_voice:.1}), game amplitude {game:.1}; local game/voice amplitude: {local:?}");
                        }
                        if game > 300.0 { heard += 1; }
                    }
                    let mut steady = 0;
                    while let Ok(samples) = captured.try_recv() {
                        if amplitude(&samples, GAME_HZ) > 2100.0 {
                            ensure!(amplitude(&samples, VIEWER_VOICE_HZ) < 30.0,
                                "viewer voice is present in the selected process PCM");
                            steady += 1;
                        }
                    }
                    ensure!(steady >= 10, "not enough steady local PCM blocks");
                    Ok::<_, anyhow::Error>(heard)
                }).await.with_context(|| format!("game tone did not reach viewer. {}", *a_audio.status.borrow()))??;
                a_audio.target.send_replace(None);
                println!("ShareAudio decoded {frames} steady game-tone frames without either competing marker");
                Ok(())
            } => result,
            result = &mut a => anyhow::bail!("publisher disconnected: {result:?}"),
            result = &mut b => anyhow::bail!("viewer disconnected: {result:?}"),
        };
        a.abort();
        b.abort();
        result
    }

    async fn connect(
        address: &DevAddress,
        path: &str,
        probe: Option<mpsc::Sender<Vec<i16>>>,
        capture_probe: Option<mpsc::Sender<Vec<i16>>>,
    ) -> Result<(Connection, video::share::DesktopFeed, Controls)> {
        let (profile, tls) = load_lab_profile(Path::new(path))?;
        let (send, receive) = profile
            .video
            .as_ref()
            .context("missing video keys")?
            .keys()?;
        let (audio_send, audio_receive) = profile
            ._share_audio
            .as_ref()
            .context("missing ShareAudio keys")?
            .keys()?;
        let a_side = profile.device == "a";
        let (voice_send, voice_receive) = profile
            .voice
            .as_ref()
            .context("missing microphone fixture keys")?
            .keys()?;
        let reply = join(address, profile, tls.clone()).await?;
        let (video_feed, video_pipe) = video::share::channel();
        let (audio_feed, mut audio_pipe) = channel(audio_send, audio_receive);
        audio_pipe.probe = probe;
        audio_pipe.capture_probe = capture_probe;
        let connection = Connection::connect(
            address,
            reply,
            tls,
            Some(voice::Settings {
                mode: voice::Mode::Tone,
                send: voice_send,
                receive: voice_receive,
                a_side,
            }),
            Some(video::Settings {
                send,
                receive,
                a_side,
                share: Some(video_pipe),
                audio: Some(audio_pipe),
            }),
        )
        .await?;
        Ok((connection, video_feed, audio_feed))
    }

    fn amplitude(frame: &[i16], frequency: f32) -> f32 {
        if frame.len() != (SAMPLES_PER_CHANNEL * CHANNELS) as usize {
            return 0.0;
        }
        let (real, imaginary) = frame.iter().step_by(CHANNELS as usize).enumerate().fold(
            (0.0, 0.0),
            |(r, i), (sample, value)| {
                let angle = std::f32::consts::TAU * frequency * sample as f32 / RATE as f32;
                let (sin, cos) = angle.sin_cos();
                (r + cos * f32::from(*value), i + sin * f32::from(*value))
            },
        );
        real.hypot(imaginary) * 2.0 / SAMPLES_PER_CHANNEL as f32
    }

    struct Tone {
        _wave: Vec<u8>,
    }
    impl Tone {
        fn start(frequency: f32) -> Result<Self> {
            let mut wave = Vec::with_capacity(44 + RATE as usize * 2);
            let data = RATE * 2;
            wave.extend_from_slice(b"RIFF");
            wave.extend_from_slice(&(36 + data).to_le_bytes());
            wave.extend_from_slice(b"WAVEfmt ");
            wave.extend_from_slice(&16_u32.to_le_bytes());
            wave.extend_from_slice(&1_u16.to_le_bytes());
            wave.extend_from_slice(&1_u16.to_le_bytes());
            wave.extend_from_slice(&RATE.to_le_bytes());
            wave.extend_from_slice(&(RATE * 2).to_le_bytes());
            wave.extend_from_slice(&2_u16.to_le_bytes());
            wave.extend_from_slice(&16_u16.to_le_bytes());
            wave.extend_from_slice(b"data");
            wave.extend_from_slice(&data.to_le_bytes());
            for i in 0..RATE {
                let t = std::f32::consts::TAU * frequency * i as f32 / RATE as f32;
                wave.extend_from_slice(&((t.sin() * 2200.0) as i16).to_le_bytes());
            }
            ensure!(
                unsafe {
                    PlaySoundW(
                        PCWSTR(wave.as_ptr().cast()),
                        None,
                        SND_MEMORY | SND_ASYNC | SND_LOOP | SND_NODEFAULT,
                    )
                }
                .as_bool(),
                "Windows could not play the synthetic fixture"
            );
            Ok(Self { _wave: wave })
        }
    }

    impl Drop for Tone {
        fn drop(&mut self) {
            // Stop playback before releasing the borrowed in-memory WAV.
            let _ = unsafe { PlaySoundW(PCWSTR::null(), None, SND_NODEFAULT) };
        }
    }
}
