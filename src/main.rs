use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream,
    tungstenite::{self, Message as WsMessage, protocol::WebSocketConfig},
};
use vox_client::DevAddress;
use vox_protocol::*;

#[cfg(all(windows, feature = "lab", feature = "desktop", feature = "webrtc"))]
mod audio_targets;
#[cfg(all(windows, feature = "desktop"))]
mod desktop;
#[cfg(all(windows, feature = "lab", feature = "desktop", feature = "webrtc"))]
mod game_audio;
#[cfg(all(windows, feature = "webrtc"))]
mod media;
#[cfg(all(windows, feature = "lab", feature = "desktop", feature = "webrtc"))]
mod process_audio;
#[cfg(all(windows, feature = "webrtc"))]
mod video;
#[cfg(all(windows, feature = "webrtc"))]
mod voice;
#[cfg(all(windows, feature = "lab", feature = "desktop"))]
mod window_capture;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    room: String,
    device: String,
    credential: String,
    test_ca: Option<PathBuf>,
    voice: Option<MediaKeys>,
    video: Option<MediaKeys>,
    #[serde(rename = "share_audio")]
    _share_audio: Option<MediaKeys>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaKeys {
    send_key: String,
    receive_key: String,
}

impl MediaKeys {
    fn keys(&self) -> Result<([u8; 32], [u8; 32])> {
        let mut send = [0; 32];
        let mut receive = [0; 32];
        hex::decode_to_slice(&self.send_key, &mut send).context("invalid media send key")?;
        hex::decode_to_slice(&self.receive_key, &mut receive)
            .context("invalid media receive key")?;
        anyhow::ensure!(
            send != [0; 32] && receive != [0; 32] && send != receive,
            "media needs distinct, nonzero directional keys"
        );
        Ok((send, receive))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    #[cfg(all(windows, feature = "lab", feature = "desktop"))]
    if args == ["--capture-preview"] {
        desktop::capture_preview();
        return Ok(());
    }
    let [dev, server, address, lab, file, route, choice, options @ ..] = args.as_slice() else {
        anyhow::bail!(
            "usage: vox-client --dev --server HOST[:PORT] --lab FILE --media-route sfu [--audio tone|mic] [--video test|share]"
        );
    };
    anyhow::ensure!(
        cfg!(feature = "lab")
            && dev == "--dev"
            && server == "--server"
            && lab == "--lab"
            && route == "--media-route",
        "this command requires the lab feature and explicit dev flags"
    );
    anyhow::ensure!(choice == "sfu", "this build only supports the SFU route");
    let mut audio = None;
    let mut video = None;
    let mut options = options;
    while !options.is_empty() {
        options = match options {
            [flag, mode, tail @ ..]
                if flag == "--audio"
                    && audio.is_none()
                    && matches!(mode.as_str(), "tone" | "mic") =>
            {
                audio = Some(mode.as_str());
                tail
            }
            [flag, mode, tail @ ..]
                if flag == "--video"
                    && matches!(mode.as_str(), "test" | "share")
                    && video.is_none() =>
            {
                video = Some(mode.as_str());
                tail
            }
            _ => anyhow::bail!("expected [--audio tone|mic] [--video test|share]"),
        };
    }
    let address = DevAddress::parse(address)?;
    let (profile, tls) = load_lab_profile(Path::new(file))?;
    let video_keys = video
        .map(|mode| {
            anyhow::ensure!(
                cfg!(all(windows, feature = "webrtc")),
                "video requires native WebRTC"
            );
            anyhow::ensure!(
                mode != "share" || cfg!(feature = "desktop"),
                "screen share requires the desktop feature"
            );
            profile
                .video
                .as_ref()
                .context("video needs vox-lab init --video")?
                .keys()
        })
        .transpose()?;
    let voice_keys = audio
        .map(|_| {
            anyhow::ensure!(
                cfg!(all(windows, feature = "webrtc")),
                "voice requires a native WebRTC build"
            );
            profile
                .voice
                .as_ref()
                .context("voice needs a profile generated with vox-lab init --voice")?
                .keys()
        })
        .transpose()?;
    #[cfg(all(windows, feature = "webrtc"))]
    let a_side = profile.device == "a";
    #[cfg(all(windows, feature = "webrtc", feature = "desktop", feature = "lab"))]
    let share_audio_keys = profile
        ._share_audio
        .as_ref()
        .filter(|_| video == Some("share"))
        .map(MediaKeys::keys)
        .transpose()?;

    let reply = timeout(EXCHANGE_TIMEOUT, join(&address, profile, tls.clone()))
        .await
        .context("Vox signaling timed out")??;
    #[cfg(all(windows, feature = "webrtc"))]
    {
        #[cfg(all(feature = "desktop", feature = "lab"))]
        let (desktop_feed, pipeline) = if video == Some("share") {
            let (ui, video) = video::share::channel();
            (Some(ui), Some(video))
        } else {
            (None, None)
        };
        #[cfg(all(feature = "desktop", feature = "lab"))]
        let (game_audio_ui, game_audio) =
            match share_audio_keys.filter(|_| reply.allowed_tracks.contains(&Track::ShareAudio)) {
                Some((send, receive)) => {
                    let (ui, settings) = game_audio::channel(send, receive);
                    (Some(ui), Some(settings))
                }
                None => (None, None),
            };
        let connection = timeout(
            EXCHANGE_TIMEOUT,
            media::Connection::connect(
                &address,
                reply,
                tls,
                audio
                    .zip(voice_keys)
                    .map(|(mode, (send, receive))| voice::Settings {
                        mode: if mode == "mic" {
                            voice::Mode::Mic
                        } else {
                            voice::Mode::Tone
                        },
                        send,
                        receive,
                        a_side,
                    }),
                video_keys.map(|(send, receive)| video::Settings {
                    send,
                    receive,
                    a_side,
                    #[cfg(all(feature = "desktop", feature = "lab"))]
                    share: pipeline,
                    #[cfg(all(feature = "desktop", feature = "lab"))]
                    audio: game_audio,
                }),
            ),
        )
        .await
        .context("ICE/DTLS setup timed out")??;
        println!(
            "DEV {} | SFU ICE/DTLS connected | {}",
            address.authority(),
            audio.map_or(
                if video.is_some() {
                    "E2EE VP8 video"
                } else {
                    "media disabled"
                },
                |mode| if mode == "mic" {
                    "E2EE microphone"
                } else {
                    "E2EE test tone"
                }
            ),
        );
        #[cfg(feature = "desktop")]
        {
            let task = tokio::spawn(async move {
                if let Err(error) = connection.maintain().await {
                    eprintln!("Vox media transport closed: {error}");
                }
            });
            #[cfg(feature = "lab")]
            if let Some(feed) = desktop_feed {
                desktop::show_share(address.authority(), feed, game_audio_ui);
                task.abort();
                return Ok(());
            }
            desktop::show(
                address.authority(),
                if video.is_some() {
                    "ICE/DTLS с SFU установлен. Лабораторное видео VP8 с E2EE."
                } else if audio.is_some() {
                    "ICE/DTLS с SFU установлен. Лабораторный аудиорежим E2EE."
                } else {
                    "ICE/DTLS с SFU установлен. Передача медиа отключена."
                }
                .into(),
            );
            task.abort();
        }
        #[cfg(not(feature = "desktop"))]
        connection.maintain().await?;
    }
    #[cfg(not(all(windows, feature = "webrtc")))]
    {
        debug_assert!(voice_keys.is_none());
        debug_assert!(video_keys.is_none());
        println!(
            "DEV {} | signaling joined generation {} | media unavailable",
            address.authority(),
            reply.generation
        );
        #[cfg(all(windows, feature = "desktop"))]
        desktop::show(
            address.authority(),
            format!(
                "Сигнализация подключена. Поколение комнаты {}. Медиа недоступно.",
                reply.generation
            ),
        );
    }
    Ok(())
}

fn load_lab_profile(file: &Path) -> Result<(Profile, Option<Connector>)> {
    let profile: Profile =
        toml::from_str(&fs::read_to_string(file).context("reading lab profile")?)
            .map_err(|_| anyhow::anyhow!("invalid lab profile"))?;
    let tls = profile
        .test_ca
        .as_ref()
        .map(|ca| {
            let base = file.parent().unwrap_or_else(|| Path::new("."));
            test_connector(&base.join(ca))
        })
        .transpose()?;
    Ok((profile, tls))
}

fn test_connector(file: &Path) -> Result<Connector> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut BufReader::new(
        fs::File::open(file).context("opening test CA")?,
    )) {
        roots.add(cert?)?;
    }
    anyhow::ensure!(
        !roots.is_empty(),
        "test CA file does not contain certificates"
    );
    Ok(Connector::Rustls(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )))
}

