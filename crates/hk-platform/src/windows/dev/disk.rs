//! The drives, the folders builds and installs leave behind, the caches of downloaded
//! packages, and cleaning them up.
//!
//! Deleting is only ever done to a folder that's still what it was found to be: a build
//! folder of a project in one of the folders the user said to look in, which nothing running
//! is using. Another device asks for the path, so none of that can be taken on trust.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use hk_core::dev::{
    self, Cache, CacheKind, CleanItem, CleanResult, DevFolder, DiskReport, Drive, FolderKind,
    MAX_LISTED,
};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_Profile, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::HSTRING;

use super::{docker, processes, tools};

/// Windows' value for a drive inside the computer, rather than removable or on the network.
const DRIVE_FIXED: u32 = 3;
/// How deep to look for projects in a folder.
const MAX_DEPTH: usize = 8;
/// Where people usually keep their projects, in their own folder.
const USUAL_PLACES: [&str; 12] = [
    r"source\repos",
    r"Documents\code",
    r"Documents\GitHub",
    r"Documents\Projects",
    "code",
    "projects",
    "dev",
    "src",
    "repos",
    "git",
    "workspace",
    "Desktop",
];

fn profile() -> Option<PathBuf> {
    // SAFETY: Windows allocates the path, which is copied and then freed.
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_Profile, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0.cast()));
        text.map(PathBuf::from)
    }
}

/// The folders to look in: the ones the user chose, or the usual places that are there.
pub(super) fn roots(chosen: &[PathBuf]) -> Vec<PathBuf> {
    let candidates: Vec<PathBuf> = if chosen.is_empty() {
        profile().map_or_else(Vec::new, |home| {
            USUAL_PLACES.iter().map(|place| home.join(place)).collect()
        })
    } else {
        chosen.to_vec()
    };
    let mut roots: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|root| root.is_dir())
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    roots.sort();
    // One inside another is looked in already.
    roots.dedup_by(|inner, outer| inner.starts_with(&*outer));
    roots
}

pub(super) fn report(chosen: &[PathBuf]) -> DiskReport {
    let roots = roots(chosen);
    let running = running_command_lines();
    let found = find_build_folders(&roots);
    let mut folders: Vec<DevFolder> = std::thread::scope(|scope| {
        let measured: Vec<_> = found
            .chunks(found.len().div_ceil(8).max(1))
            .map(|chunk| {
                let (running, roots) = (&running, &roots);
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(path, kind)| describe(path, *kind, roots, running))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        measured
            .into_iter()
            .flat_map(|chunk| chunk.join().unwrap_or_default())
            .collect()
    });
    folders.sort_by_key(|folder| std::cmp::Reverse(folder.size));
    folders.truncate(MAX_LISTED);
    DiskReport {
        drives: drives(),
        folders,
        caches: caches(),
        roots: roots.iter().map(|root| plain(root)).collect(),
        docker_image: docker::disk_image(),
    }
}

/// A path without the `\\?\` that Windows puts in front of exact ones.
fn plain(path: &Path) -> String {
    let text = path.display().to_string();
    text.strip_prefix(r"\\?\")
        .map_or(text.clone(), str::to_string)
}

fn running_command_lines() -> Vec<String> {
    processes::snapshot()
        .into_iter()
        .filter_map(|process| process.command_line)
        .map(|line| line.to_ascii_lowercase().replace('/', "\\"))
        .collect()
}

/// Whether a running program was started from inside this project.
fn in_use(project: &Path, running: &[String]) -> bool {
    let project = plain(project).to_ascii_lowercase();
    let with_slash = format!("{}\\", project.trim_end_matches('\\'));
    running.iter().any(|line| line.contains(&with_slash))
}

fn find_build_folders(roots: &[PathBuf]) -> Vec<(PathBuf, FolderKind)> {
    let mut found = Vec::new();
    let mut waiting: Vec<(PathBuf, usize)> = roots.iter().map(|root| (root.clone(), 0)).collect();
    while let Some((folder, depth)) = waiting.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        let mut files = Vec::new();
        let mut folders = Vec::new();
        for entry in entries.flatten() {
            // Links lead elsewhere, possibly round in a circle.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_symlink() {
                continue;
            } else if kind.is_dir() {
                folders.push((name, entry.path()));
            } else {
                files.push(name.to_ascii_lowercase());
            }
        }
        let has = |wanted: &str| match wanted.strip_prefix('*') {
            Some(ending) => files.iter().any(|file| file.ends_with(ending)),
            None => files
                .iter()
                .any(|file| file == &wanted.to_ascii_lowercase()),
        };
        for (name, path) in folders {
            if let Some(kind) = FolderKind::of(&name, has) {
                found.push((path, kind));
            } else if depth < MAX_DEPTH && !skipped(&name) {
                waiting.push((path, depth + 1));
            }
        }
    }
    found
}

