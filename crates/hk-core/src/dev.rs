//! Developer mode: what's using a device's memory, its ports, its disk space and Docker, and
//! cleaning up after builds. Another device can ask for all of this, and act on it, only if the
//! device it's about allows that.
//!
//! What's here is what every device agrees on, and the parts that need no operating system:
//! recognising developer tools from a program's command line, putting processes together into
//! the programs people know, and how much harm each kind of clean-up can do.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// Far more than a list should hold. Other devices send these.
pub const MAX_LISTED: usize = 500;
/// Far longer than a name, a path or a command line shown here should be.
pub const MAX_TEXT: usize = 600;

/// A process, which stays the same process even if Windows gives its number to another later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramId {
    pub pid: u32,
    /// When it started, by the operating system's clock.
    #[serde(with = "exact_in_text")]
    pub started: u64,
}

/// Written out as text for the app, whose numbers can't hold one this large exactly. The app has
/// to give it back as it was, to say which program to stop. Between devices it's a number.
mod exact_in_text {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.collect_str(value)
        } else {
            serializer.serialize_u64(*value)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        if deserializer.is_human_readable() {
            String::deserialize(deserializer)?
                .parse()
                .map_err(serde::de::Error::custom)
        } else {
            u64::deserialize(deserializer)
        }
    }
}

/// Something to ask a device about, or ask it to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DevRequest {
    /// What's using memory, the build helpers left running and the ports in use.
    Programs,
    /// Stops a program, and everything it started.
    Stop {
        program: ProgramId,
    },
    /// Stops every build helper.
    StopBuildHelpers,
    /// Which developer tools are installed, and their versions.
    Versions,
    Docker,
    DockerContainer {
        id: String,
        action: ContainerAction,
    },
    StopAllContainers,
    /// The drives, and the build folders and caches taking space.
    Disk,
    Clean {
        items: Vec<CleanItem>,
    },
    /// Shrinks the file Docker keeps its disk in, after space was freed inside it.
    CompactDockerDisk,
}

