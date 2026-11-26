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

#[cfg(all(windows, feature = "desktop"))]
mod desktop;
#[cfg(all(windows, feature = "webrtc"))]
mod media;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    room: String,
    device: String,
    credential: String,
    test_ca: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [dev, server, address, lab, file, route, choice] = args.as_slice() else {
        anyhow::bail!("usage: vox-client --dev --server HOST[:PORT] --lab FILE --media-route sfu");
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
    let address = DevAddress::parse(address)?;
    let file = Path::new(file);
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

    let reply = timeout(EXCHANGE_TIMEOUT, join(&address, profile, tls.clone()))
        .await
        .context("Vox signaling timed out")??;
    #[cfg(all(windows, feature = "webrtc"))]
    {
        let connection = timeout(
            EXCHANGE_TIMEOUT,
            media::Connection::connect(&address, reply, tls),
        )
        .await
        .context("ICE/DTLS setup timed out")??;
        println!(
            "DEV {} | SFU ICE/DTLS connected | media disabled",
            address.authority()
        );
        #[cfg(feature = "desktop")]
        {
            let task = tokio::spawn(async move {
                if let Err(error) = connection.maintain().await {
                    eprintln!("Vox media transport closed: {error}");
                }
            });
            desktop::show(
                address.authority(),
                "ICE/DTLS с SFU установлен. Передача медиа отключена.".into(),
            );
            task.abort();
        }
        #[cfg(not(feature = "desktop"))]
        connection.maintain().await?;
    }
    #[cfg(not(all(windows, feature = "webrtc")))]
    {
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
