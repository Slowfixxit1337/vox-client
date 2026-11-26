//! WASAPI capture of an explicitly selected process tree.
use anyhow::{Context, Result, bail, ensure};
use async_channel::{Receiver, Sender};
use std::{
    collections::HashMap,
    mem::ManuallyDrop,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    ptr::null_mut,
    slice,
    sync::mpsc::{self, RecvTimeoutError, SyncSender},
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{ERROR_NO_MORE_FILES, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
            AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
            IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
            IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
            PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVE_FORMAT_PCM, WAVEFORMATEX,
        },
        System::{
            Com::{
                BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, IAgileObject,
                IAgileObject_Impl,
                StructuredStorage::{
                    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
                },
            },
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Threading::{
                CreateEventW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
            },
            Variant::VT_BLOB,
        },
    },
    core::{HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement},
};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const SAMPLE_BYTES: usize = size_of::<i16>();
const SAMPLES_PER_BLOCK: usize = SAMPLE_RATE as usize / 100 * CHANNELS as usize;
const QUEUE_SIZE: usize = 4;
const WAIT_MS: u32 = 40;
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(4);

type Frames = Sender<Result<Vec<i16>>>;

/// Start capturing 10 ms stereo PCM blocks from the selected process and its descendants.
pub fn capture(pid: u32) -> Result<Receiver<Result<Vec<i16>>>> {
    ensure!(
        pid != 0 && pid != std::process::id(),
        "нельзя захватывать звук Vox"
    );
    let process = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )
    }
    .context("выбранный процесс недоступен")?;
    // Keep this exact process object alive and check for exit throughout capture.
    let process = unsafe { OwnedHandle::from_raw_handle(process.0) };
    let (sender, receiver) = async_channel::bounded(QUEUE_SIZE);
    thread::Builder::new()
        .name("vox-process-loopback".into())
        .spawn(move || {
            if let Err(error) = run(pid, &process, &sender) {
                let _ = sender.force_send(Err(error));
            }
        })
        .context("не удалось запустить захват звука приложения")?;
    Ok(receiver)
}

fn run(pid: u32, process: &OwnedHandle, sender: &Frames) -> Result<()> {
    let process = HANDLE(process.as_raw_handle());
    validate_target(pid, std::process::id(), &process_parents()?)?;
    validate_process_image(process)?;
    ensure!(process_running(process)?, "выбранное приложение закрыто");
    let _apartment = Apartment::initialize()?;
    let ready = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }?;
    let ready = unsafe { OwnedHandle::from_raw_handle(ready.0) };
    let event = HANDLE(ready.as_raw_handle());
    let Some(client) = activate(pid, sender)? else {
        return Ok(());
    };
    ensure!(process_running(process)?, "выбранное приложение закрылось");
    // Check again after asynchronous activation before receiving or emitting PCM.
    validate_target(pid, std::process::id(), &process_parents()?)?;
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * CHANNELS as u32 * SAMPLE_BYTES as u32,
        nBlockAlign: CHANNELS * SAMPLE_BYTES as u16,
        wBitsPerSample: (SAMPLE_BYTES * 8) as u16,
        cbSize: 0,
    };
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            0,
            0,
            &format,
            None,
        )
    }
    .context("WASAPI не подтвердил изолированный формат 48 кГц")?;
    let reader: IAudioCaptureClient = unsafe { client.GetService()? };
    unsafe { client.SetEventHandle(event)? };
    unsafe { client.Start() }.context("не удалось запустить изолированный захват WASAPI")?;
    let _active = ActiveCapture(&client);
    let mut pending = Vec::with_capacity(SAMPLES_PER_BLOCK);
    while !sender.is_closed() {
        let ready = unsafe { WaitForSingleObject(event, WAIT_MS) };
        ensure!(process_running(process)?, "выбранное приложение закрылось");
        match ready {
            WAIT_OBJECT_0 => read_packets(&reader, sender, &mut pending)?,
            WAIT_TIMEOUT => {}
            _ => return Err(windows::core::Error::from_thread()).context("сбой ожидания WASAPI"),
        }
    }
    Ok(())
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activation {
    complete: SyncSender<()>,
    // Keep the PROPVARIANT and its borrowed BLOB stable during async activation,
    // including when the caller returns on timeout or cancellation.
    // The VT_BLOB borrows Rust storage. PropVariantClear would try to free it.
    _variant: Box<ManuallyDrop<PROPVARIANT>>,
    _params: Box<AUDIOCLIENT_ACTIVATION_PARAMS>,
}

impl IAgileObject_Impl for Activation_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activation_Impl {
    fn ActivateCompleted(
        &self,
        _: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.complete.try_send(());
        Ok(())
    }
}

