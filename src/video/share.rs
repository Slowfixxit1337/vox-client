use crate::window_capture::Frame;
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use libwebrtc::{
    native::yuv_helper::argb_to_i420,
    video_frame::{
        I420Buffer, VideoBuffer, VideoFormatType, VideoFrame, VideoRotation,
        native::VideoFrameBufferExt,
    },
};
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, time};

const MAX_CAPTURE_PIXELS: usize = 4096 * 2160;
const MAX_ENCODE_WIDTH: u32 = 1280;
const MAX_ENCODE_HEIGHT: u32 = 720;
const STALE_IMAGE: Duration = Duration::from_secs(2);
const STATIC_REFRESH: Duration = Duration::from_millis(500);

type LatestFrame = Option<Arc<Frame>>;

pub struct DesktopFeed {
    pub capture: watch::Sender<LatestFrame>,
    pub received: watch::Receiver<LatestFrame>,
}

pub struct Pipeline {
    source: watch::Receiver<LatestFrame>,
    decoded: watch::Sender<LatestFrame>,
}

pub fn channel() -> (DesktopFeed, Pipeline) {
    let (capture, source) = watch::channel(None);
    let (decoded, received) = watch::channel(None);
    (
        DesktopFeed { capture, received },
        Pipeline { source, decoded },
    )
}

impl super::Session {
    pub(super) async fn run_share(&mut self, mut share: Pipeline) -> Result<()> {
        self.set_share_active(false)?;
        let stale = time::sleep(STALE_IMAGE);
        tokio::pin!(stale);
        let mut displayed_size = None;
        let mut cached = None;
        let mut refresh = time::interval(STATIC_REFRESH);
        refresh.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                changed = share.source.changed() => {
                    changed.context("Vox capture window closed")?;
                    let frame = share.source.borrow_and_update().clone();
                    if let Some(frame) = frame {
                        if frame.width < 2 || frame.height < 2 {
                            continue;
                        }
                        let frame = encode_frame(&frame)?;
                        // Stop may have arrived during CPU readback conversion.
                        if share.source.borrow().is_some() {
                            if cached.is_none() {
                                // Retire libwebrtc's startup black frames while RTP is inactive.
                                let _ = self.source.capture_frame(&frame);
                                self.set_share_active(true)?;
                                println!("E2EE ShareVideo source: {}x{}", frame.buffer.width(), frame.buffer.height());
                            }
                            let _ = self.source.capture_frame(&frame);
                            cached = Some(frame);
                            refresh.reset();
                        }
                    } else {
                        if cached.take().is_some() {
                            #[cfg(test)]
                            eprintln!("Disabling ShareVideo encoding");
                            self.set_share_active(false)?;
                            #[cfg(test)]
                            eprintln!("ShareVideo encoding disabled");
                        }
                    }
                }
                _ = refresh.tick(), if cached.is_some() => {
                    // WGC and DXGI can be silent while an already shared image is static.
                    if let Some(frame) = &cached && share.source.borrow().is_some() {
                        let _ = self.source.capture_frame(frame);
                    }
                }
                incoming = self.remote.next() => {
                    let incoming = incoming.context("VP8 receiver closed")?;
                    let frame = decode_frame(incoming.buffer.as_ref())?;
                    let size = (frame.width, frame.height);
                    if displayed_size != Some(size) {
                        println!("E2EE ShareVideo received: {}x{}", frame.width, frame.height);
                    }
                    displayed_size = Some(size);
                    stale.as_mut().reset(time::Instant::now() + STALE_IMAGE);
                    share.decoded.send_replace(Some(Arc::new(frame)));
                }
                _ = &mut stale, if displayed_size.is_some() => {
                    displayed_size = None;
                    share.decoded.send_replace(None);
                }
                error = self.errors.recv() => {
                    let (sending, state) = error.context("VP8 cryptor observer closed")?;
                    ensure!(!sending, "E2EE VP8 sender failed: {state:?}");
                    displayed_size = None;
                    share.decoded.send_replace(None);
                    eprintln!("E2EE VP8 receiver rejected video: {state:?}");
                }
            }
        }
    }
    fn set_share_active(&self, active: bool) -> Result<()> {
        let mut parameters = self.rtp_sender.parameters();
        let [encoding] = parameters.encodings.as_mut_slice() else {
            anyhow::bail!("ShareVideo requires a negotiated single VP8 encoding");
        };
        encoding.active = active;
        self.rtp_sender
            .set_parameters(parameters)
            .context("setting ShareVideo activity")?;
        self.local.set_enabled(active);
        Ok(())
    }
}