async fn join(
    address: &DevAddress,
    profile: Profile,
    tls: Option<Connector>,
) -> Result<RoomJoined> {
    let mut socket = open(
        format!("wss://{}/v1/ws", address.authority()),
        tls,
        CAP_FRAME_E2EE,
    )
    .await?;
    let join = JoinRoom {
        room: profile.room,
        device: profile.device,
        credential: profile.credential,
    };
    exchange(&mut socket, 2, &join).await
}

async fn open(url: String, tls: Option<Connector>, capabilities: u64) -> Result<Socket> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    let (mut socket, _) =
        tokio_tungstenite::connect_async_tls_with_config(url, Some(config), true, tls).await?;
    let hello = ClientHello {
        min_minor: MINOR,
        max_minor: MINOR,
        capabilities,
        required: capabilities,
    };
    let reply: ServerHello = exchange(&mut socket, 1, &hello).await?;
    anyhow::ensure!(
        reply.minor == MINOR && reply.capabilities & capabilities == capabilities,
        "server did not negotiate required capabilities"
    );
    Ok(socket)
}

async fn exchange<M: Message, R: Message>(socket: &mut Socket, id: u64, request: &M) -> Result<R> {
    socket
        .send(WsMessage::Binary(encode(MINOR, id, request)?.into()))
        .await?;
    response(&receive_binary(socket).await?, id)
}

async fn receive_binary(socket: &mut Socket) -> Result<tungstenite::Bytes> {
    loop {
        match socket.next().await {
            Some(Ok(WsMessage::Binary(bytes))) => return Ok(bytes),
            Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => {}
            Some(Err(error)) => return Err(error.into()),
            _ => anyhow::bail!("expected a binary signaling reply"),
        }
    }
}

fn response<M: Message>(bytes: &[u8], request_id: u64) -> Result<M> {
    let frame = decode(bytes)?;
    anyhow::ensure!(
        frame.major == MAJOR && frame.minor == MINOR && frame.request_id == request_id,
        "unexpected signaling reply"
    );
    if frame.kind == ProtocolError::KIND {
        anyhow::bail!(
            "server rejected request: {:?}",
            frame.body::<ProtocolError>()?.code
        );
    }
    Ok(frame.body::<M>()?)
}