fn activate(pid: u32, frames: &Frames) -> Result<Option<IAudioClient>> {
    let params = Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    });
    let variant = Box::new(ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: std::ptr::from_ref(&*params).cast_mut().cast(),
                    },
                },
                ..Default::default()
            }),
        },
    }));
    let ptr = std::ptr::from_ref::<PROPVARIANT>(&variant);
    let (complete, wait) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Activation {
        complete,
        _variant: variant,
        _params: params,
    }
    .into();
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(ptr),
            &handler,
        )
    }
    .context("эта версия Windows не предоставляет изолированный захват звука приложения")?;

    let deadline = Instant::now() + ACTIVATION_TIMEOUT;
    loop {
        if frames.is_closed() {
            return Ok(None);
        }
        ensure!(
            Instant::now() < deadline,
            "WASAPI не ответил на запрос захвата"
        );
        match wait.recv_timeout(Duration::from_millis(u64::from(WAIT_MS))) {
            Ok(()) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(error) => return Err(error).context("сбой активации WASAPI"),
        }
    }

    let mut result = HRESULT::default();
    let mut interface: Option<IUnknown> = None;
    unsafe { operation.GetActivateResult(&mut result, &mut interface) }?;
    result
        .ok()
        .context("Windows отклонила изолированный захват звука приложения")?;
    Ok(Some(
        interface
            .context("WASAPI не вернул аудиоинтерфейс")?
            .cast()?,
    ))
}

fn read_packets(
    reader: &IAudioCaptureClient,
    sender: &Frames,
    pending: &mut Vec<i16>,
) -> Result<()> {
    while !sender.is_closed() && unsafe { reader.GetNextPacketSize()? } > 0 {
        let mut data = null_mut();
        let mut frame_count = 0;
        let mut flags = 0;
        unsafe { reader.GetBuffer(&mut data, &mut frame_count, &mut flags, None, None)? };
        let packet = Packet {
            reader,
            frame_count,
        };
        if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
            pending.clear();
        }
        let len = frame_count as usize * usize::from(CHANNELS);
        if len != 0 && flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 {
            ensure!(!data.is_null(), "WASAPI вернул некорректный буфер");
            // The capture client holds the shared-mode PCM buffer until ReleaseBuffer.
            let bytes = unsafe { slice::from_raw_parts(data, len * SAMPLE_BYTES) };
            append_samples(
                sender,
                pending,
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b)),
            );
        } else {
            append_samples(sender, pending, std::iter::repeat_n(0, len));
        }
        packet.release()?;
    }
    Ok(())
}

fn append_samples(sender: &Frames, pending: &mut Vec<i16>, mut samples: impl Iterator<Item = i16>) {
    while !sender.is_closed() {
        pending.extend(samples.by_ref().take(SAMPLES_PER_BLOCK - pending.len()));
        if pending.len() != SAMPLES_PER_BLOCK {
            break;
        }
        let block = std::mem::replace(pending, Vec::with_capacity(SAMPLES_PER_BLOCK));
        if sender.force_send(Ok(block)).is_err() {
            break;
        }
    }
}

fn process_parents() -> Result<HashMap<u32, u32>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?;
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot.0) };
    let handle = HANDLE(snapshot.as_raw_handle());
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe { Process32FirstW(handle, &mut entry)? };
    let mut parents = HashMap::new();
    loop {
        parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        match unsafe { Process32NextW(handle, &mut entry) } {
            Ok(()) => {}
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) => break,
            Err(error) => return Err(error).context("не удалось проверить дерево процессов"),
        }
    }
    Ok(parents)
}

fn validate_target(pid: u32, own: u32, parents: &HashMap<u32, u32>) -> Result<()> {
    ensure!(
        pid != 0 && parents.contains_key(&pid) && parents.contains_key(&own),
        "не удалось подтвердить выбранный процесс"
    );
    ensure!(
        !includes(own, pid, parents)? && !includes(pid, own, parents)?,
        "нельзя захватывать звук дерева процессов Vox"
    );
    Ok(())
}

fn includes(root: u32, mut process: u32, parents: &HashMap<u32, u32>) -> Result<bool> {
    // A stale Toolhelp parent identifier can form a cycle after PID reuse.
    for _ in 0..=parents.len() {
        if process == root {
            return Ok(true);
        }
        let Some(&parent) = parents.get(&process).filter(|&&pid| pid != 0) else {
            return Ok(false);
        };
        process = parent;
    }
    bail!("не удалось однозначно подтвердить дерево процессов")
}

fn process_running(process: HANDLE) -> Result<bool> {
    match unsafe { WaitForSingleObject(process, 0) } {
        WAIT_TIMEOUT => Ok(true),
        WAIT_OBJECT_0 => Ok(false),
        WAIT_FAILED => {
            Err(windows::core::Error::from_thread()).context("не удалось проверить процесс")
        }
        _ => bail!("не удалось проверить процесс"),
    }
}