/// Folders never looked inside for projects.
fn skipped(name: &str) -> bool {
    name.starts_with('.')
        || name.starts_with('$')
        || [
            "node_modules",
            "appdata",
            "target",
            "bin",
            "obj",
            "dist",
            "build",
        ]
        .contains(&name.to_ascii_lowercase().as_str())
}

fn describe(path: &Path, kind: FolderKind, roots: &[PathBuf], running: &[String]) -> DevFolder {
    let project = path.parent().unwrap_or(path);
    DevFolder {
        path: plain(path),
        kind,
        project: dev::shorten(&project_name(project, roots)),
        size: size_of_folder(path),
        last_used: last_used(project),
        in_use: in_use(project, running),
        shared_files: kind == FolderKind::NodeModules
            && (path.join(".pnpm").is_dir() || path.join(".modules.yaml").is_file()),
    }
}

/// What the project goes by: where it is in the folder it was found in, like "permyemail" or
/// "teamlilitclient". Names inside projects are often copied from one to the next.
fn project_name(project: &Path, roots: &[PathBuf]) -> String {
    roots
        .iter()
        .filter_map(|root| project.strip_prefix(root).ok())
        .map(|inside| inside.display().to_string())
        .find(|inside| !inside.is_empty())
        .or_else(|| {
            project
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| plain(project))
}

/// When the project was last changed: its own files, not what's been built from it.
fn last_used(project: &Path) -> Option<u64> {
    let markers = [
        "",
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "Cargo.toml",
        "Cargo.lock",
        "src",
    ];
    markers
        .iter()
        .filter_map(|marker| {
            std::fs::metadata(project.join(marker))
                .ok()?
                .modified()
                .ok()
        })
        .filter_map(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as u64)
        .max()
}

/// Everything in it, not following links, which lead to files counted elsewhere.
pub(super) fn size_of_folder(folder: &Path) -> u64 {
    let mut total = 0;
    let mut waiting = vec![folder.to_path_buf()];
    while let Some(folder) = waiting.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            } else if kind.is_dir() {
                waiting.push(entry.path());
            } else if let Ok(metadata) = entry.metadata() {
                total += metadata.len();
            }
        }
    }
    total
}

fn drives() -> Vec<Drive> {
    let mut names = [0u16; 512];
    // SAFETY: the buffer's length goes with it.
    let length = unsafe { GetLogicalDriveStringsW(Some(&mut names)) } as usize;
    String::from_utf16_lossy(&names[..length.min(names.len())])
        .split('\0')
        .filter(|root| !root.is_empty())
        .filter_map(|root| {
            let path = HSTRING::from(root);
            // SAFETY: plain calls about a drive, filling in two plain values.
            unsafe {
                if GetDriveTypeW(&path) != DRIVE_FIXED {
                    return None;
                }
                let (mut free, mut total) = (0u64, 0u64);
                GetDiskFreeSpaceExW(&path, Some(&raw mut free), Some(&raw mut total), None).ok()?;
                Some(Drive {
                    name: root.trim_end_matches('\\').to_string(),
                    total,
                    free,
                })
            }
        })
        .collect()
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn cache_path(kind: CacheKind) -> Option<PathBuf> {
    let home = profile()?;
    let local = env_path("LOCALAPPDATA").unwrap_or_else(|| home.join(r"AppData\Local"));
    Some(match kind {
        CacheKind::Npm => local.join("npm-cache"),
        CacheKind::PnpmStore => local.join(r"pnpm\store"),
        CacheKind::NuGetPackages => {
            env_path("NUGET_PACKAGES").unwrap_or(home.join(r".nuget\packages"))
        }
        CacheKind::NuGetHttp => local.join(r"NuGet\v3-cache"),
        CacheKind::CargoRegistry => env_path("CARGO_HOME")
            .unwrap_or(home.join(".cargo"))
            .join("registry"),
    })
}

const CACHES: [CacheKind; 5] = [
    CacheKind::Npm,
    CacheKind::PnpmStore,
    CacheKind::NuGetPackages,
    CacheKind::NuGetHttp,
    CacheKind::CargoRegistry,
];

fn caches() -> Vec<Cache> {
    CACHES
        .iter()
        .filter_map(|&kind| {
            let path = cache_path(kind).filter(|path| path.is_dir())?;
            Some(Cache {
                kind,
                size: size_of_folder(&path),
                path: plain(&path),
            })
        })
        .collect()
}

/// Cleans each thing up as far as it can, saying how it went for each.
pub(super) fn clean(items: &[CleanItem], chosen: &[PathBuf]) -> Vec<CleanResult> {
    let roots = roots(chosen);
    let running = running_command_lines();
    items
        .iter()
        .take(MAX_LISTED)
        .map(|item| {
            let (freed, problem) = match item {
                CleanItem::Folder { path } => match checked_folder(path, &roots, &running) {
                    Ok(folder) => delete_folder(&folder),
                    Err(problem) => (0, Some(problem)),
                },
                CleanItem::Cache { kind } => clean_cache(*kind),
                CleanItem::Docker { kind } => docker::clean(*kind),
            };
            CleanResult {
                item: item.clone(),
                freed,
                problem,
            }
        })
        .collect()
}

/// The folder at this path, if it's still a build folder of a project in one of the folders
/// looked in, and nothing running is using it.
fn checked_folder(path: &str, roots: &[PathBuf], running: &[String]) -> Result<PathBuf, String> {
    let given = Path::new(path);
    let not_one = || format!("{path} isn't a build folder in the folders looked in");
    if !given.is_absolute() {
        return Err(not_one());
    }
    let metadata = std::fs::symlink_metadata(given).map_err(|_| format!("{path} isn't there"))?;
    if !metadata.is_dir() {
        return Err(not_one());
    }
    let exact = std::fs::canonicalize(given).map_err(|_| not_one())?;
    if !roots
        .iter()
        .any(|root| exact.starts_with(root) && exact != *root)
    {
        return Err(not_one());
    }
    let project = exact.parent().ok_or_else(not_one)?;
    let name = exact
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(not_one)?;
    let files: Vec<String> = std::fs::read_dir(project)
        .map_err(|_| not_one())?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.file_name().to_string_lossy().to_ascii_lowercase())
        .collect();
    let has = |wanted: &str| match wanted.strip_prefix('*') {
        Some(ending) => files.iter().any(|file| file.ends_with(ending)),
        None => files
            .iter()
            .any(|file| file == &wanted.to_ascii_lowercase()),
    };
    if FolderKind::of(&name, has).is_none() {
        return Err(not_one());
    }
    if in_use(project, running) {
        return Err(format!("{path} is in use by a program that's running"));
    }
    Ok(exact)
}