impl DevRequest {
    /// Whether this changes anything, rather than only looking.
    pub fn acts(&self) -> bool {
        !matches!(
            self,
            Self::Programs | Self::Versions | Self::Docker | Self::Disk
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContainerAction {
    Stop,
    Restart,
    Start,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DevReply {
    Programs(ProgramsReport),
    /// What a request that changes something did.
    Done {
        summary: String,
    },
    Versions {
        tools: Vec<ToolVersion>,
    },
    Docker(DockerReport),
    Disk(DiskReport),
    Cleaned {
        results: Vec<CleanResult>,
    },
    /// It wasn't done, for this reason.
    Refused {
        reason: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramsReport {
    /// Most memory first.
    pub programs: Vec<Program>,
    pub build_helpers: Vec<Program>,
    pub ports: Vec<PortUse>,
    /// The device's memory, in bytes.
    pub memory_total: u64,
    pub memory_used: u64,
}

/// One program as people know it, which can be several processes: a Next.js dev server with
/// the processes it started, or every window of a browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Program {
    pub id: ProgramId,
    pub kind: ProgramKind,
    /// Like "Next.js" or "chrome".
    pub name: String,
    /// The project it's for, if that can be told, like "hackerman-site".
    pub project: Option<String>,
    /// All its processes together, in bytes.
    pub memory: u64,
    pub processes: u32,
    /// Whether this device would let it be stopped: not one of Windows' own, and not Hackerman.
    pub can_stop: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProgramKind {
    NextJs,
    Vite,
    /// Another web framework's dev server, named in the program's name.
    WebDevServer,
    Node,
    DotNet,
    Rust,
    Docker,
    /// Something that speeds up builds by staying in the background after them.
    BuildHelper,
    /// Works for a code editor, which would miss it.
    EditorHelper,
    PackageManager,
    Other,
}

impl ProgramKind {
    /// Which kind names a group of processes, when they're of several kinds.
    fn rank(self) -> u8 {
        match self {
            Self::NextJs | Self::Vite | Self::WebDevServer => 6,
            Self::DotNet | Self::Rust => 5,
            Self::Node => 4,
            Self::Docker => 3,
            Self::EditorHelper | Self::BuildHelper => 2,
            Self::PackageManager => 1,
            Self::Other => 0,
        }
    }

    /// Whether it's a developer tool, whose processes go together by who started whom.
    /// Everything else goes together by name.
    fn is_dev(self) -> bool {
        self != Self::Other
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortUse {
    pub port: u16,
    /// Where it listens: "0.0.0.0" or "::" is every network, "127.0.0.1" only this device.
    pub address: String,
    /// The program listening, if it could be told.
    pub program: Option<ProgramId>,
    pub name: String,
    pub can_stop: bool,
    /// The Docker container it's for. Docker Desktop is what listens, which isn't the program
    /// to stop: the container is.
    pub container: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolVersion {
    pub tool: String,
    /// None when it isn't installed.
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerReport {
    /// Whether Docker is installed and running. Nothing else is filled in without it.
    pub running: bool,
    pub installed: bool,
    pub containers: Vec<Container>,
    pub usage: Vec<DockerUsage>,
    /// The file Docker keeps its disk in, which doesn't shrink by itself.
    pub disk_image: Option<DiskImage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub running: bool,
    /// Like "Up 2 hours" or "Exited (0) 3 days ago".
    pub status: String,
    pub memory: Option<u64>,
    /// Percent of one processor.
    pub cpu: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerUsage {
    pub kind: DockerClean,
    /// In bytes, as Docker says.
    pub size: u64,
    pub reclaimable: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskImage {
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskReport {
    pub drives: Vec<Drive>,
    /// Largest first.
    pub folders: Vec<DevFolder>,
    pub caches: Vec<Cache>,
    /// The folders looked in for projects.
    pub roots: Vec<String>,
    pub docker_image: Option<DiskImage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    /// Like "C:".
    pub name: String,
    pub total: u64,
    pub free: u64,
}

/// A folder a build or install made, which comes back by building or installing again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevFolder {
    pub path: String,
    pub kind: FolderKind,
    /// The project's name, or its folder's.
    pub project: String,
    pub size: u64,
    /// When the project was last changed, in milliseconds since 1970.
    pub last_used: Option<u64>,
    /// A running program is using it, so it isn't deleted.
    pub in_use: bool,
    /// Its files are shared with a store elsewhere, as pnpm does, so deleting it frees less.
    pub shared_files: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FolderKind {
    NodeModules,
    RustTarget,
    /// A .NET project's `bin` or `obj`.
    DotNetBuild,
    NextBuild,
}

impl FolderKind {
    /// The folders it can be, and the file next to it that says what the project is.
    pub fn of(folder_name: &str, has: impl Fn(&str) -> bool) -> Option<Self> {
        match folder_name.to_ascii_lowercase().as_str() {
            "node_modules" if has("package.json") => Some(Self::NodeModules),
            "target" if has("Cargo.toml") => Some(Self::RustTarget),
            "bin" | "obj" if has("*.csproj") || has("*.fsproj") || has("*.vbproj") => {
                Some(Self::DotNetBuild)
            }
            ".next" if has("package.json") => Some(Self::NextBuild),
            _ => None,
        }
    }

    /// What gets it back.
    pub fn restore(self) -> &'static str {
        match self {
            Self::NodeModules => "npm install, pnpm install or yarn",
            Self::RustTarget => "cargo build",
            Self::DotNetBuild => "dotnet build",
            Self::NextBuild => "next dev or next build",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cache {
    pub kind: CacheKind,
    pub path: String,
    pub size: u64,
}

/// Packages downloaded once to save downloading them again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CacheKind {
    Npm,
    PnpmStore,
    NuGetPackages,
    NuGetHttp,
    CargoRegistry,
}

/// What Docker can clean up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DockerClean {
    BuildCache,
    /// Images no other image or container uses, with no name.
    DanglingImages,
    /// Every image no container uses.
    UnusedImages,
    StoppedContainers,
    /// Volumes no container uses, named ones included.
    UnusedVolumes,
}

/// Something to clean up.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CleanItem {
    Folder { path: String },
    Cache { kind: CacheKind },
    Docker { kind: DockerClean },
}

/// How much harm cleaning something up can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Risk {
    /// It comes back by itself, when it's needed.
    Safe,
    /// It comes back by building or installing, which takes time and the internet.
    SlowerNextBuild,
    /// What's in it may exist nowhere else.
    CanLoseData,
}

impl CleanItem {
    pub fn risk(&self) -> Risk {
        match self {
            Self::Cache { .. }
            | Self::Docker {
                kind: DockerClean::BuildCache | DockerClean::DanglingImages,
            } => Risk::Safe,
            Self::Folder { .. }
            | Self::Docker {
                kind: DockerClean::UnusedImages,
            } => Risk::SlowerNextBuild,
            Self::Docker {
                kind: DockerClean::StoppedContainers | DockerClean::UnusedVolumes,
            } => Risk::CanLoseData,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanResult {
    pub item: CleanItem,
    /// In bytes, as far as it can be told.
    pub freed: u64,
    /// Why it wasn't cleaned, or only partly.
    pub problem: Option<String>,
}

/// A program on a device using more memory than its user said is too much.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAlert {
    pub name: String,
    pub project: Option<String>,
    pub memory: u64,
    pub limit: u64,
}

// What a process is.

/// A process, as the operating system describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessInfo {
    pub id: ProgramId,
    /// The process that started it, if that's still running.
    pub parent: Option<u32>,
    /// Its program's file name, like "node.exe".
    pub name: String,
    pub command_line: Option<String>,
    pub memory: u64,
    pub can_stop: bool,
}

/// What a process turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recognised {
    pub kind: ProgramKind,
    pub name: String,
    pub project: Option<String>,
}

fn file_stem(name: &str) -> &str {
    let name = name.rsplit(['\\', '/']).next().unwrap_or(name);
    name.strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .or_else(|| name.strip_suffix(".dll"))
        .unwrap_or(name)
}

/// The name of the folder just before `marker` in a path, like "site" in
/// `C:\code\site\node_modules\next\...`.
fn folder_before(path: &str, marker: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase().replace('/', "\\");
    let at = lower.find(marker)?;
    let before = &path[..at];
    let name = before
        .rsplit(['\\', '/'])
        .next()
        .map(|name| name.trim_matches('"'))?;
    (!name.is_empty() && !name.ends_with(':')).then(|| name.to_string())
}

/// Recognises a developer tool from its program's name and command line.
pub fn recognise(name: &str, command_line: Option<&str>) -> Recognised {
    let exe = name.to_ascii_lowercase();
    let command = command_line.unwrap_or_default();
    let lower = command.to_ascii_lowercase().replace('/', "\\");
    let has = |text: &str| lower.contains(text);
    let node_project = || folder_before(command, "\\node_modules\\");
    let made = |kind, name: &str, project| Recognised {
        kind,
        name: name.to_string(),
        project,
    };

    match exe.as_str() {
        "node.exe" | "bun.exe" | "deno.exe" | "node" | "bun" | "deno" => {
            if has("\\next\\dist\\") || has("next-server") || has(" next dev") {
                made(ProgramKind::NextJs, "Next.js", node_project())
            } else if has("\\vite\\bin\\") || has("\\vite\\dist\\") {
                made(ProgramKind::Vite, "Vite", node_project())
            } else if let Some(framework) = [
                ("\\nuxt\\", "Nuxt"),
                ("\\astro\\", "Astro"),
                ("\\@remix-run\\", "Remix"),
                ("\\@angular\\cli\\", "Angular"),
                ("\\@nestjs\\cli\\", "NestJS"),
                ("\\webpack-dev-server\\", "webpack"),
                ("\\react-scripts\\", "React"),
                ("\\@sveltejs\\kit\\", "SvelteKit"),
            ]
            .iter()
            .find_map(|(marker, name)| has(marker).then_some(*name))
            {
                made(ProgramKind::WebDevServer, framework, node_project())
            } else if has("tsserver.js")
                || has("typescript-language-server")
                || has("\\eslint\\")
                || has("languageserver")
                || has("--type=extensionhost")
            {
                made(ProgramKind::EditorHelper, "Editor helper", None)
            } else if has("pnpm.cjs") || has("pnpm.js") {
                made(ProgramKind::PackageManager, "pnpm", None)
            } else if has("npm-cli.js") {
                made(ProgramKind::PackageManager, "npm", None)
            } else if has("yarn.js") || has("yarn.cjs") {
                made(ProgramKind::PackageManager, "yarn", None)
            } else {
                let label = match exe.as_str() {
                    "bun.exe" | "bun" => "Bun",
                    "deno.exe" | "deno" => "Deno",
                    _ => "Node.js",
                };
                made(ProgramKind::Node, label, node_project())
            }
        }
        "dotnet.exe" | "dotnet" => {
            if has("vbcscompiler.dll") {
                made(ProgramKind::BuildHelper, "C# compiler server", None)
            } else if has("msbuild.dll") && (has("nodemode") || has("nodereuse")) {
                made(ProgramKind::BuildHelper, "MSBuild node", None)
            } else if has("rzc.dll") || has("razor") && has("server") {
                made(ProgramKind::BuildHelper, "Razor build server", None)
            } else if has("languageserver") || has("omnisharp") || has("devkit") {
                made(ProgramKind::EditorHelper, "Editor helper", None)
            } else if has(" watch") {
                made(ProgramKind::DotNet, "dotnet watch", None)
            } else if let Some(dll) = command
                .split('"')
                .flat_map(|part| part.split(' '))
                .find(|part| part.to_ascii_lowercase().ends_with(".dll"))
            {
                made(
                    ProgramKind::DotNet,
                    ".NET",
                    Some(file_stem(dll).to_string()),
                )
            } else {
                made(ProgramKind::DotNet, ".NET", None)
            }
        }
        "vbcscompiler.exe" => made(ProgramKind::BuildHelper, "C# compiler server", None),
        "msbuild.exe" if has("nodemode") || has("nodereuse") => {
            made(ProgramKind::BuildHelper, "MSBuild node", None)
        }
        "msbuild.exe" => made(ProgramKind::DotNet, "MSBuild", None),
        "esbuild.exe" if has("--service") => {
            made(ProgramKind::BuildHelper, "esbuild service", None)
        }
        "turbo.exe" if has("daemon") => made(ProgramKind::BuildHelper, "Turborepo daemon", None),
        "java.exe" | "javaw.exe" if has("gradledaemon") => {
            made(ProgramKind::BuildHelper, "Gradle daemon", None)
        }
        "cargo.exe" | "rustc.exe" => made(
            ProgramKind::Rust,
            if exe == "cargo.exe" {
                "Cargo"
            } else {
                "Rust compiler"
            },
            None,
        ),
        "rust-analyzer.exe" => made(ProgramKind::EditorHelper, "rust-analyzer", None),
        "docker desktop.exe" | "com.docker.backend.exe" | "com.docker.build.exe" => {
            made(ProgramKind::Docker, "Docker Desktop", None)
        }
        "vmmemwsl" | "vmmem" | "vmmemwsl.exe" | "vmmem.exe" => {
            made(ProgramKind::Docker, "Docker and WSL", None)
        }
        _ => {
            let stem = file_stem(name).to_string();
            if let Some(project) = folder_before(command, "\\target\\debug\\")
                .or_else(|| folder_before(command, "\\target\\release\\"))
            {
                made(ProgramKind::Rust, &stem, Some(project))
            } else if has("\\bin\\debug\\") || has("\\bin\\release\\") {
                made(ProgramKind::DotNet, ".NET", Some(stem))
            } else {
                made(ProgramKind::Other, &stem, None)
            }
        }
    }
}

/// Puts processes together into programs. Developer tools go together with what they
/// started, so a dev server counts all its processes. Anything else goes together by name,
/// as Task Manager does.
pub fn group(processes: &[ProcessInfo]) -> Vec<Program> {
    let by_pid: HashMap<u32, &ProcessInfo> = processes.iter().map(|p| (p.id.pid, p)).collect();
    let recognised: HashMap<u32, Recognised> = processes
        .iter()
        .map(|p| (p.id.pid, recognise(&p.name, p.command_line.as_deref())))
        .collect();
    // A parent only counts if it started before its child: Windows gives a finished process's
    // number to new ones.
    let parent_of = |process: &ProcessInfo| {
        process
            .parent
            .and_then(|pid| by_pid.get(&pid))
            .filter(|parent| parent.id.started <= process.id.started && parent.id != process.id)
            .copied()
    };

    // Each developer tool belongs to the furthest developer tool up the line that started it.
    let mut groups: BTreeMap<GroupKey, Vec<&ProcessInfo>> = BTreeMap::new();
    for process in processes {
        let kind = recognised[&process.id.pid].kind;
        let key = if kind.is_dev() {
            let mut root = process;
            let mut steps = 0;
            while let Some(parent) = parent_of(root)
                && recognised[&parent.id.pid].kind.is_dev()
                && steps < 64
            {
                root = parent;
                steps += 1;
            }
            GroupKey::Tree(root.id)
        } else {
            GroupKey::Name(process.name.to_ascii_lowercase())
        };
        groups.entry(key).or_default().push(process);
    }

    let mut programs: Vec<Program> = groups
        .into_values()
        .map(|members| {
            // Named after the most telling of them, and the biggest of those.
            let telling = members
                .iter()
                .max_by_key(|p| (recognised[&p.id.pid].kind.rank(), p.memory))
                .copied()
                .unwrap_or(members[0]);
            let what = &recognised[&telling.id.pid];
            let project = what.project.clone().or_else(|| {
                members
                    .iter()
                    .find_map(|p| recognised[&p.id.pid].project.clone())
            });
            // A group by name is stopped by its biggest process's id, which stands for all.
            let id = if what.kind.is_dev() {
                members
                    .iter()
                    .min_by_key(|p| p.id.started)
                    .map_or(telling.id, |p| p.id)
            } else {
                members
                    .iter()
                    .max_by_key(|p| p.memory)
                    .map_or(telling.id, |p| p.id)
            };
            Program {
                id,
                kind: what.kind,
                name: what.name.clone(),
                project,
                memory: members.iter().map(|p| p.memory).sum(),
                processes: members.len() as u32,
                can_stop: members.iter().all(|p| p.can_stop),
            }
        })
        .collect();
    programs.sort_by(|a, b| b.memory.cmp(&a.memory).then(a.name.cmp(&b.name)));
    programs
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum GroupKey {
    Tree(ProgramId),
    Name(String),
}

/// The processes that make up a program, to stop it: everything grouped with it, as [`group`]
/// does. Empty if it's gone.
pub fn members_of(processes: &[ProcessInfo], program: ProgramId) -> Vec<ProgramId> {
    let Some(target) = processes.iter().find(|p| p.id == program) else {
        return Vec::new();
    };
    let what = recognise(&target.name, target.command_line.as_deref());
    if !what.kind.is_dev() {
        let name = target.name.to_ascii_lowercase();
        return processes
            .iter()
            .filter(|p| {
                p.name.to_ascii_lowercase() == name
                    && !recognise(&p.name, p.command_line.as_deref()).kind.is_dev()
            })
            .map(|p| p.id)
            .collect();
    }
    // Everything it started, and what they started, that's a developer tool too.
    let mut members = vec![target.id];
    let mut at = 0;
    while at < members.len() {
        let parent = members[at];
        for child in processes {
            let started_by = child.parent == Some(parent.pid) && child.id.started >= parent.started;
            if started_by
                && !members.contains(&child.id)
                && recognise(&child.name, child.command_line.as_deref())
                    .kind
                    .is_dev()
            {
                members.push(child.id);
            }
        }
        at += 1;
    }
    members
}

/// Watches for programs using more memory than allowed, and says so once for each, until it
/// has come back down or gone.
#[derive(Debug, Default)]
pub struct MemoryWatch {
    told: HashMap<ProgramId, u64>,
}

impl MemoryWatch {
    /// The programs newly over the limit.
    pub fn check(&mut self, programs: &[Program], limit: u64) -> Vec<MemoryAlert> {
        // Well below, so that one going up and down around the limit isn't said over and over.
        let calm = limit / 10 * 9;
        self.told.retain(|id, _| {
            programs
                .iter()
                .any(|program| program.id == *id && program.memory > calm)
        });
        let mut alerts = Vec::new();
        for program in programs.iter().filter(|program| program.memory > limit) {
            if self.told.insert(program.id, program.memory).is_none() {
                alerts.push(MemoryAlert {
                    name: program.name.clone(),
                    project: program.project.clone(),
                    memory: program.memory,
                    limit,
                });
            }
        }
        alerts
    }
}

/// Reads a size the way Docker writes it, like "1.234GB", "512MB" or "0B".
pub fn parse_docker_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let text = text.split_whitespace().next()?;
    let digits = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let number: f64 = number.parse().ok()?;
    let scale: f64 = match unit.to_ascii_uppercase().as_str() {
        "B" | "" => 1.0,
        "KB" | "K" => 1e3,
        "MB" | "M" => 1e6,
        "GB" | "G" => 1e9,
        "TB" | "T" => 1e12,
        "KIB" => 1024.0,
        "MIB" => 1024.0 * 1024.0,
        "GIB" => 1024.0 * 1024.0 * 1024.0,
        "TIB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * scale).round() as u64)
}

/// Cuts text from another device down to a sensible length.
pub fn shorten(text: &str) -> String {
    text.chars().take(MAX_TEXT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, parent: Option<u32>, name: &str, command: &str, mb: u64) -> ProcessInfo {
        ProcessInfo {
            id: ProgramId {
                pid,
                started: u64::from(pid),
            },
            parent,
            name: name.into(),
            command_line: Some(command.into()),
            memory: mb * 1_000_000,
            can_stop: true,
        }
    }

    #[test]
    fn dev_servers_are_recognised_with_their_project() {
        let next = recognise(
            "node.exe",
            Some(
                r#""C:\Program Files\nodejs\node.exe" C:\code\hackerman-site\node_modules\next\dist\bin\next dev"#,
            ),
        );
        assert_eq!(next.kind, ProgramKind::NextJs);
        assert_eq!(next.project.as_deref(), Some("hackerman-site"));

        let vite = recognise(
            "node.exe",
            Some(r"node C:/Users/one/code/app/node_modules/vite/bin/vite.js"),
        );
        assert_eq!(
            (vite.kind, vite.project.as_deref()),
            (ProgramKind::Vite, Some("app"))
        );

        let nest = recognise(
            "node.exe",
            Some(r"node C:\work\api\node_modules\@nestjs\cli\bin\nest.js start --watch"),
        );
        assert_eq!(
            (nest.kind, nest.name.as_str()),
            (ProgramKind::WebDevServer, "NestJS")
        );
    }

    #[test]
    fn dotnet_apps_and_build_helpers_are_told_apart() {
        let api = recognise(
            "dotnet.exe",
            Some(
                r#""C:\Program Files\dotnet\dotnet.exe" C:\code\Shop\Shop.Api\bin\Debug\net8.0\Shop.Api.dll"#,
            ),
        );
        assert_eq!(
            (api.kind, api.project.as_deref()),
            (ProgramKind::DotNet, Some("Shop.Api"))
        );

        let compiler = recognise(
            "dotnet.exe",
            Some(
                r#""C:\Program Files\dotnet\dotnet.exe" "C:\Program Files\dotnet\sdk\8.0.100\Roslyn\bincore\VBCSCompiler.dll" "-pipename:x""#,
            ),
        );
        assert_eq!(compiler.kind, ProgramKind::BuildHelper);

        let node = recognise(
            "dotnet.exe",
            Some(
                r#"dotnet "C:\Program Files\dotnet\sdk\8.0.100\MSBuild.dll" /nologo /nodemode:1 /nodeReuse:true"#,
            ),
        );
        assert_eq!(
            (node.kind, node.name.as_str()),
            (ProgramKind::BuildHelper, "MSBuild node")
        );

        let apphost = recognise(
            "Shop.Api.exe",
            Some(r"C:\code\Shop\Shop.Api\bin\Debug\net8.0\Shop.Api.exe"),
        );
        assert_eq!(
            (apphost.kind, apphost.project.as_deref()),
            (ProgramKind::DotNet, Some("Shop.Api"))
        );
    }

    #[test]
    fn rust_and_everything_else() {
        let built = recognise(
            "hackerman.exe",
            Some(r"C:\code\hackerman\target\debug\hackerman.exe"),
        );
        assert_eq!(
            (built.kind, built.project.as_deref()),
            (ProgramKind::Rust, Some("hackerman"))
        );
        assert_eq!(
            recognise("rust-analyzer.exe", None).kind,
            ProgramKind::EditorHelper
        );
        assert_eq!(
            recognise("esbuild.exe", Some("esbuild.exe --service=0.21.5 --ping")).kind,
            ProgramKind::BuildHelper
        );
        let chrome = recognise(
            "chrome.exe",
            Some(r#""C:\Program Files\Google\Chrome\Application\chrome.exe""#),
        );
        assert_eq!(
            (chrome.kind, chrome.name.as_str()),
            (ProgramKind::Other, "chrome")
        );
    }

    /// `pnpm dev` in a terminal: the shell, pnpm, Next.js, and the server it starts.
    fn a_next_dev_server() -> Vec<ProcessInfo> {
        vec![
            process(10, Some(1), "cmd.exe", "cmd.exe /c pnpm dev", 5),
            process(
                11,
                Some(10),
                "node.exe",
                r"node C:\Users\one\AppData\Roaming\npm\node_modules\pnpm\bin\pnpm.cjs dev",
                60,
            ),
            process(
                12,
                Some(11),
                "node.exe",
                r"node C:\code\site\node_modules\next\dist\bin\next dev",
                300,
            ),
            process(
                13,
                Some(12),
                "node.exe",
                r"node C:\code\site\node_modules\next\dist\server\lib\start-server.js",
                1500,
            ),
            process(20, None, "chrome.exe", "chrome.exe", 400),
            process(
                21,
                Some(20),
                "chrome.exe",
                "chrome.exe --type=renderer",
                600,
            ),
        ]
    }

    #[test]
    fn a_dev_server_counts_everything_it_started() {
        let programs = group(&a_next_dev_server());
        let next = programs
            .iter()
            .find(|p| p.kind == ProgramKind::NextJs)
            .unwrap();
        assert_eq!(next.name, "Next.js");
        assert_eq!(next.project.as_deref(), Some("site"));
        assert_eq!(
            next.processes, 3,
            "pnpm, next and its server, but not the shell"
        );
        assert_eq!(next.memory, 1_860_000_000);
        assert_eq!(next.id.pid, 11, "stopped from the top");

        let chrome = programs.iter().find(|p| p.name == "chrome").unwrap();
        assert_eq!((chrome.processes, chrome.memory), (2, 1_000_000_000));
        assert_eq!(programs[0].kind, ProgramKind::NextJs, "most memory first");
    }

    #[test]
    fn stopping_a_program_stops_what_it_started() {
        let processes = a_next_dev_server();
        let next = group(&processes)
            .into_iter()
            .find(|p| p.kind == ProgramKind::NextJs)
            .unwrap();
        let members: Vec<u32> = members_of(&processes, next.id)
            .iter()
            .map(|id| id.pid)
            .collect();
        assert_eq!(members, [11, 12, 13]);
        let chrome: Vec<u32> = members_of(
            &processes,
            ProgramId {
                pid: 21,
                started: 21,
            },
        )
        .iter()
        .map(|id| id.pid)
        .collect();
        assert_eq!(chrome, [20, 21]);
    }

    /// Windows hands out the number of a finished process again.
    #[test]
    fn a_process_with_a_number_used_again_is_a_different_one() {
        let processes = a_next_dev_server();
        assert!(
            members_of(
                &processes,
                ProgramId {
                    pid: 12,
                    started: 999
                }
            )
            .is_empty()
        );
        let mut adopted = processes.clone();
        // Started before its supposed parent, so that parent is someone else's old number.
        adopted.push(process(
            5,
            Some(13),
            "node.exe",
            r"node C:\other\node_modules\vite\bin\vite.js",
            10,
        ));
        let programs = group(&adopted);
        let vite = programs
            .iter()
            .find(|p| p.kind == ProgramKind::Vite)
            .unwrap();
        assert_eq!(vite.processes, 1);
    }

    #[test]
    fn one_that_cant_be_stopped_makes_the_program_unstoppable() {
        let mut processes = a_next_dev_server();
        processes[3].can_stop = false;
        let next = group(&processes)
            .into_iter()
            .find(|p| p.kind == ProgramKind::NextJs)
            .unwrap();
        assert!(!next.can_stop);
    }

    #[test]
    fn build_folders_are_known_by_the_project_file_beside_them() {
        let next_to = |files: &'static [&'static str]| move |file: &str| files.contains(&file);
        assert_eq!(
            FolderKind::of("node_modules", next_to(&["package.json"])),
            Some(FolderKind::NodeModules)
        );
        assert_eq!(
            FolderKind::of("target", next_to(&["Cargo.toml"])),
            Some(FolderKind::RustTarget)
        );
        assert_eq!(
            FolderKind::of("bin", next_to(&["*.csproj"])),
            Some(FolderKind::DotNetBuild)
        );
        assert_eq!(
            FolderKind::of("obj", next_to(&["*.fsproj"])),
            Some(FolderKind::DotNetBuild)
        );
        assert_eq!(
            FolderKind::of(".next", next_to(&["package.json"])),
            Some(FolderKind::NextBuild)
        );
        // Folders with those names that aren't a build's.
        assert_eq!(FolderKind::of("target", next_to(&["package.json"])), None);
        assert_eq!(FolderKind::of("bin", next_to(&["Cargo.toml"])), None);
        assert_eq!(FolderKind::of("node_modules", next_to(&[])), None);
    }

    #[test]
    fn what_can_lose_data_is_marked_so() {
        assert_eq!(
            CleanItem::Cache {
                kind: CacheKind::Npm
            }
            .risk(),
            Risk::Safe
        );
        assert_eq!(
            CleanItem::Docker {
                kind: DockerClean::BuildCache
            }
            .risk(),
            Risk::Safe
        );
        assert_eq!(
            CleanItem::Folder { path: "x".into() }.risk(),
            Risk::SlowerNextBuild
        );
        assert_eq!(
            CleanItem::Docker {
                kind: DockerClean::UnusedImages
            }
            .risk(),
            Risk::SlowerNextBuild
        );
        assert_eq!(
            CleanItem::Docker {
                kind: DockerClean::UnusedVolumes
            }
            .risk(),
            Risk::CanLoseData
        );
        assert_eq!(
            CleanItem::Docker {
                kind: DockerClean::StoppedContainers
            }
            .risk(),
            Risk::CanLoseData
        );
    }

    #[test]
    fn a_program_over_the_limit_is_said_once_until_it_comes_down() {
        let mut watch = MemoryWatch::default();
        let mut programs = group(&a_next_dev_server());
        let limit = 1_500_000_000;
        let alerts = watch.check(&programs, limit);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].name, "Next.js");
        assert!(watch.check(&programs, limit).is_empty(), "not again");

        // Down a little, still near the limit: nothing new.
        programs[0].memory = 1_400_000_000;
        assert!(watch.check(&programs, limit).is_empty());
        programs[0].memory = 1_600_000_000;
        assert!(watch.check(&programs, limit).is_empty());
        // Well down, then up again.
        programs[0].memory = 500_000_000;
        assert!(watch.check(&programs, limit).is_empty());
        programs[0].memory = 1_600_000_000;
        assert_eq!(watch.check(&programs, limit).len(), 1);
    }

    #[test]
    fn docker_sizes_are_read_as_written() {
        assert_eq!(parse_docker_size("0B"), Some(0));
        assert_eq!(parse_docker_size("512MB"), Some(512_000_000));
        assert_eq!(parse_docker_size("1.234GB"), Some(1_234_000_000));
        assert_eq!(parse_docker_size("2.5GB (40%)"), Some(2_500_000_000));
        assert_eq!(parse_docker_size("1.5GiB"), Some(1_610_612_736));
        assert_eq!(parse_docker_size("lots"), None);
    }

    /// Larger than JavaScript can hold exactly, as Windows' start times are.
    #[test]
    fn a_programs_start_time_reaches_the_app_and_back_exactly() {
        let id = ProgramId {
            pid: 4242,
            started: 133_745_678_901_234_567,
        };
        let text = serde_json::to_string(&id).unwrap();
        assert_eq!(text, r#"{"pid":4242,"started":"133745678901234567"}"#);
        assert_eq!(serde_json::from_str::<ProgramId>(&text).unwrap(), id);
        let bytes = postcard::to_allocvec(&id).unwrap();
        assert_eq!(postcard::from_bytes::<ProgramId>(&bytes).unwrap(), id);
    }

    #[test]
    fn only_what_changes_something_counts_as_acting() {
        assert!(!DevRequest::Programs.acts());
        assert!(!DevRequest::Disk.acts());
        assert!(DevRequest::StopBuildHelpers.acts());
        assert!(DevRequest::Clean { items: vec![] }.acts());
    }
}
