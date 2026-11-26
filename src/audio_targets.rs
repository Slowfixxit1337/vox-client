use anyhow::{Context, Result};
use std::{
    collections::HashSet,
    ffi::OsString,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM},
        System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
        },
    },
    core::{BOOL, PWSTR},
};

pub struct Application {
    pub pid: u32,
    pub label: String,
}

pub fn applications() -> Result<Vec<Application>> {
    let mut windows = Vec::<(u32, String)>::new();
    // EnumWindows calls back synchronously and does not keep the borrowed context.
    unsafe {
        EnumWindows(
            Some(collect_window),
            LPARAM(&mut windows as *mut _ as isize),
        )
    }
    .context("не удалось получить список окон")?;

    let mut seen = HashSet::new();
    let mut path_buffer = vec![0; 32_768];
    let mut apps: Vec<_> = windows
        .into_iter()
        .filter(|(pid, _)| seen.insert(*pid))
        .filter_map(|(pid, title)| {
            let executable = executable_name(pid, &mut path_buffer)?;
            if vox_process_name(&executable) {
                return None;
            }
            Some(Application {
                pid,
                label: format!("{executable} ({pid}) · {title}"),
            })
        })
        .collect();
    apps.sort_by_cached_key(|app| (app.label.to_lowercase(), app.pid));
    Ok(apps)
}

unsafe extern "system" fn collect_window(window: HWND, context: LPARAM) -> BOOL {
    if !unsafe { IsWindowVisible(window) }.as_bool() {
        return true.into();
    }

    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    if pid == 0 || pid == std::process::id() {
        return true.into();
    }

    let mut title = [0; 512];
    let length = unsafe { GetWindowTextW(window, &mut title) };
    if length > 0 {
        let title = String::from_utf16_lossy(&title[..length as usize]);
        let title = title.trim();
        if !title.is_empty() {
            // The caller lends one Vec to this synchronous enumeration.
            let windows = unsafe { &mut *(context.0 as *mut Vec<(u32, String)>) };
            windows.push((pid, title.to_owned()));
        }
    }
    true.into()
}

fn executable_name(pid: u32, buffer: &mut [u16]) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    // OpenProcess transfers one handle. OwnedHandle closes it on every return path.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let mut length = buffer.len().try_into().ok()?;
    unsafe {
        QueryFullProcessImageNameW(
            HANDLE(handle.as_raw_handle()),
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()?;
    let path = OsString::from_wide(buffer.get(..length as usize)?);
    Some(Path::new(&path).file_name()?.to_string_lossy().into_owned())
}

fn vox_process_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "vox.exe"
            | "vox-client.exe"
            | "vox_client.exe"
            | "vox-capture-preview.exe"
            | "vox-capture-lab.exe"
    ) || name.starts_with("vox-client-")
        || name.starts_with("vox_client-")
}

#[cfg(test)]
mod tests {
    use super::vox_process_name;

    #[test]
    fn hides_known_vox_clients_but_not_unrelated_processes() {
        assert!(vox_process_name("VOX-CLIENT.EXE"));
        assert!(vox_process_name("vox_client-1234.exe"));
        assert!(vox_process_name("vox-capture-preview.exe"));
        assert!(!vox_process_name("game-vox.exe"));
    }
}
