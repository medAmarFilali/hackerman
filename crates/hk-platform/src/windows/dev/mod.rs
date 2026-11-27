//! Developer mode on Windows: see `hk_core::dev`.

mod disk;
pub mod docker;
mod ports;
pub(super) mod processes;
mod tools;

use std::path::PathBuf;

use hk_core::dev::{DevReply, DevRequest};

/// Answers a request about this computer, doing what it asks if it asks for something. It can
/// take minutes, so it mustn't be called where waiting holds anything up. `folders` are where
/// to look for projects, or none for the usual places.
pub(super) fn answer(request: &DevRequest, folders: &[PathBuf]) -> DevReply {
    let done = |result: Result<String, String>| match result {
        Ok(summary) => DevReply::Done { summary },
        Err(reason) => DevReply::Refused { reason },
    };
    match request {
        DevRequest::Programs => DevReply::Programs(processes::report()),
        DevRequest::Stop { program } => done(processes::stop(*program)),
        DevRequest::StopBuildHelpers => done(processes::stop_build_helpers()),
        DevRequest::Versions => DevReply::Versions {
            tools: tools::versions(),
        },
        DevRequest::Docker => DevReply::Docker(docker::report()),
        DevRequest::DockerContainer { id, action } => done(docker::act(id, *action)),
        DevRequest::StopAllContainers => done(docker::stop_all()),
        DevRequest::Disk => DevReply::Disk(disk::report(folders)),
        DevRequest::Clean { items } => DevReply::Cleaned {
            results: disk::clean(items, folders),
        },
        DevRequest::CompactDockerDisk => done(docker::compact()),
    }
}

/// Only looks: `cargo test -p hk-platform dev -- --nocapture` shows what's using memory here.
#[cfg(test)]
mod tests {
    use hk_core::dev::ProgramKind;

    use super::*;

    #[test]
    fn whats_using_memory_here_makes_sense() {
        let DevReply::Programs(report) = answer(&DevRequest::Programs, &[]) else {
            panic!("not a list of programs");
        };
        for program in report.programs.iter().take(15) {
            println!(
                "{:>8.0} MB  {:<22} {:<28} {} processes{}",
                program.memory as f64 / 1e6,
                format!("{:?}", program.kind),
                format!(
                    "{}{}",
                    program.name,
                    program
                        .project
                        .as_ref()
                        .map(|p| format!(" ({p})"))
                        .unwrap_or_default()
                ),
                program.processes,
                if program.can_stop {
                    ""
                } else {
                    ", can't be stopped"
                }
            );
        }
        for helper in &report.build_helpers {
            println!(
                "build helper: {} ({} MB)",
                helper.name,
                helper.memory / 1_000_000
            );
        }
        for port in report.ports.iter().take(15) {
            println!(
                "port {:>5} on {:<15} {}",
                port.port, port.address, port.name
            );
        }
        assert!(report.memory_total > report.memory_used && report.memory_used > 0);
        assert!(!report.programs.is_empty());
        assert!(
            report
                .programs
                .windows(2)
                .all(|pair| pair[0].memory >= pair[1].memory)
        );
        assert!(
            report
                .build_helpers
                .iter()
                .all(|helper| helper.kind == ProgramKind::BuildHelper)
        );
        // This test program itself is seen, and could be stopped.
        assert!(report.programs.iter().any(|program| program.can_stop));
    }

    /// Starts a program of its own and stops it, as the Stop button does.
    #[test]
    fn a_program_and_what_it_started_are_stopped() {
        use std::os::windows::process::CommandExt;
        // A shell that starts a second one, which waits: one program of two processes.
        let mut started = std::process::Command::new("cmd")
            .args(["/c", "cmd /c ping -n 120 127.0.0.1 > nul"])
            .creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(800));
        let ping = processes::snapshot()
            .into_iter()
            .find(|process| {
                process.name.eq_ignore_ascii_case("PING.EXE")
                    && process
                        .command_line
                        .as_deref()
                        .is_some_and(|line| line.contains("-n 120"))
            })
            .expect("the program it started");
        let stopped = processes::stop(ping.id);
        assert!(stopped.is_ok(), "{stopped:?}");
        assert!(
            !processes::snapshot()
                .iter()
                .any(|process| process.id == ping.id),
            "it's gone"
        );
        // Stopping something that's gone says so.
        assert!(processes::stop(ping.id).is_err());
        let _ = started.kill();
        let _ = started.wait();
    }

    /// Only looks, in the folders projects are usually kept in. Run with `--nocapture`.
    #[test]
    fn the_disk_here_makes_sense() {
        let DevReply::Disk(report) = answer(&DevRequest::Disk, &[]) else {
            panic!("not a disk report");
        };
        println!("looked in {:?}", report.roots);
        for drive in &report.drives {
            println!(
                "{} {:.0} of {:.0} GB free",
                drive.name,
                drive.free as f64 / 1e9,
                drive.total as f64 / 1e9
            );
        }
        for folder in report.folders.iter().take(12) {
            println!(
                "{:>8.0} MB  {:<12} {:<28} {}{}{}",
                folder.size as f64 / 1e6,
                format!("{:?}", folder.kind),
                folder.project,
                folder.path,
                if folder.in_use { "  (in use)" } else { "" },
                if folder.shared_files { "  (pnpm)" } else { "" },
            );
        }
        for cache in &report.caches {
            println!(
                "{:>8.0} MB  {:?} {}",
                cache.size as f64 / 1e6,
                cache.kind,
                cache.path
            );
        }
        assert!(!report.drives.is_empty());
        assert!(
            report
                .folders
                .windows(2)
                .all(|pair| pair[0].size >= pair[1].size)
        );
    }

    #[test]
    fn hackerman_and_windows_itself_are_never_stopped() {
        let processes = processes::snapshot();
        if let Some(explorer) = processes
            .iter()
            .find(|process| process.name.eq_ignore_ascii_case("explorer.exe"))
        {
            assert!(!explorer.can_stop);
            assert!(processes::stop(explorer.id).is_err());
        }
        let own = std::process::id();
        assert!(
            processes
                .iter()
                .any(|process| process.id.pid == own && !process.can_stop)
        );
    }
}
