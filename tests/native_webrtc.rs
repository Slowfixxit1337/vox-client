#![cfg(all(windows, feature = "webrtc"))]

use libwebrtc::{
    audio_source::{AudioSourceOptions, native::NativeAudioSource},
    media_stream_track::MediaStreamTrack,
    native::frame_cryptor::{
        EncryptionAlgorithm, FrameCryptor, KeyDerivationAlgorithm, KeyProvider, KeyProviderOptions,
    },
    peer_connection::OfferOptions,
    peer_connection_factory::{
        PeerConnectionFactory, RtcConfiguration, native::PeerConnectionFactoryExt,
    },
    rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit},
    video_source::{VideoResolution, native::NativeVideoSource},
};

#[tokio::test]
async fn native_factory_builds_signaling_with_frame_cryptors() {
    let factory = PeerConnectionFactory::default();
    let peer = factory
        .create_peer_connection(RtcConfiguration::default())
        .unwrap();
    let provider = KeyProvider::new(KeyProviderOptions {
        shared_key: false,
        ratchet_window_size: 0,
        ratchet_salt: b"vox-synthetic-fixture".to_vec(),
        failure_tolerance: -1,
        key_ring_size: 1,
        key_derivation_algorithm: KeyDerivationAlgorithm::HKDF,
    });
    assert!(provider.set_key("fixture".into(), 0, vec![1; 32]));

    let mut cryptors = Vec::new();
    for (label, audio) in [
        ("mic", true),
        ("share-video", false),
        ("share-audio", true),
        ("camera", false),
    ] {
        // The native sender constructor requires a track. Keep synthetic
        // sources disabled and the SDP unapplied until transforms are ready.
        let track: MediaStreamTrack = if audio {
            factory
                .create_audio_track(
                    label,
                    NativeAudioSource::new(AudioSourceOptions::default(), 48_000, 1, 20),
                )
                .into()
        } else {
            factory
                .create_video_track(
                    label,
                    NativeVideoSource::new_encoded(VideoResolution {
                        width: 640,
                        height: 360,
                    }),
                )
                .into()
        };
        track.set_enabled(false);
        let transceiver = peer
            .add_transceiver(
                track,
                RtpTransceiverInit {
                    direction: RtpTransceiverDirection::SendRecv,
                    stream_ids: vec![],
                    send_encodings: vec![],
                },
            )
            .unwrap();
        let sender = FrameCryptor::new_for_rtp_sender(
            &factory,
            "fixture".into(),
            EncryptionAlgorithm::AesGcm,
            provider.clone(),
            transceiver.sender(),
        );
        let receiver = FrameCryptor::new_for_rtp_receiver(
            &factory,
            "fixture".into(),
            EncryptionAlgorithm::AesGcm,
            provider.clone(),
            transceiver.receiver(),
        );
        for cryptor in [&sender, &receiver] {
            cryptor.set_key_index(0);
            cryptor.set_enabled(true);
            assert!(cryptor.enabled());
        }
        cryptors.push((sender, receiver));
    }
    let offer = peer
        .create_offer(OfferOptions::default())
        .await
        .unwrap()
        .to_string();
    assert_eq!(
        offer
            .lines()
            .filter(|line| line.starts_with("m=audio "))
            .count(),
        2
    );
    assert_eq!(
        offer
            .lines()
            .filter(|line| line.starts_with("m=video "))
            .count(),
        2
    );
    assert_eq!(cryptors.len(), 4);
    peer.close();
}
