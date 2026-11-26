use anyhow::{Context, Result, ensure};
use async_channel::{Receiver, Sender};
use std::{
    slice,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{
            Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
            GraphicsCaptureItem, GraphicsCapturePicker, GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus,
    Win32::{
        Foundation::HWND,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN},
            Direct3D11::*,
            Dxgi::{
                Common::{DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED},
                CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIDevice, IDXGIFactory1, IDXGIOutput,
                IDXGIOutput1, IDXGIOutputDuplication,
            },
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
        },
        UI::Shell::IInitializeWithWindow,
    },
    core::Interface,
};

const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const MAX_FRAME_PIXELS: usize = 4096 * 2160;

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub enum Source {
    Window {
        item: GraphicsCaptureItem,
        borderless: bool,
    },
    Monitor(String),
}

struct Output {
    name: String,
    adapter: IDXGIAdapter,
    output: IDXGIOutput,
}

pub fn monitors() -> Result<Vec<String>> {
    let mut names: Vec<_> = outputs()?.into_iter().map(|output| output.name).collect();
    names.sort();
    Ok(names)
}

fn outputs() -> Result<Vec<Output>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut outputs = Vec::new();
    for index in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error.into()),
        };
        for index in 0.. {
            let output = match unsafe { adapter.EnumOutputs(index) } {
                Ok(output) => output,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };
            let desc = unsafe { output.GetDesc()? };
            if desc.AttachedToDesktop.as_bool() {
                let name = String::from_utf16_lossy(
                    desc.DeviceName
                        .split(|ch| *ch == 0)
                        .next()
                        .unwrap_or_default(),
                );
                outputs.push(Output {
                    name,
                    adapter: adapter.cast()?,
                    output,
                });
            }
        }
    }
    Ok(outputs)
}

