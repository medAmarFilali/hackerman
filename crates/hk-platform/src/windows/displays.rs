//! Lists the monitors, with their position, scale, name and a stable identity.
//!
//! Two Windows APIs are needed. The older GDI functions give each monitor's
//! position and scale but only a name like `\\.\DISPLAY2`, whose number can
//! change on reboot. The newer display configuration API gives the friendly
//! name and a device path that stays the same, and links back to the GDI name.

use std::collections::HashMap;

use hk_core::geometry::Rect;
use hk_core::layout::Monitor;
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::BOOL;

/// Windows' 100% scale.
const BASE_DPI: f64 = 96.0;

struct DisplayName {
    friendly: String,
    device_path: String,
}

pub(super) fn enumerate() -> Vec<Monitor> {
    let names = display_names();
    monitor_handles()
        .into_iter()
        .filter_map(|handle| describe(handle, &names))
        .collect()
}

fn monitor_handles() -> Vec<HMONITOR> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        handles: LPARAM,
    ) -> BOOL {
        // SAFETY: `handles` is the pointer to the vector passed below, which
        // outlives EnumDisplayMonitors, and this callback is only called during it.
        let handles = unsafe { &mut *(handles.0 as *mut Vec<HMONITOR>) };
        handles.push(monitor);
        true.into()
    }

    let mut handles: Vec<HMONITOR> = Vec::new();
    let pointer = LPARAM(&raw mut handles as isize);
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(collect), pointer) };
    handles
}

fn describe(handle: HMONITOR, names: &HashMap<String, DisplayName>) -> Option<Monitor> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: the size set above tells Windows it may fill in the extended
    // structure, which starts with the MONITORINFO it asks for.
    let found = unsafe { GetMonitorInfoW(handle, (&raw mut info).cast::<MONITORINFO>()) };
    if !found.as_bool() {
        return None;
    }

    let rect = info.monitorInfo.rcMonitor;
    let gdi_name = from_wide(&info.szDevice);
    let (mut dpi_x, mut dpi_y) = (0, 0);
    let scale = match unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
    {
        Ok(()) if dpi_x > 0 => f64::from(dpi_x) / BASE_DPI,
        _ => 1.0,
    };

    let name = names.get(&gdi_name);
    Some(Monitor {
        id: name
            .map(|name| name.device_path.clone())
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| gdi_name.clone()),
        name: name
            .map(|name| name.friendly.clone())
            .filter(|friendly| !friendly.is_empty())
            .unwrap_or_else(|| fallback_name(&gdi_name)),
        bounds: Rect::new(
            f64::from(rect.left),
            f64::from(rect.top),
            f64::from(rect.right - rect.left),
            f64::from(rect.bottom - rect.top),
        ),
        scale,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

fn fallback_name(gdi_name: &str) -> String {
    let number = gdi_name.trim_start_matches(r"\\.\DISPLAY");
    format!("Display {number}")
}

/// Built-in laptop panels often have no friendly name of their own.
fn is_built_in(technology: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> bool {
    [
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
    ]
    .contains(&technology)
}

/// Friendly names and device paths, keyed by GDI name.
fn display_names() -> HashMap<String, DisplayName> {
    let (mut path_count, mut mode_count) = (0, 0);
    let sized = unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
    };
    if sized != ERROR_SUCCESS {
        return HashMap::new();
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    // SAFETY: the buffers have the sizes passed alongside them.
    let queried = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    };
    if queried != ERROR_SUCCESS {
        return HashMap::new();
    }
    paths.truncate(path_count as usize);

    let mut names = HashMap::new();
    for path in &paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: each header states the size of the structure it begins.
        let ok = unsafe {
            DisplayConfigGetDeviceInfo(&mut source.header) == 0
                && DisplayConfigGetDeviceInfo(&mut target.header) == 0
        };
        if ok {
            // With mirrored displays, several targets share a source: keep the first.
            names
                .entry(from_wide(&source.viewGdiDeviceName))
                .or_insert_with(|| {
                    let friendly = from_wide(&target.monitorFriendlyDeviceName);
                    DisplayName {
                        friendly: if friendly.is_empty() && is_built_in(target.outputTechnology) {
                            "Built-in display".to_string()
                        } else {
                            friendly
                        },
                        device_path: from_wide(&target.monitorDevicePath),
                    }
                });
        }
    }
    names
}

fn from_wide(text: &[u16]) -> String {
    let end = text
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_stop_at_the_terminator() {
        let text: Vec<u16> = "DELL U2720Q\0junk".encode_utf16().collect();
        assert_eq!(from_wide(&text), "DELL U2720Q");
    }

    #[test]
    fn fallback_names_use_the_display_number() {
        assert_eq!(fallback_name(r"\\.\DISPLAY2"), "Display 2");
    }

    #[test]
    fn this_machines_monitors_make_sense() {
        // A machine running tests without a display, like some CI runners, has none.
        let monitors = enumerate();
        assert!(monitors.iter().all(Monitor::is_valid));
        assert!(monitors.iter().filter(|monitor| monitor.primary).count() <= 1);
    }
}
