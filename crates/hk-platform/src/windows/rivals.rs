//! Other programs that share a keyboard and mouse between PCs.
//!
//! Each of them sits in the way of every key press and mouse movement on the computer, like
//! Hackerman's own input hooks do. Two together can hold each other up, so the app says so.

use std::collections::BTreeSet;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

/// How each one's programs start, and what it's called.
const RIVALS: [(&str, &str); 8] = [
    ("inputdirector", "Input Director"),
    ("idwinservice", "Input Director"),
    ("synergy", "Synergy"),
    ("barrier", "Barrier"),
    ("deskflow", "Deskflow"),
    ("input-leap", "Input Leap"),
    ("lan-mouse", "Lan Mouse"),
    ("powertoys.mousewithoutborders", "Mouse Without Borders"),
];

fn rival_named(program: &str) -> Option<&'static str> {
    let program = program.to_lowercase();
    RIVALS
        .iter()
        .find_map(|(start, name)| program.starts_with(start).then_some(*name))
}

pub(super) fn running() -> Vec<String> {
    let mut found = BTreeSet::new();
    // SAFETY: the snapshot is checked and closed, and each entry states its size.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return Vec::new();
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot, &raw mut entry).is_ok();
        while more {
            let end = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let program = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if let Some(name) = rival_named(&program) {
                found.insert(name.to_string());
            }
            more = Process32NextW(snapshot, &raw mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_programs_for_the_same_job_are_recognised() {
        assert_eq!(rival_named("InputDirector.exe"), Some("Input Director"));
        assert_eq!(
            rival_named("InputDirectorSessionHelper.exe"),
            Some("Input Director")
        );
        assert_eq!(rival_named("IDWinService.exe"), Some("Input Director"));
        assert_eq!(rival_named("deskflow-client.exe"), Some("Deskflow"));
        assert_eq!(
            rival_named("PowerToys.MouseWithoutBorders.exe"),
            Some("Mouse Without Borders")
        );
    }

    #[test]
    fn ordinary_programs_are_not() {
        for program in ["hackerman.exe", "explorer.exe", "Code.exe", "input.exe"] {
            assert_eq!(rival_named(program), None, "{program}");
        }
    }
}
