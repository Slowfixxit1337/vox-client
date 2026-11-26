use anyhow::{Context, Result};

#[cfg(all(windows, feature = "desktop"))]
mod desktop;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{fs, time::Duration};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{self, Message as WsMessage, protocol::WebSocketConfig};
use vox_client::DevAddress;
use vox_protocol::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    room: String,
    device: String,
    credential: String,
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
    anyhow::ensure!(
        choice == "sfu",
        "only SFU signaling is available in this build"
    );
    let address = DevAddress::parse(address)?;
    let profile: Profile =
        toml::from_str(&fs::read_to_string(file).context("reading lab profile")?)
            .map_err(|_| anyhow::anyhow!("invalid lab profile"))?;
    let reply = timeout(Duration::from_secs(10), join(&address, profile))
        .await
        .context("Vox signaling timed out")??;
    #[cfg(all(windows, feature = "desktop"))]
    desktop::show(
        address.authority(),
        format!(
            "Сигнализация подключена. Поколение комнаты {}. Медиа недоступно.",
            reply.generation
        ),
    );
    #[cfg(not(all(windows, feature = "desktop")))]
    println!(
        "DEV {} | signaling joined generation {} | media unavailable",
        address.authority(),
        reply.generation
    );
    Ok(())
}

async fn join(address: &DevAddress, profile: Profile) -> Result<RoomJoined> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    let url = format!("wss://{}/v1/ws", address.authority());
    let (mut socket, _) =
        tokio_tungstenite::connect_async_with_config(url, Some(config), false).await?;
    let hello = ClientHello {
        min_minor: MINOR,
        max_minor: MINOR,
        capabilities: CAP_FRAME_E2EE,
        required: CAP_FRAME_E2EE,
    };
    socket
        .send(WsMessage::Binary(encode(MINOR, 1, &hello)?.into()))
        .await?;
    let bytes = receive_binary(&mut socket).await?;
    let server_hello: ServerHello = response(&bytes, 1)?;
    anyhow::ensure!(
        server_hello.minor == MINOR && server_hello.capabilities & CAP_FRAME_E2EE != 0,
        "server did not negotiate mandatory E2EE"
    );
    let join = JoinRoom {
        room: profile.room,
        device: profile.device,
        credential: profile.credential,
    };
    socket
        .send(WsMessage::Binary(encode(MINOR, 2, &join)?.into()))
        .await?;
    let bytes = receive_binary(&mut socket).await?;
    response(&bytes, 2)
}

async fn receive_binary(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<tungstenite::Bytes> {
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