pub async fn pick(owner: HWND) -> Result<Option<GraphicsCaptureItem>> {
    ensure!(
        GraphicsCaptureSession::IsSupported()?,
        "захват Windows недоступен"
    );
    let picker = GraphicsCapturePicker::new()?;
    // The handle is borrowed from the live Vox window on the UI thread.
    unsafe { picker.cast::<IInitializeWithWindow>()?.Initialize(owner)? };
    match picker.PickSingleItemAsync()?.await {
        Ok(item) => Ok(Some(item)),
        Err(error) if matches!(error.code().0 as u32, 0 | 0x8007_04c7) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub async fn request_borderless_window() -> Result<()> {
    let status = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
        .context("для окна без рамки требуется пакет Vox с поддержкой этого режима")?
        .await
        .context("Windows не предоставила доступ к захвату окна без рамки")?;
    ensure!(
        status == AppCapabilityAccessStatus::Allowed,
        "Windows не разрешила захват окна без рамки"
    );
    Ok(())
}

pub fn frames(source: Source) -> Result<Receiver<Result<Frame>>> {
    let (tx, rx) = async_channel::bounded(1);
    thread::Builder::new()
        .name("vox-window-capture".into())
        .spawn(move || {
            if let Err(error) = run(source, &tx) {
                let _ = tx.force_send(Err(error));
            }
        })?;
    Ok(rx)
}

fn run(source: Source, tx: &Sender<Result<Frame>>) -> Result<()> {
    // All D3D device/context operations happen on this COM-initialized thread.
    unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
    let _apartment = Apartment;
    match source {
        Source::Window { item, borderless } => capture_window(item, borderless, tx),
        Source::Monitor(name) => capture_monitor(&name, tx),
    }
}

fn capture_window(
    item: GraphicsCaptureItem,
    borderless: bool,
    tx: &Sender<Result<Frame>>,
) -> Result<()> {
    let d3d = Readback::new(None)?;
    let dxgi: IDXGIDevice = d3d.device.cast()?;
    let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
    let mut size = item.Size()?;
    validate_size(size)?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device, FORMAT, 2, size)?;
    let session = pool.CreateCaptureSession(&item)?;
    if borderless {
        session
            .SetIsBorderRequired(false)
            .context("эта версия Windows не поддерживает захват окна без рамки")?;
    }
    let closed = Arc::new(AtomicBool::new(false));
    let signal = closed.clone();
    let token = item.Closed(&TypedEventHandler::new(move |_, _| {
        signal.store(true, Ordering::Relaxed);
        Ok(())
    }))?;
    let result = (|| {
        session.StartCapture()?;
        let mut staging = None;
        while !tx.is_closed() && !closed.load(Ordering::Relaxed) {
            let frame = match pool.TryGetNextFrame() {
                Ok(frame) => frame,
                Err(error) if error.code().0 == 0 => {
                    thread::sleep(FRAME_INTERVAL);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let content = frame.ContentSize()?;
            if content.Width > 0 && content.Height > 0 {
                if content.Width != size.Width || content.Height != size.Height {
                    validate_size(content)?;
                    frame.Close()?;
                    pool.Recreate(&device, FORMAT, 2, content)?;
                    size = content;
                    staging = None;
                    continue;
                }
                let texture: ID3D11Texture2D = unsafe {
                    frame
                        .Surface()?
                        .cast::<IDirect3DDxgiInterfaceAccess>()?
                        .GetInterface()?
                };
                let Some(bgra) = d3d.copy(&texture, size, &mut staging)? else {
                    frame.Close()?;
                    continue;
                };
                if tx
                    .force_send(Ok(Frame {
                        width: size.Width as u32,
                        height: size.Height as u32,
                        bgra,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            frame.Close()?;
            thread::sleep(FRAME_INTERVAL);
        }
        Ok(())
    })();
    let _ = item.RemoveClosed(token);
    let _ = session.Close();
    let _ = pool.Close();
    result
}

fn capture_monitor(name: &str, tx: &Sender<Result<Frame>>) -> Result<()> {
    let target = outputs()?
        .into_iter()
        .find(|output| output.name == name)
        .context("выбранный монитор отключён")?;
    // Desktop Duplication requires the D3D device to use the output's adapter.
    let d3d = Readback::new(Some(&target.adapter))?;
    let output: IDXGIOutput1 = target.output.cast()?;
    let duplication = unsafe { output.DuplicateOutput(&d3d.device)? };
    let desc = unsafe { duplication.GetDesc() };
    ensure!(
        matches!(
            desc.Rotation,
            DXGI_MODE_ROTATION_IDENTITY | DXGI_MODE_ROTATION_UNSPECIFIED
        ),
        "в этом режиме пока поддерживается горизонтальная ориентация экрана"
    );
    let size = SizeInt32 {
        Width: desc.ModeDesc.Width.try_into()?,
        Height: desc.ModeDesc.Height.try_into()?,
    };
    validate_size(size)?;
    let mut staging = None;
    while !tx.is_closed() {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        match unsafe { duplication.AcquireNextFrame(33, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
            Err(error) => return Err(error).context("захват монитора прерван"),
        }
        {
            let _frame = DesktopFrame(&duplication);
            let texture = resource.context("DXGI не вернул кадр монитора")?.cast()?;
            let bgra = d3d
                .copy(&texture, size, &mut staging)?
                .context("размер кадра монитора изменился")?;
            if tx
                .force_send(Ok(Frame {
                    width: size.Width as u32,
                    height: size.Height as u32,
                    bgra,
                }))
                .is_err()
            {
                break;
            }
        }
        thread::sleep(FRAME_INTERVAL);
    }
    Ok(())
}

struct DesktopFrame<'a>(&'a IDXGIOutputDuplication);

impl Drop for DesktopFrame<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.0.ReleaseFrame() };
    }
}

fn validate_size(size: SizeInt32) -> Result<()> {
    ensure!(
        size.Width > 0
            && size.Height > 0
            && (size.Width as usize).saturating_mul(size.Height as usize) <= MAX_FRAME_PIXELS,
        "размер источника выходит за пределы предпросмотра"
    );
    Ok(())
}

struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

struct Readback {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

impl Readback {
    fn new(adapter: Option<&IDXGIAdapter>) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                adapter,
                if adapter.is_some() {
                    D3D_DRIVER_TYPE_UNKNOWN
                } else {
                    D3D_DRIVER_TYPE_HARDWARE
                },
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        Ok(Self {
            device: device.context("D3D11 не вернул устройство")?,
            context: context.context("D3D11 не вернул контекст")?,
        })
    }

    fn copy(
        &self,
        texture: &ID3D11Texture2D,
        size: SizeInt32,
        staging: &mut Option<ID3D11Texture2D>,
    ) -> Result<Option<Vec<u8>>> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let width = size.Width as u32;
        let height = size.Height as u32;
        if desc.Width < width || desc.Height < height {
            // A queued WGC frame can still use the previous allocation after resize.
            return Ok(None);
        }
        if staging.is_none() {
            desc.Width = width;
            desc.Height = height;
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(staging))? };
        }
        let staging = staging
            .as_ref()
            .context("D3D11 не вернул staging-текстуру")?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        unsafe {
            self.context
                .CopySubresourceRegion(staging, 0, 0, 0, 0, texture, 0, Some(&region));
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let _mapping = Mapping {
            context: &self.context,
            texture: staging,
        };
        let width = size.Width as usize;
        let height = size.Height as usize;
        let pitch = mapped.RowPitch as usize;
        let len = mapped_len(pitch, width, height)?;
        ensure!(
            !mapped.pData.is_null() && len <= isize::MAX as usize,
            "D3D11 вернул недоступный буфер"
        );
        // The mapped staging texture covers all rows. Exclude trailing padding.
        let bytes = unsafe { slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
        Ok(Some(copy_bgra(bytes, pitch, width, height)?))
    }
}

struct Mapping<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
}

impl Drop for Mapping<'_> {
    fn drop(&mut self) {
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

fn mapped_len(pitch: usize, width: usize, height: usize) -> Result<usize> {
    let row = width.checked_mul(4).context("row size overflow")?;
    ensure!(
        row != 0 && height != 0 && pitch >= row,
        "invalid BGRA layout"
    );
    (height - 1)
        .checked_mul(pitch)
        .and_then(|offset| offset.checked_add(row))
        .context("BGRA buffer size overflow")
}

fn copy_bgra(mapped: &[u8], pitch: usize, width: usize, height: usize) -> Result<Vec<u8>> {
    let len = mapped_len(pitch, width, height)?;
    ensure!(mapped.len() >= len, "incomplete BGRA texture");
    let row_len = width * 4;
    let mut bgra = Vec::with_capacity(row_len * height);
    for row in mapped[..len].chunks(pitch) {
        bgra.extend_from_slice(&row[..row_len]);
    }
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
    Ok(bgra)
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_gpu_row_padding_and_makes_preview_opaque() -> anyhow::Result<()> {
        let mapped = [1, 2, 3, 0, 90, 91, 92, 93, 4, 5, 6, 127];
        assert_eq!(
            super::copy_bgra(&mapped, 8, 1, 2)?,
            [1, 2, 3, 255, 4, 5, 6, 255]
        );
        assert!(super::copy_bgra(&mapped[..11], 8, 1, 2).is_err());
        assert!(super::copy_bgra(&mapped, 3, 1, 2).is_err());
        Ok(())
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop with WGC and D3D11"]
    fn reads_frames_from_a_test_window_after_resize() -> super::Result<()> {
        use super::*;
        use std::time::Instant;
        use windows::{
            Win32::{
                System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop,
                UI::WindowsAndMessaging::{
                    CreateWindowExW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
                    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos, SetWindowTextW,
                    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
                },
            },
            core::{factory, w},
        };

        struct TestWindow(HWND);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }

        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        let _apartment = Apartment;
        let window = TestWindow(unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!("Vox WGC smoke test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                80,
                80,
                440,
                320,
                None,
                None,
                None,
                None,
            )?
        });
        let item: GraphicsCaptureItem = unsafe {
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?
                .CreateForWindow(window.0)?
        };
        let frames = frames(Source::Window {
            item,
            borderless: false,
        })?;
        let first = next_frame(window.0, &frames, |_| true).context("initial window frame")?;
        assert_eq!(
            first.bgra.len(),
            first.width as usize * first.height as usize * 4
        );
        unsafe {
            SetWindowPos(
                window.0,
                None,
                0,
                0,
                620,
                400,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOZORDER,
            )?;
        }
        let resized = next_frame(window.0, &frames, |frame| {
            frame.width > first.width && frame.height > first.height
        })
        .with_context(|| {
            format!(
                "waiting for a larger frame after {}x{}",
                first.width, first.height
            )
        })?;
        assert_eq!(
            resized.bgra.len(),
            resized.width as usize * resized.height as usize * 4
        );
        drop(window);

        let until = Instant::now() + Duration::from_secs(3);
        while !frames.is_closed() && Instant::now() < until {
            pump_messages();
        }
        assert!(frames.is_closed(), "closing the source did not end capture");
        return Ok(());

        fn pump_messages() {
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe { DispatchMessageW(&msg) };
            }
            thread::sleep(Duration::from_millis(10));
        }

        fn next_frame(
            window: HWND,
            frames: &Receiver<Result<Frame>>,
            matches: impl Fn(&Frame) -> bool,
        ) -> Result<Frame> {
            let until = Instant::now() + Duration::from_secs(5);
            let mut alternate = false;
            while Instant::now() < until {
                // Keep the instrumented compositor surface changing, as a game would.
                alternate = !alternate;
                unsafe {
                    SetWindowTextW(
                        window,
                        if alternate {
                            w!("Vox WGC frame A")
                        } else {
                            w!("Vox WGC frame B")
                        },
                    )?
                };
                pump_messages();
                match frames.try_recv() {
                    Ok(frame) => {
                        let frame = frame?;
                        if matches(&frame) {
                            return Ok(frame);
                        }
                    }
                    Err(async_channel::TryRecvError::Empty) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            anyhow::bail!("no capture frame with expected dimensions")
        }
    }
}
