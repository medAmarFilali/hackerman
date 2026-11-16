//! Docker: its containers, the space it takes, cleaning that up, and shrinking the file its disk
//! is kept in. Everything goes through Docker's own `docker` command.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hk_core::dev::{
    self, Container, ContainerAction, DiskImage, DockerClean, DockerReport, DockerUsage,
};
use serde::Deserialize;
use windows::Win32::Foundation::ERROR_CANCELLED;
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
    SHELLEXECUTEINFOW, SHGetKnownFolderPath, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{HSTRING, Owned, PCWSTR, w};

use super::{processes, tools};

/// Given to this program, with administrator rights, to shrink Docker's disk file.
pub const COMPACT_ARGUMENT: &str = "--compact-docker-disk";

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Listed {
    #[serde(rename = "ID")]
    id: String,
    names: String,
    image: String,
    state: String,
    status: String,
    #[serde(default)]
    ports: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Stats {
    #[serde(rename = "ID")]
    id: String,
    mem_usage: String,
    #[serde(rename = "CPUPerc")]
    cpu_perc: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Usage {
    #[serde(rename = "Type")]
    kind: String,
    size: String,
    reclaimable: String,
}

/// One JSON object a line, as `--format "{{json .}}"` writes.
fn lines<T: for<'a> Deserialize<'a>>(text: &str) -> Vec<T> {
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub(super) fn report() -> DockerReport {
    let disk_image = disk_image();
    if tools::run("docker", &["--version"], tools::QUICK).is_err() {
        return DockerReport {
            disk_image,
            ..DockerReport::default()
        };
    }
    let running = tools::run(
        "docker",
        &["info", "--format", "{{.ServerVersion}}"],
        tools::QUICK,
    )
    .is_ok();
    if !running {
        return DockerReport {
            installed: true,
            disk_image,
            ..DockerReport::default()
        };
    }
    let listed: Vec<Listed> = tools::run(
        "docker",
        &["ps", "-a", "--format", "{{json .}}"],
        tools::QUICK,
    )
    .map(|text| lines(&text))
    .unwrap_or_default();
    let stats: Vec<Stats> = tools::run(
        "docker",
        &["stats", "--no-stream", "--format", "{{json .}}"],
        tools::QUICK,
    )
    .map(|text| lines(&text))
    .unwrap_or_default();
    let containers = listed
        .into_iter()
        .take(dev::MAX_LISTED)
        .map(|listed| {
            let stats = stats
                .iter()
                .find(|stats| stats.id.starts_with(&listed.id) || listed.id.starts_with(&stats.id));
            Container {
                running: listed.state == "running",
                memory: stats.and_then(|stats| {
                    dev::parse_docker_size(stats.mem_usage.split('/').next().unwrap_or_default())
                }),
                cpu: stats.and_then(|stats| stats.cpu_perc.trim_end_matches('%').parse().ok()),
                id: dev::shorten(&listed.id),
                name: dev::shorten(&listed.names),
                image: dev::shorten(&listed.image),
                status: dev::shorten(&listed.status),
            }
        })
        .collect();
    let usage = tools::run(
        "docker",
        &["system", "df", "--format", "{{json .}}"],
        tools::QUICK,
    )
    .map(|text| lines::<Usage>(&text))
    .unwrap_or_default()
    .into_iter()
    .filter_map(|usage| {
        let kind = match usage.kind.as_str() {
            "Images" => DockerClean::UnusedImages,
            "Containers" => DockerClean::StoppedContainers,
            "Local Volumes" => DockerClean::UnusedVolumes,
            "Build Cache" => DockerClean::BuildCache,
            _ => return None,
        };
        Some(DockerUsage {
            kind,
            size: dev::parse_docker_size(&usage.size).unwrap_or(0),
            reclaimable: dev::parse_docker_size(&usage.reclaimable).unwrap_or(0),
        })
    })
    .collect();
    DockerReport {
        running: true,
        installed: true,
        containers,
        usage,
        disk_image,
    }
}

/// The ports each running container has opened on this computer, by container name. Docker
/// Desktop is what Windows says is listening on them.
pub(super) fn published_ports() -> Vec<(u16, String)> {
    let Ok(text) = tools::run("docker", &["ps", "--format", "{{json .}}"], tools::QUICK) else {
        return Vec::new();
    };
    lines::<Listed>(&text)
        .into_iter()
        .flat_map(|listed| {
            host_ports(&listed.ports)
                .into_iter()
                .map(move |port| (port, listed.names.clone()))
        })
        .collect()
}

/// From Docker's "0.0.0.0:5432->5432/tcp, :::5432->5432/tcp", the ports on this side: 5432.
fn host_ports(ports: &str) -> Vec<u16> {
    let mut found: Vec<u16> = ports
        .split(", ")
        .filter_map(|mapping| {
            let (host, _) = mapping.split_once("->")?;
            host.rsplit(':').next()?.parse().ok()
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// A container's ID or name, which can't be mistaken for anything else on Docker's command line.
fn is_container(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

pub(super) fn act(id: &str, action: ContainerAction) -> Result<String, String> {
    if !is_container(id) {
        return Err("that isn't a container".into());
    }
    let (command, done) = match action {
        ContainerAction::Stop => ("stop", "Stopped"),
        ContainerAction::Restart => ("restart", "Restarted"),
        ContainerAction::Start => ("start", "Started"),
    };
    tools::run("docker", &[command, id], tools::SLOW)?;
    Ok(format!("{done} container {id}"))
}

pub(super) fn stop_all() -> Result<String, String> {
    let running = tools::run("docker", &["ps", "-q"], tools::QUICK)?;
    let ids: Vec<&str> = running
        .lines()
        .map(str::trim)
        .filter(|id| is_container(id))
        .collect();
    if ids.is_empty() {
        return Ok("No containers were running".into());
    }
    let mut arguments = vec!["stop"];
    arguments.extend(&ids);
    tools::run("docker", &arguments, tools::SLOW)?;
    Ok(format!("Stopped {} containers", ids.len()))
}

/// Cleans up one kind of thing, saying how much was freed, or what went wrong.
pub(super) fn clean(kind: DockerClean) -> (u64, Option<String>) {
    let arguments: &[&str] = match kind {
        DockerClean::BuildCache => &["builder", "prune", "-f"],
        DockerClean::DanglingImages => &["image", "prune", "-f"],
        DockerClean::UnusedImages => &["image", "prune", "-a", "-f"],
        DockerClean::StoppedContainers => &["container", "prune", "-f"],
        DockerClean::UnusedVolumes => &["volume", "prune", "-a", "-f"],
    };
    let mut result = tools::run("docker", arguments, tools::SLOW);
    // Docker before version 23 only ever removes volumes without a name, and has no `-a`.
    if kind == DockerClean::UnusedVolumes
        && let Err(problem) = &result
        && problem.contains("unknown")
    {
        result = tools::run("docker", &["volume", "prune", "-f"], tools::SLOW);
    }
    match result {
        Ok(text) => (reclaimed(&text), None),
        Err(problem) => (0, Some(problem)),
    }
}

/// What Docker says it freed, at the end of what it wrote.
fn reclaimed(text: &str) -> u64 {
    text.lines()
        .rev()
        .find_map(|line| {
            let (label, size) = line.split_once(':')?;
            let label = label.to_ascii_lowercase();
            (label.contains("reclaimed") || label.trim() == "total")
                .then(|| dev::parse_docker_size(size))
                .flatten()
        })
        .unwrap_or(0)
}

fn local_app_data() -> Option<PathBuf> {
    // SAFETY: Windows allocates the path, which is copied and then freed.
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(path.0.cast()));
        text.map(PathBuf::from)
    }
}

/// Where Docker Desktop keeps its disk, which has moved between versions.
fn docker_wsl_folder() -> Option<PathBuf> {
    Some(local_app_data()?.join("Docker").join("wsl"))
}

pub(super) fn disk_image() -> Option<DiskImage> {
    let folder = docker_wsl_folder()?;
    [
        ("disk", "docker_data.vhdx"),
        ("data", "ext4.vhdx"),
        ("main", "ext4.vhdx"),
    ]
    .iter()
    .map(|(inside, file)| folder.join(inside).join(file))
    .find_map(|path| {
        let size = std::fs::metadata(&path).ok()?.len();
        Some(DiskImage {
            path: path.display().to_string(),
            size,
        })
    })
}

/// Shrinks Docker's disk file: once Docker Desktop has been quit, with administrator
/// permission, which Windows asks the person at this computer for.
pub(super) fn compact() -> Result<String, String> {
    let Some(image) = disk_image() else {
        return Err("Docker Desktop's disk wasn't found".into());
    };
    let docker_running = processes::snapshot().iter().any(|process| {
        let name = process.name.to_ascii_lowercase();
        name == "docker desktop.exe" || name == "com.docker.backend.exe"
    });
    if docker_running {
        return Err("Quit Docker Desktop first, from its icon by the clock".into());
    }
    let program = std::env::current_exe().map_err(|error| error.to_string())?;
    let arguments = format!("{COMPACT_ARGUMENT} \"{}\"", image.path);
    match run_as_administrator(&program, &arguments) {
        Ok(0) => {}
        Ok(_) => return Err("Shrinking it didn't work".into()),
        Err(Declined) => return Err("Windows wasn't given permission".into()),
    }
    let after = std::fs::metadata(&image.path).map_or(image.size, |found| found.len());
    Ok(format!(
        "Docker's disk went from {} to {}",
        megabytes(image.size),
        megabytes(after)
    ))
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

struct Declined;

/// Runs this program with administrator rights, which Windows asks for, and waits for it.
fn run_as_administrator(program: &Path, arguments: &str) -> Result<u32, Declined> {
    let (program, arguments) = (HSTRING::from(program), HSTRING::from(arguments));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(program.as_ptr()),
        lpParameters: PCWSTR(arguments.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: the strings outlive the call, and the handle Windows gives back is owned here.
    unsafe {
        if let Err(error) = ShellExecuteExW(&raw mut info) {
            return if error.code() == ERROR_CANCELLED.to_hresult() {
                Err(Declined)
            } else {
                Ok(1)
            };
        }
        let process = Owned::new(info.hProcess);
        // Shrinking a large disk takes a while.
        WaitForSingleObject(*process, Duration::from_secs(60 * 60).as_millis() as u32);
        let mut code = 1;
        let _ = GetExitCodeProcess(*process, &raw mut code);
        Ok(code)
    }
}

/// Run with administrator rights: stops WSL, which Docker Desktop runs in, and shrinks the disk
/// file with Windows' own `diskpart`. The file has to be Docker Desktop's, in this user's own
/// folder, since this runs with rights the user's programs don't have.
pub fn compact_as_administrator(path: &str) -> Result<(), String> {
    let folder = docker_wsl_folder().ok_or("can't find this user's folder")?;
    let wanted = Path::new(path);
    let inside = wanted
        .to_string_lossy()
        .to_ascii_lowercase()
        .starts_with(&folder.to_string_lossy().to_ascii_lowercase());
    let is_disk = wanted
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("vhdx"));
    if !inside || !is_disk || path.contains(['"', '\n', '\r']) || path.contains("..") {
        return Err(format!("{path} isn't Docker Desktop's disk"));
    }
    if !wanted.is_file() {
        return Err(format!("{path} isn't there"));
    }
    let system = std::env::var_os("SystemRoot")
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
        .join("System32");
    let run = |program: &str, arguments: &[&str]| {
        std::process::Command::new(system.join(program))
            .args(arguments)
            .status()
            .map_err(|error| error.to_string())
    };
    run("wsl.exe", &["--shutdown"])?;
    let script = std::env::temp_dir().join("hackerman-compact-docker.txt");
    std::fs::write(
        &script,
        format!(
            "select vdisk file=\"{path}\"\r\nattach vdisk readonly\r\ncompact vdisk\r\ndetach vdisk\r\n"
        ),
    )
    .map_err(|error| error.to_string())?;
    let status = run("diskpart.exe", &["/s", &script.to_string_lossy()]);
    let _ = std::fs::remove_file(&script);
    match status? {
        status if status.success() => Ok(()),
        status => Err(format!("diskpart ended with {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_containers_id_or_name_goes_on_dockers_command_line() {
        assert!(is_container("3f2a9c1b0d4e"));
        assert!(is_container("my_postgres-1"));
        assert!(!is_container("--rm"));
        assert!(!is_container("a b"));
        assert!(!is_container(""));
    }

    #[test]
    fn a_containers_ports_on_this_side_are_read() {
        assert_eq!(
            host_ports("0.0.0.0:5432->5432/tcp, :::5432->5432/tcp"),
            [5432]
        );
        assert_eq!(
            host_ports("127.0.0.1:8080->80/tcp, 0.0.0.0:9000->9000/tcp"),
            [8080, 9000]
        );
        assert_eq!(
            host_ports("6379/tcp"),
            Vec::<u16>::new(),
            "not opened on this computer"
        );
        assert_eq!(host_ports(""), Vec::<u16>::new());
    }

    #[test]
    fn what_was_freed_is_read_from_what_docker_wrote() {
        assert_eq!(
            reclaimed("Deleted Images:\nuntagged: x\n\nTotal reclaimed space: 1.2GB"),
            1_200_000_000
        );
        assert_eq!(
            reclaimed("ID\tRECLAIMABLE\nabc\ttrue\nTotal:\t512MB"),
            512_000_000
        );
        assert_eq!(reclaimed("nothing to do"), 0);
    }

    #[test]
    fn docker_lists_are_read_a_line_at_a_time() {
        let text = r#"{"ID":"3f2a","Names":"db","Image":"postgres:16","State":"running","Status":"Up 2 hours"}
{"ID":"9b1c","Names":"old","Image":"redis","State":"exited","Status":"Exited (0) 3 days ago"}
not json"#;
        let listed: Vec<Listed> = lines(text);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[1].state, "exited");
    }

    /// Run with administrator rights, a file outside Docker's own folder must be turned away.
    #[test]
    fn only_docker_desktops_own_disk_is_shrunk() {
        assert!(compact_as_administrator(r"C:\Windows\System32\config\SYSTEM").is_err());
        assert!(compact_as_administrator(r"C:\other.vhdx").is_err());
        if let Some(folder) = docker_wsl_folder() {
            let sneaky = folder.join(r"..\..\..\..\Windows\disk.vhdx");
            assert!(compact_as_administrator(&sneaky.to_string_lossy()).is_err());
        }
    }

    /// Only looks.
    #[test]
    fn docker_here_answers() {
        let report = report();
        println!(
            "installed {}, running {}, {} containers, disk {:?}",
            report.installed,
            report.running,
            report.containers.len(),
            report.disk_image
        );
        if !report.installed {
            assert!(report.containers.is_empty());
        }
    }
}