fn validate_process_image(process: HANDLE) -> Result<()> {
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    }
    .context("не удалось подтвердить исполняемый файл выбранного приложения")?;
    let path = String::from_utf16_lossy(&path[..length as usize]);
    let name = Path::new(&path)
        .file_stem()
        .and_then(|name| name.to_str())
        .context("не удалось подтвердить имя выбранного приложения")?;
    ensure!(
        !is_vox_image(name),
        "нельзя захватывать звук другого экземпляра Vox"
    );
    Ok(())
}

fn is_vox_image(stem: &str) -> bool {
    let name = stem.to_ascii_lowercase();
    name == "vox"
        || name.starts_with("vox-client")
        || name.starts_with("vox_client")
        || name.starts_with("vox-capture")
        || name.starts_with("vox-lab")
}

struct Apartment;

impl Apartment {
    fn initialize() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct ActiveCapture<'a>(&'a IAudioClient);

impl Drop for ActiveCapture<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.0.Stop() };
    }
}

struct Packet<'a> {
    reader: &'a IAudioCaptureClient,
    frame_count: u32,
}

impl Packet<'_> {
    fn release(mut self) -> Result<()> {
        let len = std::mem::take(&mut self.frame_count);
        unsafe { self.reader.ReleaseBuffer(len) }?;
        Ok(())
    }
}

impl Drop for Packet<'_> {
    fn drop(&mut self) {
        if self.frame_count != 0 {
            // A failed read must still release the captured packet.
            let _ = unsafe { self.reader.ReleaseBuffer(self.frame_count) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_stereo_frames_without_reordering() {
        let samples: Vec<i16> = (0..SAMPLES_PER_BLOCK as i16 * 2).collect();
        let (sender, receiver) = async_channel::bounded(2);
        let mut pending = Vec::new();
        append_samples(&sender, &mut pending, samples[..100].iter().copied());
        assert!(receiver.is_empty());
        append_samples(&sender, &mut pending, samples[100..].iter().copied());
        assert_eq!(
            receiver.try_recv().unwrap().unwrap(),
            samples[..SAMPLES_PER_BLOCK]
        );
        assert_eq!(
            receiver.try_recv().unwrap().unwrap(),
            samples[SAMPLES_PER_BLOCK..]
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn rejects_unsafe_process_trees() {
        let parents = [(10, 1), (20, 10), (30, 20), (40, 30), (50, 10)]
            .into_iter()
            .collect();
        for pid in [0, 10, 20, 30, 40, 999] {
            assert!(validate_target(pid, 30, &parents).is_err(), "{pid}");
        }
        assert!(validate_target(50, 30, &parents).is_ok());
    }

    #[test]
    fn rejects_separately_launched_vox_images() {
        assert!(is_vox_image("VOX-client"));
        assert!(is_vox_image("vox_client-d2046"));
        assert!(is_vox_image("vox-capture-preview"));
        assert!(!is_vox_image("VoxelGame"));
    }

    #[tokio::test]
    #[ignore = "requires a render endpoint and two independent PCM playback processes"]
    async fn captures_selected_tone_without_other_process_audio() -> Result<()> {
        // Two external siblings continuously render 400 Hz and 1200 Hz at the same amplitude.
        // This test is the listener and never renders its own signal.
        let pid = std::env::var("VOX_WASAPI_TEST_PID")?.parse()?;
        let expected = match std::env::var("VOX_WASAPI_TEST_TARGET_HZ").as_deref() {
            Ok("1200") => 1200.0,
            _ => 400.0,
        };
        let excluded = if expected == 400.0 { 1200.0 } else { 400.0 };
        let rx = capture(pid)?;
        tokio::time::timeout(Duration::from_secs(6), async {
            let mut detected = 0;
            while detected < 10 {
                let block = rx.recv().await??;
                assert_eq!(block.len(), SAMPLES_PER_BLOCK);
                let game = magnitude(&block, expected);
                let other = magnitude(&block, excluded);
                if game > 500.0 {
                    assert!(
                        other < game * 0.05,
                        "unexpected {excluded} Hz from another process: {other}"
                    );
                    detected += 1;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("не обнаружен тестовый сигнал выбранного приложения")??;
        Ok(())
    }

    fn magnitude(block: &[i16], hz: f32) -> f32 {
        let (sin, cos) =
            block
                .iter()
                .step_by(2)
                .enumerate()
                .fold((0.0, 0.0), |(sin, cos), (index, sample)| {
                    let (s, c) =
                        (std::f32::consts::TAU * hz * index as f32 / SAMPLE_RATE as f32).sin_cos();
                    (sin + s * f32::from(*sample), cos + c * f32::from(*sample))
                });
        (sin.hypot(cos) * 2.0) / (block.len() / 2) as f32
    }
}