fn delete_folder(folder: &Path) -> (u64, Option<String>) {
    let before = size_of_folder(folder);
    let mut result = std::fs::remove_dir_all(folder);
    if result.is_err() {
        // Some installs leave read-only files, which Windows won't delete as they are.
        allow_writing(folder);
        result = std::fs::remove_dir_all(folder);
    }
    let left = if folder.exists() {
        size_of_folder(folder)
    } else {
        0
    };
    let freed = before.saturating_sub(left);
    match result {
        Ok(()) => (freed, None),
        Err(error) => (
            freed,
            Some(format!("some of it couldn't be deleted: {error}")),
        ),
    }
}

fn allow_writing(folder: &Path) {
    let mut waiting = vec![folder.to_path_buf()];
    while let Some(folder) = waiting.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && !kind.is_symlink() {
                waiting.push(entry.path());
            } else if let Ok(metadata) = entry.metadata() {
                let mut permissions = metadata.permissions();
                if permissions.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(false);
                    let _ = std::fs::set_permissions(entry.path(), permissions);
                }
            }
        }
    }
}

/// Each cache is cleaned the way its own tool does it where there's one, so that the tool
/// doesn't find half a cache.
fn clean_cache(kind: CacheKind) -> (u64, Option<String>) {
    let Some(path) = cache_path(kind) else {
        return (0, Some("can't find this user's folder".into()));
    };
    if !path.is_dir() {
        return (0, None);
    }
    let before = size_of_folder(&path);
    let result = match kind {
        CacheKind::Npm => tools::run("npm", &["cache", "clean", "--force"], tools::SLOW).map(drop),
        CacheKind::PnpmStore => tools::run("pnpm", &["store", "prune"], tools::SLOW).map(drop),
        CacheKind::NuGetPackages => tools::run(
            "dotnet",
            &["nuget", "locals", "global-packages", "--clear"],
            tools::SLOW,
        )
        .map(drop),
        CacheKind::NuGetHttp => tools::run(
            "dotnet",
            &["nuget", "locals", "http-cache", "--clear"],
            tools::SLOW,
        )
        .map(drop),
        // Cargo has no command for this. Its downloaded packages and their unpacked copies come
        // back by themselves, and the index of what's available is left alone.
        CacheKind::CargoRegistry => ["cache", "src"]
            .iter()
            .map(|part| path.join(part))
            .filter(|part| part.is_dir())
            .try_for_each(|part| std::fs::remove_dir_all(part).map_err(|error| error.to_string())),
    };
    let freed = before.saturating_sub(size_of_folder(&path));
    (freed, result.err())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A folder with a few projects, emptied first.
    fn projects(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join("hackerman-dev-tests").join(name);
        let _ = fs::remove_dir_all(&root);
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write("site/package.json", r#"{"name":"hackerman-site"}"#);
        write("site/node_modules/next/index.js", &"x".repeat(1000));
        write("site/.next/cache/a", &"y".repeat(500));
        write(
            "tool/Cargo.toml",
            "[package]\nname = \"hk-tool\"\nversion = \"0.1.0\"\n",
        );
        write("tool/target/debug/hk-tool.exe", &"z".repeat(2000));
        write("Api/Api.csproj", "<Project />");
        write("Api/bin/Debug/Api.dll", &"w".repeat(300));
        write("Api/obj/project.assets.json", "{}");
        // Names a build folder would have, but no project beside them.
        write("notes/target/plan.txt", "keep me");
        write("notes/bin/tool.exe", "keep me too");
        fs::canonicalize(root).unwrap()
    }

    #[test]
    fn build_folders_are_found_with_their_projects() {
        let root = projects("found");
        let report = report(std::slice::from_ref(&root));
        let found: Vec<(FolderKind, &str, u64)> = report
            .folders
            .iter()
            .map(|folder| (folder.kind, folder.project.as_str(), folder.size))
            .collect();
        assert_eq!(found.len(), 5, "{found:?}");
        assert!(found.contains(&(FolderKind::NodeModules, "site", 1000)));
        assert!(found.contains(&(FolderKind::NextBuild, "site", 500)));
        assert!(found.contains(&(FolderKind::RustTarget, "tool", 2000)));
        assert!(found.contains(&(FolderKind::DotNetBuild, "Api", 300)));
        assert_eq!(found[0].2, 2000, "largest first");
        assert!(report.folders.iter().all(|folder| !folder.in_use));
        assert!(!report.drives.is_empty());
    }

    #[test]
    fn a_build_folder_is_deleted_and_nothing_else() {
        let root = projects("deleted");
        let roots = vec![root.clone()];
        let target = root.join("tool").join("target");
        let results = clean(
            &[
                CleanItem::Folder {
                    path: plain(&target),
                },
                // Not a build folder: no Cargo.toml beside it.
                CleanItem::Folder {
                    path: plain(&root.join("notes").join("target")),
                },
                // Outside the folders looked in.
                CleanItem::Folder {
                    path: plain(&std::env::temp_dir()),
                },
                // A way out of the folders looked in.
                CleanItem::Folder {
                    path: plain(&root.join("tool").join("..").join("..")),
                },
            ],
            &roots,
        );
        assert_eq!(results[0].problem, None);
        assert_eq!(results[0].freed, 2000);
        assert!(!target.exists());
        assert!(root.join("tool/Cargo.toml").exists(), "the project stays");
        for refused in &results[1..] {
            assert!(refused.problem.is_some(), "{refused:?}");
            assert_eq!(refused.freed, 0);
        }
        assert!(root.join("notes/target/plan.txt").exists());
        assert!(std::env::temp_dir().exists());
    }

    #[test]
    fn read_only_files_are_deleted_too() {
        let root = projects("read-only");
        let file = root.join("site/node_modules/next/index.js");
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).unwrap();
        let results = clean(
            &[CleanItem::Folder {
                path: plain(&root.join("site").join("node_modules")),
            }],
            std::slice::from_ref(&root),
        );
        assert_eq!(results[0].problem, None);
        assert!(!root.join("site/node_modules").exists());
    }

    /// Links lead to files that are counted, and deleted, where they really are.
    #[test]
    fn links_are_neither_counted_nor_followed() {
        let root = projects("linked");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("big"), "q".repeat(5000)).unwrap();
        let link = root.join("site/node_modules/linked");
        // Junctions need no special rights to make, unlike symbolic links.
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        assert_eq!(size_of_folder(&root.join("site/node_modules")), 1000);
        clean(
            &[CleanItem::Folder {
                path: plain(&root.join("site").join("node_modules")),
            }],
            std::slice::from_ref(&root),
        );
        assert!(outside.join("big").exists(), "what the link led to stays");
    }

    #[test]
    fn the_usual_places_are_the_ones_that_are_there() {
        for root in roots(&[]) {
            assert!(root.is_dir(), "{}", root.display());
        }
        let root = projects("roots");
        let inner = root.join("site");
        assert_eq!(
            roots(&[root.clone(), inner]),
            [root],
            "one inside another counts once"
        );
    }
}
