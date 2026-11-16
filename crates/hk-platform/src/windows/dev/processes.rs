//! The processes running here, how much memory they use, and stopping them.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use hk_core::dev::{
    self, MAX_LISTED, ProcessInfo, Program, ProgramId, ProgramKind, ProgramsReport,
};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
use windows::Win32::Foundation::{FILETIME, HANDLE, UNICODE_STRING};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};
use windows::core::Owned;

use super::{docker, ports, tools};

/// Windows' own programs, which nothing here stops: the desktop and what keeps it going.
/// Hackerman isn't stopped either.
const NEVER_STOPPED: [&str; 18] = [
    "applicationframehost.exe",
    "conhost.exe",
    "csrss.exe",
    "ctfmon.exe",
    "dwm.exe",
    "explorer.exe",
    "fontdrvhost.exe",
    "hackerman.exe",
    "lsass.exe",
    "searchhost.exe",
    "services.exe",
    "shellexperiencehost.exe",
    "sihost.exe",
    "smss.exe",
    "startmenuexperiencehost.exe",
    "svchost.exe",
    "textinputhost.exe",
    "winlogon.exe",
];

fn from_wide(text: &[u16]) -> String {
    let end = text
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

/// When it started, by Windows' clock. Zero if that can't be read.
fn started(process: HANDLE) -> u64 {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: four plain structures for Windows to fill in.
    let read = unsafe {
        GetProcessTimes(
            process,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if read.is_err() {
        return 0;
    }
    (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
}

/// The memory it has claimed for itself alone, in bytes.
fn memory(process: HANDLE) -> u64 {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: the structure states its own size, which starts with the plain counters.
    let read = unsafe {
        K32GetProcessMemoryInfo(
            process,
            (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if read.as_bool() {
        counters.PrivateUsage as u64
    } else {
        0
    }
}

fn command_line(process: HANDLE) -> Option<String> {
    let mut needed = 0u32;
    // SAFETY: asking with no buffer only says how large it needs to be.
    let _ = unsafe {
        NtQueryInformationProcess(
            process,
            ProcessCommandLineInformation,
            std::ptr::null_mut(),
            0,
            &raw mut needed,
        )
    };
    if needed == 0 || needed > 64 * 1024 {
        return None;
    }
    // In u64s, so that the structure Windows writes at the start is aligned.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: the buffer has the size Windows asked for. What it writes is a UNICODE_STRING
    // pointing at text just after it, in the same buffer, which is read before it goes.
    unsafe {
        let status = NtQueryInformationProcess(
            process,
            ProcessCommandLineInformation,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        );
        if status.is_err() {
            return None;
        }
        let text = &*buffer.as_ptr().cast::<UNICODE_STRING>();
        if text.Buffer.is_null() {
            return None;
        }
        let units = std::slice::from_raw_parts(text.Buffer.0, usize::from(text.Length) / 2);
        Some(String::from_utf16_lossy(units))
    }
}

fn session_of(pid: u32) -> Option<u32> {
    let mut session = 0;
    // SAFETY: the session number is a plain value for Windows to fill in.
    unsafe { ProcessIdToSessionId(pid, &raw mut session) }
        .ok()
        .map(|()| session)
}

/// Every process this program can see into. Windows keeps some of its own closed to it,
/// which are left out.
pub(super) fn snapshot() -> Vec<ProcessInfo> {
    // SAFETY: only reads.
    let own_pid = unsafe { GetCurrentProcessId() };
    let own_session = session_of(own_pid);
    let mut processes = Vec::new();
    // SAFETY: the snapshot is owned as soon as it exists, and each entry states its own size.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return processes;
        };
        let snapshot = Owned::new(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(*snapshot, &raw mut entry).is_ok();
        while more {
            let pid = entry.th32ProcessID;
            if pid > 4
                && let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            {
                let process = Owned::new(process);
                let name = from_wide(&entry.szExeFile);
                let can_stop = pid != own_pid
                    && own_session.is_some()
                    && session_of(pid) == own_session
                    && !NEVER_STOPPED.contains(&name.to_ascii_lowercase().as_str());
                processes.push(ProcessInfo {
                    id: ProgramId {
                        pid,
                        started: started(*process),
                    },
                    parent: Some(entry.th32ParentProcessID),
                    name,
                    command_line: command_line(*process),
                    memory: memory(*process),
                    can_stop,
                });
            }
            more = Process32NextW(*snapshot, &raw mut entry).is_ok();
        }
    }
    processes
}

fn memory_totals() -> (u64, u64) {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: the structure states its own size.
    match unsafe { GlobalMemoryStatusEx(&raw mut status) } {
        Ok(()) => (
            status.ullTotalPhys,
            status.ullTotalPhys - status.ullAvailPhys,
        ),
        Err(_) => (0, 0),
    }
}

pub(super) fn report() -> ProgramsReport {
    let processes = snapshot();
    let mut programs = dev::group(&processes);
    let (memory_total, memory_used) = memory_totals();

    // Which program each process is part of, for the ports.
    let mut owner: HashMap<u32, &Program> = HashMap::new();
    for program in &programs {
        for member in dev::members_of(&processes, program.id) {
            owner.insert(member.pid, program);
        }
    }
    let listening = ports::listening();
    // Docker Desktop listens for its containers, which are what the ports are really for.
    let for_docker = |pid: &u32| {
        owner
            .get(pid)
            .is_some_and(|program| program.kind == ProgramKind::Docker)
            || processes
                .iter()
                .any(|p| p.id.pid == *pid && p.name.eq_ignore_ascii_case("wslrelay.exe"))
    };
    let containers = if listening.iter().any(|port| for_docker(&port.pid)) {
        docker::published_ports()
    } else {
        Vec::new()
    };
    let ports = listening
        .into_iter()
        .map(|port| {
            let program = owner.get(&port.pid).copied();
            let container = for_docker(&port.pid)
                .then(|| {
                    containers
                        .iter()
                        .find(|(published, _)| *published == port.port)
                        .map(|(_, name)| dev::shorten(name))
                })
                .flatten();
            let name = match (&container, program) {
                (Some(container), _) => format!("{container} (Docker)"),
                (None, None) => "Windows".to_string(),
                (None, Some(program)) => match &program.project {
                    Some(project) => format!("{} ({project})", program.name),
                    None => program.name.clone(),
                },
            };
            hk_core::dev::PortUse {
                port: port.port,
                address: port.address,
                program: program.map(|program| program.id),
                can_stop: container.is_none() && program.is_some_and(|program| program.can_stop),
                name,
                container,
            }
        })
        .take(MAX_LISTED)
        .collect();

    let build_helpers = programs
        .iter()
        .filter(|program| program.kind == ProgramKind::BuildHelper)
        .cloned()
        .collect();
    programs.retain(|program| program.kind != ProgramKind::BuildHelper);
    programs.truncate(MAX_LISTED);
    ProgramsReport {
        programs,
        build_helpers,
        ports,
        memory_total,
        memory_used,
    }
}

/// Stops a program and what it started, the last started first. It has to be the same
/// program that was seen: Windows gives the number of one that's gone to new ones.
pub(super) fn stop(program: ProgramId) -> Result<String, String> {
    let processes = snapshot();
    let members = dev::members_of(&processes, program);
    if members.is_empty() {
        return Err("it isn't running any more".into());
    }
    let named = dev::group(&processes)
        .into_iter()
        .find(|found| members.contains(&found.id))
        .map_or_else(
            || "it".to_string(),
            |found| match found.project {
                Some(project) => format!("{} ({project})", found.name),
                None => found.name,
            },
        );
    let unstoppable = members.iter().any(|member| {
        processes
            .iter()
            .any(|process| process.id == *member && !process.can_stop)
    });
    if unstoppable {
        return Err(format!("{named} is part of Windows, or isn't this user's"));
    }
    let ending: Vec<Owned<HANDLE>> = members
        .iter()
        .rev()
        .filter_map(|member| terminate(*member))
        .collect();
    let stopped = ending.len();
    // Windows only starts ending them. Waiting means a look straight after doesn't still see them.
    let deadline = Instant::now() + ENDING_TAKES_AT_MOST;
    for handle in &ending {
        let left = deadline.saturating_duration_since(Instant::now());
        // SAFETY: the handle is open, with the right to wait on it.
        unsafe { WaitForSingleObject(**handle, left.as_millis() as u32) };
    }
    if stopped == 0 {
        return Err(format!("Windows wouldn't stop {named}"));
    }
    Ok(match stopped {
        1 => format!("Stopped {named}"),
        _ => format!("Stopped {named}, {stopped} processes"),
    })
}

/// How long stopping a program waits for its processes to be gone.
const ENDING_TAKES_AT_MOST: Duration = Duration::from_secs(3);

/// Starts ending a process, and gives it to wait on if that worked.
fn terminate(process: ProgramId) -> Option<Owned<HANDLE>> {
    // SAFETY: the handle is owned as soon as it exists. It's checked to be the same process
    // before it's stopped.
    unsafe {
        let handle = OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            process.pid,
        )
        .ok()?;
        let handle = Owned::new(handle);
        (started(*handle) == process.started && TerminateProcess(*handle, 1).is_ok())
            .then_some(handle)
    }
}

/// Stops every build helper. .NET's are asked to stop the way .NET does it first, so that
/// they finish what they're writing.
pub(super) fn stop_build_helpers() -> Result<String, String> {
    let helpers = |processes: &[ProcessInfo]| -> Vec<Program> {
        dev::group(processes)
            .into_iter()
            .filter(|program| program.kind == ProgramKind::BuildHelper)
            .collect()
    };
    let before = helpers(&snapshot());
    if before.is_empty() {
        return Ok("No build helpers were running".into());
    }
    let dotnet = before.iter().any(|helper| {
        ["C# compiler server", "MSBuild node", "Razor build server"].contains(&helper.name.as_str())
    });
    if dotnet {
        let _ = tools::run("dotnet", &["build-server", "shutdown"], tools::SLOW);
    }
    let mut problems = Vec::new();
    for helper in helpers(&snapshot()) {
        if let Err(problem) = stop(helper.id) {
            problems.push(problem);
        }
    }
    let stopped = before.len() - problems.len().min(before.len());
    match problems.first() {
        None => Ok(format!("Stopped {stopped} build helpers")),
        Some(problem) if stopped > 0 => Ok(format!(
            "Stopped {stopped} build helpers. One couldn't be: {problem}"
        )),
        Some(problem) => Err(problem.clone()),
    }
}