fn frame_len(width: u32, height: u32) -> Result<usize> {
    let area = (width as usize)
        .checked_mul(height as usize)
        .context("video dimensions overflow")?;
    ensure!(
        area != 0 && area <= MAX_CAPTURE_PIXELS,
        "unsupported video dimensions"
    );
    Ok(area * 4)
}

fn encode_frame(frame: &Frame) -> Result<VideoFrame<I420Buffer>> {
    ensure!(
        frame.bgra.len() == frame_len(frame.width, frame.height)?,
        "incomplete BGRA frame"
    );
    let width = frame.width & !1;
    let height = frame.height & !1;
    ensure!(width >= 2 && height >= 2, "video frame is too small");
    let mut i420 = I420Buffer::new(width, height);
    let (stride_y, stride_u, stride_v) = i420.strides();
    let (y, u, v) = i420.data_mut();
    // libyuv ARGB is BGRA in little-endian memory, matching Windows D3D11.
    argb_to_i420(
        &frame.bgra,
        frame.width * 4,
        y,
        stride_y,
        u,
        stride_u,
        v,
        stride_v,
        width as i32,
        height as i32,
    );
    let scale = (f64::from(MAX_ENCODE_WIDTH) / f64::from(width))
        .min(f64::from(MAX_ENCODE_HEIGHT) / f64::from(height));
    if scale < 1.0 {
        let width = ((f64::from(width) * scale) as i32 & !1).max(2);
        let height = ((f64::from(height) * scale) as i32 & !1).max(2);
        i420 = i420.scale(width, height);
    }
    Ok(VideoFrame::new(VideoRotation::VideoRotation0, i420))
}

