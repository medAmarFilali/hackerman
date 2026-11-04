//! Lists the monitors, with their position, scale, name and a stable identity.
//!
//! Two Windows APIs are needed. The older GDI functions give each monitor's
//! position and scale but only a name like `\\.\DISPLAY2`, whose number can
//! change on reboot. The newer display configuration API gives the friendly
//! name and a device path that stays the same, and links back to the GDI name.

use std::collections::HashMap;

use hk_core::geometry::Rect;
use hk_core::layout::Monitor;
use hk_core::shared_monitors::MonitorIdentity;
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
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY, RegGetValueW};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::{BOOL, HSTRING, w};

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

/// Each monitor's identity, by the handle Windows gives it for now.
pub(super) fn ids_by_handle() -> HashMap<isize, String> {
    let names = display_names();
    monitor_handles()
        .into_iter()
        .filter_map(|handle| Some((handle.0 as isize, describe(handle, &names)?.id)))
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

/// What each monitor says it is, for the ones that give a serial number.
pub(super) fn identities() -> Vec<MonitorIdentity> {
    enumerate()
        .into_iter()
        .filter_map(|monitor| {
            let hardware = identity_from_edid(&edid(&monitor.id)?)?;
            Some(MonitorIdentity {
                monitor: monitor.id,
                hardware,
            })
        })
        .collect()
}

/// The description a monitor gives of itself, which Windows keeps. A monitor's ID looks like
/// `\\?\DISPLAY#PKB036E#4&b73cac7&0&UID4145#{e6f07b5f-…}`, and it's kept under the middle two.
fn edid(monitor_id: &str) -> Option<Vec<u8>> {
    let mut parts = monitor_id.split('#');
    parts.next()?;
    let (model, instance) = (parts.next()?, parts.next()?);
    let plain = |part: &str| !part.is_empty() && !part.contains(['\\', '/']);
    if !plain(model) || !plain(instance) {
        return None;
    }
    let key = HSTRING::from(format!(
        r"SYSTEM\CurrentControlSet\Enum\DISPLAY\{model}\{instance}\Device Parameters"
    ));
    let mut edid = vec![0u8; 1024];
    let mut size = edid.len() as u32;
    // SAFETY: the size given is that of the buffer, which Windows says how much of it filled.
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &key,
            w!("EDID"),
            RRF_RT_REG_BINARY,
            None,
            Some(edid.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    if result != ERROR_SUCCESS {
        return None;
    }
    edid.truncate(size as usize);
    Some(edid)
}

/// A monitor's maker, model and serial number, like `PKB036E:D2NEE0014200`, from its EDID.
/// One without a serial number has none: two of that model would look like one monitor.
fn identity_from_edid(edid: &[u8]) -> Option<String> {
    const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    // Three letters, five bits each, with 1 for A.
    let maker = u16::from_be_bytes([edid[8], edid[9]]);
    let maker: String = [10, 5, 0]
        .into_iter()
        .map(|shift| match (maker >> shift) & 0x1F {
            letter @ 1..=26 => Some(char::from(b'A' + letter as u8 - 1)),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let product = u16::from_le_bytes([edid[10], edid[11]]);

    // Written out in one of the four descriptors, or else as a number.
    let written = (0..4)
        .map(|index| &edid[54 + 18 * index..72 + 18 * index])
        .find(|descriptor| descriptor[..3] == [0, 0, 0] && descriptor[3] == 0xFF)
        .map(|descriptor| {
            descriptor[5..]
                .iter()
                .take_while(|&&byte| byte != b'\n')
                .filter(|byte| byte.is_ascii_graphic())
                .map(|&byte| char::from(byte))
                .collect::<String>()
        })
        .filter(|serial| !serial.is_empty() && serial.chars().any(|c| c != '0'));
    let number = u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]);
    // Some models all give the same made-up one.
    let numbered = (number != 0 && number != 0x0101_0101).then(|| number.to_string());
    let serial = written.or(numbered)?;
    Some(format!("{maker}{product:04X}:{serial}"))
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

    /// Like the Maestro236D's: made by Packard Bell, with its serial written out.
    fn edid(serial_text: Option<&str>, serial_number: u32) -> Vec<u8> {
        let mut edid = vec![0u8; 128];
        edid[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        edid[8..10].copy_from_slice(&[0x41, 0x62]);
        edid[10..12].copy_from_slice(&0x036E_u16.to_le_bytes());
        edid[12..16].copy_from_slice(&serial_number.to_le_bytes());
        // The monitor's name, in the last descriptor.
        edid[108 + 3] = 0xFC;
        edid[108 + 5..108 + 17].copy_from_slice(b"Maestro236D\n");
        if let Some(text) = serial_text {
            edid[90 + 3] = 0xFF;
            let mut field = [b' '; 13];
            field[..text.len()].copy_from_slice(text.as_bytes());
            field[text.len()] = b'\n';
            edid[90 + 5..90 + 18].copy_from_slice(&field);
        }
        edid
    }

    #[test]
    fn a_monitor_is_its_maker_model_and_serial_number() {
        assert_eq!(
            identity_from_edid(&edid(Some("D2NEE0014200"), 1_134_011_102)).as_deref(),
            Some("PKB036E:D2NEE0014200")
        );
        assert_eq!(
            identity_from_edid(&edid(None, 1_134_011_102)).as_deref(),
            Some("PKB036E:1134011102"),
            "with only the number"
        );
    }

    /// Two monitors of one model would look like the same one.
    #[test]
    fn a_monitor_with_no_real_serial_number_has_no_identity() {
        assert_eq!(identity_from_edid(&edid(None, 0)), None);
        assert_eq!(identity_from_edid(&edid(None, 0x0101_0101)), None);
        assert_eq!(identity_from_edid(&edid(Some("0000000"), 0)), None);
    }

    #[test]
    fn what_isnt_an_edid_says_nothing() {
        assert_eq!(identity_from_edid(&[]), None);
        assert_eq!(identity_from_edid(&[0xFF; 128]), None);
        let mut short = edid(Some("D2NEE0014200"), 1);
        short.truncate(100);
        assert_eq!(identity_from_edid(&short), None);
    }

    /// Only reads. Run with `--nocapture` to see what it finds.
    #[test]
    fn this_machines_monitors_say_what_they_are() {
        let monitors = enumerate();
        for identity in identities() {
            println!("{} is {}", identity.monitor, identity.hardware);
            assert!(
                monitors
                    .iter()
                    .any(|monitor| monitor.id == identity.monitor)
            );
            assert!(identity.hardware.contains(':'), "{}", identity.hardware);
        }
    }

    #[test]
    fn this_machines_monitors_make_sense() {
        // A machine running tests without a display, like some CI runners, has none.
        let monitors = enumerate();
        assert!(monitors.iter().all(Monitor::is_valid));
        assert!(monitors.iter().filter(|monitor| monitor.primary).count() <= 1);
    }
}
