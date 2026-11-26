use anyhow::{Context, Result, ensure};
use libwebrtc::{
    MediaType,
    native::frame_cryptor::{
        EncryptionAlgorithm, FrameCryptor, KeyDerivationAlgorithm, KeyProvider, KeyProviderOptions,
    },
    peer_connection::{OfferOptions, PeerConnection, PeerConnectionState},
    peer_connection_factory::{IceServer as RtcIceServer, PeerConnectionFactory, RtcConfiguration},
    rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit},
    session_description::{SdpType, SessionDescription},
};
use std::time::Duration;
use tokio::{
    sync::mpsc,
    time::{self, timeout},
};
use tokio_tungstenite::Connector;
use vox_client::DevAddress;
use vox_protocol::{
    AttachMedia, CAP_FRAME_E2EE, CAP_SFU_TRANSPORT, GetTransportState, IceCandidate, RoomJoined,
    SdpAnswer, SdpOffer, TransportState,
};

use super::{EXCHANGE_TIMEOUT, Socket, exchange, open};
use crate::voice;

pub struct Connection {
    socket: Socket,
    peer: PeerConnection,
    candidates: mpsc::Receiver<IceCandidate>,
    generation: u64,
    next_request: u64,
    bundled_mid: Option<String>,
    receiver_guard: Option<FrameCryptor>,
    voice: Option<voice::Session>,
}

impl Connection {
    pub async fn connect(
        address: &DevAddress,
        room: RoomJoined,
        tls: Option<Connector>,
        voice_settings: Option<voice::Settings>,
    ) -> Result<Self> {
        ensure!(
            voice_settings.is_none() || room.allowed_tracks.contains(&vox_protocol::Track::Mic),
            "ticket does not allow a microphone track"
        );
        let url = address.verified_sfu_url(&room.sfu_url)?;
        let socket = open(url, tls, CAP_FRAME_E2EE | CAP_SFU_TRANSPORT).await?;
        let mut config = RtcConfiguration::default();
        config.ice_servers = room
            .ice
            .into_iter()
            .map(|server| RtcIceServer {
                urls: server.urls,
                username: server.username.unwrap_or_default(),
                password: server.credential.unwrap_or_default(),
            })
            .collect();
        let factory = PeerConnectionFactory::default();
        let peer = factory.create_peer_connection(config)?;
        let (tx, candidates) = mpsc::channel(64);
        let generation = room.generation;
        peer.on_ice_candidate(Some(Box::new(move |candidate| {
            if std::env::var_os("VOX_TRACE_ICE").is_some() {
                eprintln!("local ICE candidate for MID {}", candidate.sdp_mid());
            }
            let _ = tx.try_send(IceCandidate {
                generation,
                mid: candidate.sdp_mid(),
                candidate: candidate.to_string(),
            });
        })));
        if std::env::var_os("VOX_TRACE_ICE").is_some() {
            peer.on_connection_state_change(Some(Box::new(|state| {
                eprintln!("local ICE/DTLS state {state:?}")
            })));
            peer.on_ice_gathering_state_change(Some(Box::new(|state| {
                eprintln!("local ICE gathering {state:?}")
            })));
        }
        let mut connection = Self {
            socket,
            peer,
            candidates,
            generation,
            next_request: 3,
            bundled_mid: None,
            receiver_guard: None,
            voice: None,
        };

        let mut voice_settings = voice_settings;
        for (index, kind) in [
            MediaType::Audio,
            MediaType::Video,
            MediaType::Audio,
            MediaType::Video,
        ]
        .into_iter()
        .enumerate()
        {
            if index == 0
                && let Some(settings) = voice_settings.take()
            {
                connection.voice = Some(voice::Session::prepare(
                    &factory,
                    &connection.peer,
                    settings,
                )?);
                continue;
            }
            let transceiver = connection.peer.add_transceiver_for_media(
                kind,
                RtpTransceiverInit {
                    direction: if index == 0 {
                        RtpTransceiverDirection::RecvOnly
                    } else {
                        RtpTransceiverDirection::Inactive
                    },
                    stream_ids: vec![],
                    send_encodings: vec![],
                },
            )?;
            if index == 0 {
                // libwebrtc needs an active m-line. Guard the receiver before
                // negotiating, with zero provisioned media keys and no source.
                let keys = KeyProvider::new(KeyProviderOptions {
                    shared_key: false,
                    ratchet_window_size: 0,
                    ratchet_salt: Vec::new(),
                    failure_tolerance: -1,
                    key_ring_size: 1,
                    key_derivation_algorithm: KeyDerivationAlgorithm::HKDF,
                });
                let cryptor = FrameCryptor::new_for_rtp_receiver(
                    &factory,
                    "lab-transport".into(),
                    EncryptionAlgorithm::AesGcm,
                    keys,
                    transceiver.receiver(),
                );
                cryptor.set_enabled(true);
                ensure!(cryptor.enabled(), "receiver encryption guard unavailable");
                connection.receiver_guard = Some(cryptor);
            }
        }
        let local = create_offer(&connection.peer).await?;
        let offer = SdpOffer {
            generation,
            sdp: local.to_string(),
        };
        connection.peer.set_local_description(local).await?;
        let answer: SdpAnswer = exchange(
            &mut connection.socket,
            2,
            &AttachMedia {
                ticket: room.ticket,
                offer,
            },
        )
        .await?;
        ensure!(
            answer.generation == generation,
            "unexpected room generation in SDP answer"
        );
        connection.bundled_mid = answer
            .sdp
            .lines()
            .find_map(|line| line.strip_prefix("a=group:BUNDLE "))
            .and_then(|mids| mids.split_ascii_whitespace().next())
            .map(str::to_owned);
        connection
            .peer
            .set_remote_description(SessionDescription::parse(&answer.sdp, SdpType::Answer)?)
            .await?;
        if std::env::var_os("VOX_TRACE_ICE").is_some() {
            eprintln!(
                "applied SFU answer with {} ICE candidates",
                answer
                    .sdp
                    .lines()
                    .filter(|line| line.starts_with("a=candidate:"))
                    .count()
            );
        }
        let mut poll = time::interval(Duration::from_millis(150));
        loop {
            let status = connection.update(&mut poll).await?;
            if status.connected
                && connection.peer.connection_state() == PeerConnectionState::Connected
            {
                return Ok(connection);
            }
        }
    }

