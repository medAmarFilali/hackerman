//! Where the service and what it keeps are.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::CreateDirectoryW;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::{GUID, HSTRING, w};

/// A folder that Windows knows about, for the user with this token or the current one. It's
/// asked rather than read from the environment, which programs starting this one can change.
pub(crate) fn known_folder(id: &GUID, token: Option<HANDLE>) -> Result<PathBuf> {
    // SAFETY: Windows allocates the path, which is copied and then freed here.
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, token)?;
        let text = path.to_string();
        CoTaskMemFree(Some(path.0.cast()));
        Ok(PathBuf::from(
            text.context("that folder's name isn't valid")?,
        ))
    }
}

/// Where the service runs from. Only administrators can change what's in Program Files.
pub fn program_folder() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramFiles, None)?.join("Hackerman Lock Screen"))
}

pub(crate) fn approval_file() -> Result<PathBuf> {
    Ok(program_folder()?.join("approved.json"))
}

pub(crate) fn service_program() -> Result<PathBuf> {
    Ok(program_folder()?.join("hackerman-lock-screen.exe"))
}

/// What the service keeps, including a copy of this computer's private key, so it's kept
/// where only the system and administrators can read it.
pub(crate) fn data_folder() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramData, None)?.join("Hackerman"))
}

/// What the helper runs from.
pub(crate) fn helper_settings() -> Result<PathBuf> {
    Ok(data_folder()?.join("lock-screen"))
}

/// Makes a folder that only the system and administrators can reach. It fails if the folder is
/// there already: anyone can make folders where this goes, and one that's already there might
/// hold anything, with anyone able to reach it.
pub(crate) fn create_private_folder(path: &Path) -> Result<()> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: Windows allocates the descriptor, which is kept for the call that needs it, and
    // is then freed.
    unsafe {
        // Protected, so nothing is inherited. Full access for the system and administrators,
        // passed on to what's inside, and no entry for anyone else.
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let created = CreateDirectoryW(&HSTRING::from(path), Some(&raw const attributes));
        LocalFree(Some(HLOCAL(descriptor.0)));
        created.with_context(|| format!("can't create {}", path.display()))
    }
}