fn decode_frame(buffer: &dyn VideoBuffer) -> Result<Frame> {
    let i420 = buffer
        .as_i420()
        .context("unsupported decoded VP8 pixel format")?;
    let (width, height) = (buffer.width(), buffer.height());
    let mut bgra = vec![0; frame_len(width, height)?];
    i420.to_argb(
        VideoFormatType::ARGB,
        &mut bgra,
        width * 4,
        width as i32,
        height as i32,
    );
    Ok(Frame {
        width,
        height,
        bgra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        join, load_lab_profile,
        media::Connection,
        video::Settings,
        window_capture::tests::{TEST_BGR, TestWindow},
    };
    use std::{path::Path, thread};
    use vox_client::DevAddress;

    #[test]
    fn round_trip_crops_odd_edges_and_keeps_bgra_channel_order() -> anyhow::Result<()> {
        let source = Frame {
            width: 3,
            height: 3,
            bgra: [12, 42, 210, 255].repeat(9),
        };
        let converted = encode_frame(&source)?;
        let preview = decode_frame(converted.buffer.as_ref())?;
        assert_eq!((preview.width, preview.height), (2, 2));
        assert!(preview.bgra.as_chunks::<4>().0.iter().all(|p| {
            p[0].abs_diff(12) < 8 && p[1].abs_diff(42) < 8 && p[2].abs_diff(210) < 8 && p[3] == 255
        }));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires an interactive Windows desktop and a provisioned Vox SFU"]
    async fn shares_owned_wgc_window_over_sfu_and_stops() -> Result<()> {
        let address = DevAddress::parse(&std::env::var("VOX_TEST_SERVER")?)?;
        let a_profile = std::env::var("VOX_TEST_PROFILE_A")?;
        let b_profile = std::env::var("VOX_TEST_PROFILE_B")?;
        let ((a, a_feed), (b, mut b_feed)) = time::timeout(Duration::from_secs(15), async {
            tokio::try_join!(connect(&address, &a_profile), connect(&address, &b_profile))
        })
        .await??;
        let mut a = tokio::spawn(a.maintain());
        let mut b = tokio::spawn(b.maintain());
        let result = tokio::select! {
            result = prove_window_feed(a_feed, &mut b_feed) => result,
            result = &mut a => anyhow::bail!("publisher disconnected: {result:?}"),
            result = &mut b => anyhow::bail!("receiver disconnected: {result:?}"),
        };
        a.abort();
        b.abort();
        result
    }

    async fn connect(address: &DevAddress, file: &str) -> Result<(Connection, DesktopFeed)> {
        let (profile, tls) = load_lab_profile(Path::new(file))?;
        let (send, receive) = profile
            .video
            .as_ref()
            .context("missing test video keys")?
            .keys()?;
        let a_side = profile.device == "a";
        let room = join(address, profile, tls.clone()).await?;
        let (feed, pipeline) = channel();
        let connection = Connection::connect(
            address,
            room,
            tls,
            None,
            Some(Settings {
                send,
                receive,
                a_side,
                share: Some(pipeline),
                audio: None,
            }),
        )
        .await?;
        Ok((connection, feed))
    }

    async fn prove_window_feed(a: DesktopFeed, b: &mut DesktopFeed) -> Result<()> {
        ensure!(
            time::timeout(
                Duration::from_millis(400),
                b.received.wait_for(Option::is_some)
            )
            .await
            .is_err(),
            "a video frame was published before selecting a source"
        );
        let (tx, rx) = async_channel::bounded(1);
        let fixture = thread::spawn(move || {
            let result = (|| -> Result<()> {
                let mut window = TestWindow::new()?;
                let capture = window.capture()?;
                let mut alternate = false;
                while !tx.is_closed() {
                    alternate = !alternate;
                    window.animate(alternate)?;
                    if let Ok(frame) = capture.try_recv() {
                        let _ = tx.force_send(frame);
                    }
                }
                window.close_and_wait(&capture)
            })();
            if let Err(error) = result {
                let _ = tx.force_send(Err(error));
            }
        });
        let expected = time::timeout(Duration::from_secs(5), async {
            loop {
                let frame = rx.recv().await??;
                if shows_fixture_color(&frame) {
                    return Ok::<_, anyhow::Error>(frame);
                }
            }
        })
        .await
        .context("WGC did not capture the fixture color")??;
        let (width, height) = (expected.width & !1, expected.height & !1);
        let first = Arc::new(expected);
        a.capture.send_replace(Some(first.clone()));
        time::timeout(Duration::from_secs(12), async {
            loop {
                tokio::select! {
                    frame = rx.recv() => {
                        a.capture.send_replace(Some(Arc::new(frame??)));
                    }
                    changed = b.received.changed() => {
                        changed?;
                        let frame = b.received.borrow_and_update();
                        if let Some(frame) = &*frame && same_capture_size(frame, width, height) && shows_fixture_color(frame) {
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                }
            }
        }).await.context("viewer did not decode the captured window")??;
        eprintln!("Viewer decoded the fixture color");
        a.capture.send_replace(None);
        time::timeout(Duration::from_secs(5), async {
            while b.received.borrow().is_some() {
                b.received.changed().await?;
                b.received.borrow_and_update();
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("viewer retained the image after sharing stopped")??;
        eprintln!("Viewer cleared the captured image");
        // Reuse the same negotiated transceiver and let its static refresh keep the frame live.
        a.capture.send_replace(Some(first));
        time::timeout(
            Duration::from_secs(5),
            b.received.wait_for(|frame| {
                frame.as_ref().is_some_and(|frame| {
                    same_capture_size(frame, width, height) && shows_fixture_color(frame)
                })
            }),
        )
        .await
        .context("viewer did not recover after restarting the share")??;
        time::sleep(STALE_IMAGE + Duration::from_millis(250)).await;
        ensure!(
            b.received.borrow().is_some(),
            "viewer forgot the still-shared static image"
        );
        drop(rx);
        fixture
            .join()
            .map_err(|_| anyhow::anyhow!("WGC fixture thread panicked"))?;
        Ok(())
    }

    fn same_capture_size(frame: &Frame, width: u32, height: u32) -> bool {
        // The native WebRTC adapter may crop up to a few pixels for encoder alignment.
        frame.width.abs_diff(width) < 8 && frame.height.abs_diff(height) < 8
    }

    fn shows_fixture_color(frame: &Frame) -> bool {
        let offset = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
        frame.bgra[offset..offset + 3]
            .iter()
            .zip(TEST_BGR)
            .all(|(got, want)| got.abs_diff(want) < 24)
    }
}