    pub async fn maintain(mut self) -> Result<()> {
        if let Some(mut voice) = self.voice.take() {
            tokio::select! {
                transport = self.maintain_transport() => transport,
                audio = voice.run() => audio,
            }
        } else {
            self.maintain_transport().await
        }
    }

    async fn maintain_transport(&mut self) -> Result<()> {
        let mut poll = time::interval(Duration::from_secs(2));
        loop {
            let status = self.update(&mut poll).await?;
            ensure!(
                status.connected && self.peer.connection_state() == PeerConnectionState::Connected,
                "ICE/DTLS disconnected"
            );
        }
    }

    async fn update(&mut self, poll: &mut time::Interval) -> Result<TransportState> {
        let id = self.next_request;
        self.next_request += 1;
        let state: TransportState = timeout(EXCHANGE_TIMEOUT, async {
            loop {
                tokio::select! {
                    candidate = self.candidates.recv() => {
                        let candidate = candidate.context("ICE candidate channel closed")?;
                        if self.bundled_mid.as_ref().is_none_or(|mid| *mid == candidate.mid) {
                            break exchange(&mut self.socket, id, &candidate).await;
                        }
                    }
                    _ = poll.tick() => break exchange(&mut self.socket, id, &GetTransportState { generation: self.generation }).await,
                }
            }
        }).await.context("SFU status timed out")??;
        ensure!(
            state.generation == self.generation,
            "unexpected room generation in SFU status"
        );
        ensure!(
            !matches!(
                self.peer.connection_state(),
                PeerConnectionState::Failed | PeerConnectionState::Closed
            ),
            "local ICE/DTLS failed"
        );
        Ok(state)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.peer.close();
    }
}

async fn create_offer(peer: &PeerConnection) -> Result<SessionDescription> {
    // Legacy offer-to-receive flags override the transceiver directions.
    Ok(peer
        .create_offer(OfferOptions {
            offer_to_receive_audio: None,
            offer_to_receive_video: None,
            ..OfferOptions::default()
        })
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn offer_keeps_bidirectional_microphone() -> Result<()> {
        let factory = PeerConnectionFactory::default();
        let peer = factory.create_peer_connection(RtcConfiguration::default())?;
        let _voice = voice::Session::prepare(
            &factory,
            &peer,
            voice::Settings {
                mode: voice::Mode::Tone,
                send: [1; 32],
                receive: [2; 32],
                a_side: true,
            },
        )?;
        let sdp = create_offer(&peer).await?.to_string();
        let microphone = sdp.split_once("m=audio ").unwrap().1;
        assert!(microphone.lines().any(|line| line == "a=sendrecv"));
        Ok(())
    }
}
