use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use libwebrtc::{
    MediaType,
    media_stream_track::MediaStreamTrack,
    native::frame_cryptor::{
        EncryptionAlgorithm, EncryptionState, FrameCryptor, KeyDerivationAlgorithm, KeyProvider,
        KeyProviderOptions,
    },
    peer_connection::PeerConnection,
    peer_connection_factory::{PeerConnectionFactory, native::PeerConnectionFactoryExt},
    rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit},
    video_frame::{I420Buffer, VideoFrame, VideoRotation},
    video_source::{VideoResolution, native::NativeVideoSource},
    video_stream::native::NativeVideoStream,
};
use std::time::Duration;
use tokio::{
    sync::mpsc,
    time::{self, MissedTickBehavior},
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const ID: &str = "vox-lab-share-video";

#[cfg(all(feature = "desktop", feature = "lab"))]
pub(crate) mod share;

pub struct Settings {
    pub send: [u8; 32],
    pub receive: [u8; 32],
    pub a_side: bool,
    #[cfg(all(feature = "desktop", feature = "lab"))]
    pub share: Option<share::Pipeline>,
}

pub struct Session {
    local: MediaStreamTrack,
    source: NativeVideoSource,
    remote: NativeVideoStream,
    sender: FrameCryptor,
    receiver: FrameCryptor,
    errors: mpsc::Receiver<(bool, EncryptionState)>,
    a_side: bool,
    #[cfg(all(feature = "desktop", feature = "lab"))]
    share: Option<share::Pipeline>,
    #[cfg(all(feature = "desktop", feature = "lab"))]
    rtp_sender: libwebrtc::rtp_sender::RtpSender,
}

impl Session {
    pub fn prepare(
        factory: &PeerConnectionFactory,
        peer: &PeerConnection,
        settings: Settings,
    ) -> Result<Self> {
        let source = NativeVideoSource::new(
            VideoResolution {
                width: WIDTH,
                height: HEIGHT,
            },
            true,
        );
        let local =
            MediaStreamTrack::from(factory.create_video_track("vox-share-video", source.clone()));
        local.set_enabled(false);
        #[cfg(all(feature = "desktop", feature = "lab"))]
        let encodings = if settings.share.is_some() {
            vec![libwebrtc::rtp_parameters::RtpEncodingParameters {
                active: false,
                ..Default::default()
            }]
        } else {
            vec![]
        };
        #[cfg(not(all(feature = "desktop", feature = "lab")))]
        let encodings = vec![];
        let transceiver = peer.add_transceiver(
            local.clone(),
            RtpTransceiverInit {
                direction: RtpTransceiverDirection::SendRecv,
                stream_ids: vec![],
                send_encodings: encodings,
            },
        )?;
        let vp8: Vec<_> = factory
            .get_rtp_sender_capabilities(MediaType::Video)
            .codecs
            .into_iter()
            .filter(|codec| codec.mime_type.eq_ignore_ascii_case("video/vp8"))
            .collect();
        ensure!(!vp8.is_empty(), "VP8 is unavailable in this WebRTC build");
        transceiver.set_codec_preferences(vp8)?;
        let remote = match transceiver.receiver().track() {
            Some(MediaStreamTrack::Video(track)) => NativeVideoStream::new(track),
            _ => anyhow::bail!("ShareVideo transceiver has no video receiver"),
        };
        let rtp_sender = transceiver.sender();
        let sender = FrameCryptor::new_for_rtp_sender(
            factory,
            ID.into(),
            EncryptionAlgorithm::AesGcm,
            keys(settings.send)?,
            rtp_sender.clone(),
        );
        let receiver = FrameCryptor::new_for_rtp_receiver(
            factory,
            ID.into(),
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
            "VP8 frame cryptors are inactive"
        );
        Ok(Self {
            local,
            source,
            remote,
            sender,
            receiver,
            errors,
            a_side: settings.a_side,
            #[cfg(all(feature = "desktop", feature = "lab"))]
            share: settings.share,
            #[cfg(all(feature = "desktop", feature = "lab"))]
            rtp_sender,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        ensure!(
            self.sender.enabled() && self.receiver.enabled(),
            "VP8 frame cryptors are inactive"
        );
        #[cfg(all(feature = "desktop", feature = "lab"))]
        if let Some(share) = self.share.take() {
            return self.run_share(share).await;
        }
        self.local.set_enabled(true);
        let mut tick = time::interval(Duration::from_millis(80));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut index = 0;
        let mut announced = false;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let _ = self.source.capture_frame(&test_pattern(self.a_side, index));
                    index = (index + 1) % WIDTH as usize;
                }
                incoming = self.remote.next() => {
                    let incoming = incoming.context("VP8 receiver closed")?;
                    if !announced && incoming.buffer.as_i420().is_some_and(|image| matches_pattern(image, !self.a_side)) {
                        println!("E2EE VP8 test video decoded");
                        announced = true;
                    }
                }
                error = self.errors.recv() => {
                    let (sending, state) = error.context("VP8 cryptor observer closed")?;
                    ensure!(!sending, "E2EE VP8 sender failed: {state:?}");
                    eprintln!("E2EE VP8 receiver rejected video: {state:?}");
                }
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.local.set_enabled(false);
    }
}

fn keys(key: [u8; 32]) -> Result<KeyProvider> {
    let provider = KeyProvider::new(KeyProviderOptions {
        shared_key: false,
        ratchet_window_size: 0,
        ratchet_salt: b"vox-lab/share-video/v1".to_vec(),
        failure_tolerance: 0,
        key_ring_size: 1,
        key_derivation_algorithm: KeyDerivationAlgorithm::HKDF,
    });
    ensure!(
        provider.set_key(ID.into(), 0, key.to_vec()),
        "WebRTC refused the VP8 media key"
    );
    Ok(provider)
}

fn test_pattern(a_side: bool, index: usize) -> VideoFrame<I420Buffer> {
    let mut image = I420Buffer::new(WIDTH, HEIGHT);
    let (stride, _, _) = image.strides();
    let (y, u, v) = image.data_mut();
    for row in y.chunks_exact_mut(stride as usize) {
        let row = &mut row[..WIDTH as usize];
        let (left, right) = row.split_at_mut(WIDTH as usize / 2);
        left.fill(if a_side { 48 } else { 200 });
        right.fill(if a_side { 200 } else { 48 });
        row[index] = 110;
    }
    u.fill(128);
    v.fill(128);
    VideoFrame::new(VideoRotation::VideoRotation0, image)
}

fn matches_pattern(image: &I420Buffer, a_side: bool) -> bool {
    use libwebrtc::video_frame::VideoBuffer;
    let (width, height) = (image.width() as usize, image.height() as usize);
    if width < 8 || height < 2 {
        return false;
    }
    let (stride, _, _) = image.strides();
    let offset = stride as usize * (height / 2);
    let (y, _, _) = image.data();
    let (left, right) = (y[offset + width / 4], y[offset + width * 3 / 4]);
    if a_side {
        left < 85 && right > 150
    } else {
        left > 150 && right < 85
    }
}
