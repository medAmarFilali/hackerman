//! Files on the clipboard: listing what was copied here, supplying its content to other
//! devices, and reading the content of files that are on another device.
//!
//! Only the list of files travels when the cursor arrives. The content is read when
//! something is pasted, on a stream of its own for each file, so that there's no limit
//! on size and no temporary copy.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::Result;
use hk_core::clipboard::{ClipVersion, MAX_SHARED_FILES, SharedFile};
use hk_core::device::Fingerprint;
use hk_core::rules::NameClash;
use hk_net::{BlobChannel, DataReader, DataWriter, IncomingRequest};
use hk_platform::FileSource;
use hk_proto::{FileAnswer, FileRequest, PeerRequest};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;

use crate::engine::Event;

/// How much of a file to read and send at a time.
const PIECE: usize = 1024 * 1024;

/// How long a paste waits for a dropped connection to come back. Wi-Fi often drops out
/// for a moment, and that shouldn't ruin an hour's copying. The connection is replaced
/// within a few seconds, and the file carries on from where it was.
const RECONNECT_WAIT: Duration = Duration::from_secs(30);

const BETWEEN_ATTEMPTS: Duration = Duration::from_millis(100);

/// While nothing is arriving, how often to look at whether this is still wanted. Also
/// what ends a read if the program is stopping, since the network goes first then.
const CHECK_EVERY: Duration = Duration::from_secs(5);

/// How many sets of copied files stay available. Copying something else mustn't break a
/// paste that's still going on elsewhere, or that's waiting to resume.
const KEPT_OFFERS: usize = 8;

/// The connections to other devices, for reading files from them.
pub type Links = Arc<RwLock<HashMap<Fingerprint, BlobChannel>>>;

type Offer = (ClipVersion, Vec<PathBuf>);

#[derive(Default)]
struct Offers {
    /// From the clipboard, which come and go as other things are copied.
    clipboard: VecDeque<Offer>,
    /// Being sent to another device, until that's over.
    transfers: BTreeMap<ClipVersion, Vec<PathBuf>>,
}

/// What other devices may read: only files the user copied, and nothing else on this
/// computer, whatever they ask for.
#[derive(Clone, Default)]
pub struct OfferedFiles(Arc<Mutex<Offers>>);

impl OfferedFiles {
    /// `paths` matches the list given to other devices. Folders in it have no content.
    pub fn offer(&self, version: ClipVersion, paths: Vec<PathBuf>) {
        let mut offers = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        offers.clipboard.retain(|(offered, _)| *offered != version);
        offers.clipboard.push_back((version, paths));
        if offers.clipboard.len() > KEPT_OFFERS {
            offers.clipboard.pop_front();
        }
    }

    /// Like [`Self::offer`], for files being sent to another device. These stay until
    /// [`Self::withdraw`], however much else is copied meanwhile.
    pub fn offer_transfer(&self, version: ClipVersion, paths: Vec<PathBuf>) {
        let mut offers = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        offers.transfers.insert(version, paths);
    }

    /// Stops supplying these files, including if they're being read now.
    pub fn withdraw(&self, version: ClipVersion) {
        let mut offers = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        offers.transfers.remove(&version);
    }

    /// Stops supplying everything, including whatever is being read now.
    pub fn clear(&self) {
        let mut offers = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        offers.clipboard.clear();
        offers.transfers.clear();
    }

    fn path(&self, version: ClipVersion, index: usize) -> Option<PathBuf> {
        let offers = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let paths = offers.transfers.get(&version).or_else(|| {
            let found = offers
                .clipboard
                .iter()
                .find(|(offered, _)| *offered == version);
            found.map(|(_, paths)| paths)
        })?;
        paths.get(index).cloned()
    }

    fn is_offered(&self, version: ClipVersion) -> bool {
        let offers = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        offers.transfers.contains_key(&version)
            || offers
                .clipboard
                .iter()
                .any(|(offered, _)| *offered == version)
    }
}

/// Everything in `roots`, and what's in the folders among them, along with where each
/// one is on this computer. Folders come before what's inside them.
pub fn list(roots: &[PathBuf]) -> (Vec<SharedFile>, Vec<PathBuf>) {
    let mut files = Vec::new();
    let mut paths = Vec::new();
    for root in roots {
        let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
            tracing::warn!(path = %root.display(), "skipped something with no usable name");
            continue;
        };
        add(root, name, &mut files, &mut paths);
    }
    (files, paths)
}

fn add(path: &Path, relative: &str, files: &mut Vec<SharedFile>, paths: &mut Vec<PathBuf>) {
    if files.len() >= MAX_SHARED_FILES {
        tracing::warn!("too many files to share at once: the rest are left out");
        return;
    }
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skipped a file that can't be read");
            return;
        }
    };
    let file = SharedFile {
        path: relative.to_string(),
        size: if metadata.is_dir() { 0 } else { metadata.len() },
        is_dir: metadata.is_dir(),
        modified_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_millis() as u64),
    };
    if !file.has_safe_path() {
        tracing::warn!(path = %path.display(), "skipped a path too long to share");
        return;
    }
    files.push(file);
    paths.push(path.to_path_buf());

    // A link to a folder could lead back up to one of its own parents.
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|link| link.is_symlink());
    if !metadata.is_dir() || is_link {
        return;
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skipped a folder that can't be read");
            return;
        }
    };
    let mut children: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    children.sort();
    for child in children {
        match child.file_name().and_then(|name| name.to_str()) {
            Some(name) => add(&child, &format!("{relative}/{name}"), files, paths),
            None => {
                tracing::warn!(path = %child.display(), "skipped something with no usable name")
            }
        }
    }
}

/// Answers another device's requests for what's here, until the connection goes. File content
/// is answered from here, and anything needing a decision goes to the main loop.
pub async fn serve(
    peer: Fingerprint,
    blobs: BlobChannel,
    offered: OfferedFiles,
    events: UnboundedSender<Event>,
) -> Result<()> {
    loop {
        let request = blobs.next_request().await?;
        let offered = offered.clone();
        let events = events.clone();
        tokio::spawn(async move {
            if let Err(error) = route(peer, request, offered, events).await {
                // Usually only that the other side stopped reading.
                tracing::debug!(%error, "answering another device ended early");
            }
        });
    }
}

async fn route(
    peer: Fingerprint,
    incoming: IncomingRequest,
    offered: OfferedFiles,
    events: UnboundedSender<Event>,
) -> Result<()> {
    let (request, writer) = incoming.read().await?;
    match hk_proto::decode_peer_request(&request)? {
        PeerRequest::File(request) => answer(request, writer, offered).await,
        PeerRequest::Screen(request) => {
            let _ = events.send(Event::ScreenRequested {
                peer,
                request,
                writer,
            });
            Ok(())
        }
    }
}

async fn answer(request: FileRequest, mut writer: DataWriter, offered: OfferedFiles) -> Result<()> {
    let Some(path) = offered.path(request.version, request.index as usize) else {
        return refuse(writer, "that file is no longer being shared").await;
    };
    let mut file = match open_at(&path, request.offset).await {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "can't read a file being pasted");
            return refuse(writer, &format!("can't read {}: {error}", path.display())).await;
        }
    };
    tracing::info!(path = %path.display(), offset = request.offset, "sending a file");
    let answer = hk_proto::encode_file_answer(&FileAnswer::Content)?;
    writer.write_message(&answer).await?;

    let mut piece = vec![0; PIECE];
    loop {
        // Disabling sharing has to stop this too, not only new requests.
        if !offered.is_offered(request.version) {
            writer.abort();
            return Ok(());
        }
        match file.read(&mut piece).await {
            Ok(0) => {
                writer.finish()?;
                tracing::info!(path = %path.display(), "finished sending a file");
                return Ok(());
            }
            Ok(count) => writer.write(&piece[..count]).await?,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "reading a file failed partway");
                writer.abort();
                return Ok(());
            }
        }
    }
}

async fn open_at(path: &Path, offset: u64) -> io::Result<tokio::fs::File> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(io::SeekFrom::Start(offset)).await?;
    Ok(file)
}

async fn refuse(mut writer: DataWriter, reason: &str) -> Result<()> {
    let answer = hk_proto::encode_file_answer(&FileAnswer::Refused(reason.to_string()))?;
    writer.write_message(&answer).await?;
    writer.finish()?;
    Ok(())
}

/// Files that are on another device, for putting on this device's clipboard.
pub struct RemoteFiles {
    pub runtime: Handle,
    pub links: Links,
    pub accepts_input: Arc<AtomicBool>,
    /// What's saving these files, if something is, so reading stops when it does.
    pub control: Option<Arc<TransferControl>>,
    pub version: ClipVersion,
    pub sizes: Vec<u64>,
}

impl FileSource for RemoteFiles {
    fn open(&self, index: usize, offset: u64) -> Box<dyn Read + Send> {
        Box::new(RemoteFileReader {
            runtime: self.runtime.clone(),
            links: self.links.clone(),
            accepts_input: self.accepts_input.clone(),
            control: self.control.clone(),
            version: self.version,
            index: index as u32,
            size: self.sizes.get(index).copied().unwrap_or(0),
            position: offset,
            stream: None,
        })
    }
}

/// Reads one file from another device. Runs wherever Windows asks for the content, which
/// isn't an async task, so it waits for the network by blocking.
struct RemoteFileReader {
    runtime: Handle,
    links: Links,
    accepts_input: Arc<AtomicBool>,
    control: Option<Arc<TransferControl>>,
    version: ClipVersion,
    index: u32,
    /// The size the list gave. A device can't send more than it said, and fill the disk.
    size: u64,
    position: u64,
    stream: Option<DataReader>,
}

enum ReadFailure {
    /// It won't work however often it's tried.
    Refused(String),
    /// The connection may yet come back.
    Interrupted(String),
}

impl Read for RemoteFileReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let left = self.size.saturating_sub(self.position);
        let wanted = buffer
            .len()
            .min(usize::try_from(left).unwrap_or(usize::MAX));
        if wanted == 0 {
            return Ok(0);
        }
        let give_up_at = Instant::now() + RECONNECT_WAIT;
        loop {
            if !self.accepts_input.load(Ordering::Relaxed) {
                return Err(io::Error::other("Hackerman is disabled on this PC"));
            }
            if let Some(control) = &self.control
                && control.is_cancelled()
            {
                return Err(io::Error::other("cancelled"));
            }
            let runtime = self.runtime.clone();
            let target = &mut buffer[..wanted];
            // The timer has to be made inside here. Windows calls this on threads of its
            // own, which the async runtime knows nothing about until this point.
            let read = runtime.block_on(async {
                tokio::time::timeout(CHECK_EVERY, self.read_some(target)).await
            });
            let Ok(result) = read else {
                continue; // Nothing yet.
            };
            match result {
                Ok(count) => {
                    self.position += count as u64;
                    return Ok(count);
                }
                Err(ReadFailure::Refused(reason)) => return Err(io::Error::other(reason)),
                Err(ReadFailure::Interrupted(reason)) => {
                    self.stream = None;
                    if Instant::now() >= give_up_at {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, reason));
                    }
                    tracing::debug!(reason, "reading a file was interrupted: trying again");
                    std::thread::sleep(BETWEEN_ATTEMPTS);
                }
            }
        }
    }
}

impl RemoteFileReader {
    async fn read_some(&mut self, buffer: &mut [u8]) -> Result<usize, ReadFailure> {
        if self.stream.is_none() {
            self.stream = Some(self.request().await?);
        }
        let Some(stream) = self.stream.as_mut() else {
            return Ok(0);
        };
        stream
            .read(buffer)
            .await
            .map_err(|error| ReadFailure::Interrupted(error.to_string()))
    }

    async fn request(&self) -> Result<DataReader, ReadFailure> {
        let interrupted = |error: hk_net::Error| ReadFailure::Interrupted(error.to_string());
        let link = self
            .links
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&self.version.owner)
            .cloned()
            .ok_or_else(|| ReadFailure::Interrupted("the other PC isn't connected".into()))?;
        let request = FileRequest {
            version: self.version,
            index: self.index,
            offset: self.position,
        };
        let request = hk_proto::encode_file_request(&request)
            .map_err(|error| ReadFailure::Refused(error.to_string()))?;
        let mut stream = link.request(&request).await.map_err(interrupted)?;
        let answer = stream.read_message().await.map_err(interrupted)?;
        match hk_proto::decode_file_answer(&answer) {
            Ok(FileAnswer::Content) => Ok(stream),
            Ok(FileAnswer::Refused(reason)) => Err(ReadFailure::Refused(reason)),
            Err(error) => Err(ReadFailure::Refused(error.to_string())),
        }
    }
}

/// Files on this computer, for saving a copy of them in another folder here.
pub struct LocalFiles(pub Vec<PathBuf>);

impl FileSource for LocalFiles {
    fn open(&self, index: usize, offset: u64) -> Box<dyn Read + Send> {
        Box::new(LocalFileReader {
            path: self.0.get(index).cloned().unwrap_or_default(),
            offset,
            file: None,
        })
    }
}

struct LocalFileReader {
    path: PathBuf,
    offset: u64,
    file: Option<File>,
}

impl Read for LocalFileReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.file.is_none() {
            let mut file = File::open(&self.path)?;
            file.seek(SeekFrom::Start(self.offset))?;
            self.file = Some(file);
        }
        match &mut self.file {
            Some(file) => file.read(buffer),
            None => Ok(0),
        }
    }
}

/// What's shared between something saving files and whatever is watching it.
#[derive(Debug, Default)]
pub struct TransferControl {
    done: AtomicU64,
    cancelled: AtomicBool,
}

impl TransferControl {
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn add_done(&self, count: u64) {
        self.done.fetch_add(count, Ordering::Relaxed);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SaveError {
    Cancelled,
    Failed(String),
}

/// A short description of what's being sent, like "report.pdf and 2 more".
pub fn describe(names: &[&str]) -> String {
    match names {
        [] => "Nothing".to_string(),
        [only] => (*only).to_string(),
        [first, others @ ..] => format!("{first} and {} more", others.len()),
    }
}

/// The names of what's at the top of a list, without what's inside those folders.
pub fn top_level_names(files: &[SharedFile]) -> Vec<&str> {
    files
        .iter()
        .map(|file| file.path.as_str())
        .filter(|path| !path.contains('/'))
        .collect()
}

/// Saves all of `files` into `folder`, creating it if necessary. Blocks until it's done,
/// which can be a long time. An unfinished file has a temporary name, and goes away if
/// this fails or is cancelled, so anything with its real name is complete.
///
/// The paths in `files` must already have been checked with
/// [`SharedFile::has_safe_path`], if they came from another device.
pub fn save_all(
    files: &[SharedFile],
    source: &dyn FileSource,
    folder: &Path,
    clash: NameClash,
    control: &TransferControl,
) -> Result<(), SaveError> {
    let failed = |what: &Path, error: io::Error| {
        SaveError::Failed(format!("can't write to {}: {error}", what.display()))
    };
    std::fs::create_dir_all(folder).map_err(|error| failed(folder, error))?;
    let new_names = new_top_level_names(files, folder, clash);

    for (index, file) in files.iter().enumerate() {
        if control.is_cancelled() {
            return Err(SaveError::Cancelled);
        }
        let target = target_path(folder, &file.path, &new_names);
        if file.is_dir {
            std::fs::create_dir_all(&target).map_err(|error| failed(&target, error))?;
            continue;
        }
        if clash == NameClash::Skip && target.exists() {
            control.add_done(file.size);
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| failed(parent, error))?;
        }
        save_file(index, file, source, &target, control)?;
    }
    Ok(())
}

/// With keeping both, what's at the top of the list gets a new name if that name is
/// taken. Everything inside a folder then goes along with it, so that the copy of a
/// folder is a whole new folder rather than mixed in with the one already there.
fn new_top_level_names<'a>(
    files: &'a [SharedFile],
    folder: &Path,
    clash: NameClash,
) -> HashMap<&'a str, String> {
    let mut new_names = HashMap::new();
    if clash != NameClash::KeepBoth {
        return new_names;
    }
    let mut taken = HashSet::new();
    for name in top_level_names(files) {
        if folder.join(name).exists() || taken.contains(name) {
            let new_name = free_name(name, |candidate| {
                folder.join(candidate).exists() || taken.contains(candidate)
            });
            taken.insert(new_name.clone());
            new_names.insert(name, new_name);
        } else {
            taken.insert(name.to_string());
        }
    }
    new_names
}

/// The first of "report (1).pdf", "report (2).pdf" and so on that isn't taken.
fn free_name(name: &str, is_taken: impl Fn(&str) -> bool) -> String {
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    let extension = path.extension().and_then(|extension| extension.to_str());
    (1u64..)
        .map(|number| match extension {
            Some(extension) => format!("{stem} ({number}).{extension}"),
            None => format!("{stem} ({number})"),
        })
        .find(|candidate| !is_taken(candidate))
        .unwrap_or_else(|| name.to_string())
}

fn target_path(folder: &Path, path: &str, new_names: &HashMap<&str, String>) -> PathBuf {
    let mut target = folder.to_path_buf();
    for (depth, name) in path.split('/').enumerate() {
        match new_names.get(name).filter(|_| depth == 0) {
            Some(new_name) => target.push(new_name),
            None => target.push(name),
        }
    }
    target
}

fn save_file(
    index: usize,
    file: &SharedFile,
    source: &dyn FileSource,
    target: &Path,
    control: &TransferControl,
) -> Result<(), SaveError> {
    let mut name = target.as_os_str().to_owned();
    name.push(".hackerman-part");
    let temporary = PathBuf::from(name);

    let result = copy_to(index, file, source, &temporary, control).and_then(|()| {
        std::fs::rename(&temporary, target).map_err(|error| {
            SaveError::Failed(format!("can't write to {}: {error}", target.display()))
        })
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn copy_to(
    index: usize,
    file: &SharedFile,
    source: &dyn FileSource,
    temporary: &Path,
    control: &TransferControl,
) -> Result<(), SaveError> {
    let failed = |error: io::Error| {
        SaveError::Failed(format!("can't write to {}: {error}", temporary.display()))
    };
    let mut output = File::create(temporary).map_err(failed)?;
    let mut input = source.open(index, 0).take(file.size);
    let mut piece = vec![0; PIECE];
    let mut written = 0;
    loop {
        if control.is_cancelled() {
            return Err(SaveError::Cancelled);
        }
        let count = match input.read(&mut piece) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) if control.is_cancelled() => return Err(SaveError::Cancelled),
            Err(error) => return Err(SaveError::Failed(format!("{}: {error}", file.path))),
        };
        output.write_all(&piece[..count]).map_err(failed)?;
        written += count as u64;
        control.add_done(count as u64);
    }
    if written != file.size {
        return Err(SaveError::Failed(format!(
            "{} changed while it was being sent",
            file.path
        )));
    }
    if let Some(modified) = file.modified_ms {
        let modified = UNIX_EPOCH + Duration::from_millis(modified);
        // It's the same file either way, only with today's date on it.
        let _ = output.set_modified(modified);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use hk_net::{Accepted, Endpoint, Identity, TrustStore};
    use hk_proto::Message;

    use super::*;

    /// Whatever a run leaves behind goes when the next run gets to the same test.
    fn temp_folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join("hackerman-unit-files").join(name);
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn version(counter: u64) -> ClipVersion {
        ClipVersion {
            counter,
            owner: Fingerprint::from_bytes([1; 32]),
        }
    }

    #[test]
    fn folders_are_listed_with_everything_in_them_parents_first() {
        let root = temp_folder("nested");
        std::fs::create_dir_all(root.join("album/b")).unwrap();
        std::fs::write(root.join("album/b/deep.txt"), "12345").unwrap();
        std::fs::write(root.join("album/a.txt"), "1").unwrap();
        std::fs::write(root.join("single.txt"), "123").unwrap();

        let (files, paths) = list(&[root.join("album"), root.join("single.txt")]);
        let names: Vec<_> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            names,
            [
                "album",
                "album/a.txt",
                "album/b",
                "album/b/deep.txt",
                "single.txt"
            ]
        );
        assert!(files[0].is_dir && files[2].is_dir);
        assert_eq!(files[3].size, 5);
        assert!(files[3].modified_ms.is_some());
        assert_eq!(paths[3], root.join("album/b/deep.txt"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn what_cannot_be_read_is_left_out_of_the_list() {
        let root = temp_folder("missing");
        std::fs::write(root.join("here.txt"), "1").unwrap();
        let (files, paths) = list(&[root.join("gone.txt"), root.join("here.txt")]);
        assert_eq!(files.len(), 1);
        assert_eq!(paths, [root.join("here.txt")]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn only_what_was_copied_is_on_offer() {
        let offered = OfferedFiles::default();
        offered.offer(version(1), vec![PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(offered.path(version(1), 1), Some(PathBuf::from("b")));
        assert_eq!(offered.path(version(1), 2), None);
        assert_eq!(offered.path(version(2), 0), None);
    }

    #[test]
    fn copying_something_else_keeps_recent_offers_for_pastes_in_progress() {
        let offered = OfferedFiles::default();
        for counter in 1..=KEPT_OFFERS as u64 {
            offered.offer(version(counter), vec![PathBuf::from("file")]);
        }
        assert!(offered.is_offered(version(1)));
        offered.offer(version(100), vec![PathBuf::from("file")]);
        assert!(!offered.is_offered(version(1)), "not forever");
        assert!(offered.is_offered(version(2)));
    }

    #[test]
    fn files_being_sent_stay_on_offer_however_much_else_is_copied() {
        let offered = OfferedFiles::default();
        offered.offer_transfer(version(1), vec![PathBuf::from("sending")]);
        for counter in 100..120 {
            offered.offer(version(counter), vec![PathBuf::from("copied")]);
        }
        assert_eq!(offered.path(version(1), 0), Some(PathBuf::from("sending")));
        offered.withdraw(version(1));
        assert!(!offered.is_offered(version(1)));
    }

    /// Files held in memory, standing in for another device.
    struct Contents(Vec<&'static [u8]>);

    impl FileSource for Contents {
        fn open(&self, index: usize, _offset: u64) -> Box<dyn Read + Send> {
            Box::new(self.0[index])
        }
    }

    fn file(path: &str, content: &[u8]) -> SharedFile {
        SharedFile {
            path: path.into(),
            size: content.len() as u64,
            is_dir: false,
            modified_ms: None,
        }
    }

    fn dir(path: &str) -> SharedFile {
        SharedFile {
            path: path.into(),
            size: 0,
            is_dir: true,
            modified_ms: None,
        }
    }

    fn save(
        files: &[SharedFile],
        contents: Vec<&'static [u8]>,
        folder: &Path,
        clash: NameClash,
    ) -> Result<(), SaveError> {
        let control = TransferControl::default();
        save_all(files, &Contents(contents), folder, clash, &control)
    }

    fn read(path: PathBuf) -> String {
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    #[test]
    fn saving_creates_the_folder_and_everything_in_it_with_its_date() {
        let folder = temp_folder("save").join("not there yet");
        let mut dated = file("album/sous dossier/été.txt", b"deep");
        dated.modified_ms = Some(1_600_000_000_000);
        let files = [
            dir("album"),
            dir("album/sous dossier"),
            dated,
            file("seul.txt", b"alone"),
        ];
        let contents: Vec<&[u8]> = vec![b"", b"", b"deep", b"alone"];
        save(&files, contents, &folder, NameClash::KeepBoth).unwrap();

        let saved = folder.join("album/sous dossier/été.txt");
        assert_eq!(read(saved.clone()), "deep");
        assert_eq!(read(folder.join("seul.txt")), "alone");
        let modified = std::fs::metadata(saved).unwrap().modified().unwrap();
        let since = modified.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(since.as_millis(), 1_600_000_000_000);
    }

    #[test]
    fn keeping_both_gives_new_names_and_keeps_going_up() {
        let folder = temp_folder("keep-both");
        let files = [file("report.pdf", b"new"), file("notes", b"new")];
        for (round, name) in [(1, "report (1).pdf"), (2, "report (2).pdf")] {
            std::fs::write(folder.join("report.pdf"), "old").unwrap();
            std::fs::write(folder.join("notes"), "old").unwrap();
            save(&files, vec![b"new", b"new"], &folder, NameClash::KeepBoth).unwrap();
            assert_eq!(read(folder.join(name)), "new", "round {round}");
            assert_eq!(read(folder.join("report.pdf")), "old");
        }
        assert_eq!(read(folder.join("notes (2)")), "new");
    }

    #[test]
    fn a_folder_keeping_both_is_a_whole_new_folder() {
        let folder = temp_folder("keep-both-folder");
        std::fs::create_dir_all(folder.join("album")).unwrap();
        std::fs::write(folder.join("album/a.txt"), "old").unwrap();
        let files = [
            dir("album"),
            file("album/a.txt", b"new"),
            file("album/album", b"inner"),
        ];
        let contents: Vec<&[u8]> = vec![b"", b"new", b"inner"];
        save(&files, contents, &folder, NameClash::KeepBoth).unwrap();
        assert_eq!(read(folder.join("album/a.txt")), "old");
        assert_eq!(read(folder.join("album (1)/a.txt")), "new");
        assert_eq!(
            read(folder.join("album (1)/album")),
            "inner",
            "only the top is renamed"
        );
    }

    #[test]
    fn replacing_overwrites_and_mixes_into_a_folder() {
        let folder = temp_folder("replace");
        std::fs::create_dir_all(folder.join("album")).unwrap();
        std::fs::write(folder.join("album/a.txt"), "old").unwrap();
        std::fs::write(folder.join("album/other.txt"), "untouched").unwrap();
        let files = [dir("album"), file("album/a.txt", b"new")];
        save(&files, vec![b"", b"new"], &folder, NameClash::Replace).unwrap();
        assert_eq!(read(folder.join("album/a.txt")), "new");
        assert_eq!(read(folder.join("album/other.txt")), "untouched");
    }

    #[test]
    fn skipping_leaves_what_is_there_and_still_counts_as_progress() {
        let folder = temp_folder("skip");
        std::fs::write(folder.join("a.txt"), "old").unwrap();
        let files = [file("a.txt", b"new"), file("b.txt", b"new")];
        let control = TransferControl::default();
        let source = Contents(vec![b"new", b"new"]);
        save_all(&files, &source, &folder, NameClash::Skip, &control).unwrap();
        assert_eq!(read(folder.join("a.txt")), "old");
        assert_eq!(read(folder.join("b.txt")), "new");
        assert_eq!(control.done(), 6);
    }

    #[test]
    fn a_file_shorter_than_promised_fails_and_leaves_nothing_behind() {
        let folder = temp_folder("short");
        let mut promised = file("a.txt", b"12345");
        promised.size = 100;
        let result = save(&[promised], vec![b"12345"], &folder, NameClash::KeepBoth);
        assert!(matches!(result, Err(SaveError::Failed(_))), "{result:?}");
        let left: Vec<_> = std::fs::read_dir(&folder).unwrap().flatten().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_file_longer_than_promised_is_cut_off_at_what_was_promised() {
        let folder = temp_folder("long");
        let mut promised = file("a.txt", b"12345");
        promised.size = 3;
        save(&[promised], vec![b"12345"], &folder, NameClash::KeepBoth).unwrap();
        assert_eq!(read(folder.join("a.txt")), "123");
    }

    #[test]
    fn cancelling_leaves_nothing_unfinished_behind() {
        struct CancelsPartway(Arc<TransferControl>);
        impl FileSource for CancelsPartway {
            fn open(&self, _index: usize, _offset: u64) -> Box<dyn Read + Send> {
                struct Reader(Arc<TransferControl>);
                impl Read for Reader {
                    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                        self.0.cancel();
                        buffer[0] = 1;
                        Ok(1)
                    }
                }
                Box::new(Reader(self.0.clone()))
            }
        }
        let folder = temp_folder("cancel");
        let control = Arc::new(TransferControl::default());
        let source = CancelsPartway(control.clone());
        let mut big = file("big.bin", b"");
        big.size = 1000;
        let result = save_all(&[big], &source, &folder, NameClash::KeepBoth, &control);
        assert_eq!(result, Err(SaveError::Cancelled));
        let left: Vec<_> = std::fs::read_dir(&folder).unwrap().flatten().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_folder_that_cannot_be_made_is_reported() {
        let folder = temp_folder("blocked");
        std::fs::write(folder.join("in the way"), "a file").unwrap();
        let target = folder.join("in the way").join("sub");
        let result = save(
            &[file("a.txt", b"x")],
            vec![b"x"],
            &target,
            NameClash::KeepBoth,
        );
        assert!(matches!(result, Err(SaveError::Failed(_))), "{result:?}");
    }

    #[test]
    fn what_is_being_sent_has_a_short_description() {
        assert_eq!(describe(&["report.pdf"]), "report.pdf");
        assert_eq!(describe(&["a.txt", "b", "c"]), "a.txt and 2 more");
        let files = [dir("album"), file("album/a.txt", b""), file("b.txt", b"")];
        assert_eq!(top_level_names(&files), ["album", "b.txt"]);
    }

    #[test]
    fn clearing_withdraws_everything() {
        let offered = OfferedFiles::default();
        offered.offer(version(1), vec![PathBuf::from("a")]);
        offered.clear();
        assert_eq!(offered.path(version(1), 0), None);
    }

    /// Both ends of a working link. The first stays open for as long as it's kept.
    async fn linked() -> (BlobChannel, BlobChannel, impl Send) {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let loopback = "127.0.0.1:0".parse().unwrap();
        let server = Endpoint::bind(&a, loopback, TrustStore::new([b.fingerprint()])).unwrap();
        let client = Endpoint::bind(&b, loopback, TrustStore::new([a.fingerprint()])).unwrap();
        let address = server.local_addr().unwrap();
        let (accepted, dialed) = tokio::join!(
            async {
                let incoming = server.next_incoming().await.unwrap();
                match incoming.establish().await.unwrap() {
                    Accepted::Link(link) => link,
                    Accepted::Pairing(_) => panic!("expected a normal link"),
                }
            },
            async {
                let link = client.connect(address, a.fingerprint()).await.unwrap();
                let blobs = link.blobs();
                let (mut sender, receiver) = link.split();
                // The stream only appears to the other side once something is sent on it.
                sender.send(&Message::Enter).await.unwrap();
                (blobs, sender, receiver)
            },
        );
        let (client_blobs, sender, receiver) = dialed;
        let server_blobs = accepted.blobs();
        let keep_open = (accepted.split(), sender, receiver, server, client);
        (server_blobs, client_blobs, keep_open)
    }

    fn remote_file(links: Links, size: u64) -> Box<dyn Read + Send> {
        RemoteFiles {
            runtime: Handle::current(),
            links,
            accepts_input: Arc::new(AtomicBool::new(true)),
            control: None,
            version: version(1),
            sizes: vec![size],
        }
        .open(0, 0)
    }

    /// Pasting reads on threads that belong to Windows, not to the async runtime.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_works_on_a_thread_the_async_runtime_knows_nothing_about() {
        let (server, client, keep) = linked().await;
        let links = Links::default();
        links.write().unwrap().insert(version(1).owner, client);
        tokio::spawn(async move {
            let (_, mut writer) = server.next_request().await.unwrap().read().await.unwrap();
            let answer = hk_proto::encode_file_answer(&FileAnswer::Content).unwrap();
            writer.write_message(&answer).await.unwrap();
            writer.write(b"12345").await.unwrap();
            writer.finish().unwrap();
        });

        let mut reader = remote_file(links, 5);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let mut received = Vec::new();
            let result = reader.read_to_end(&mut received).map(|_| received);
            let _ = done_tx.send(result);
        });
        assert_eq!(done_rx.await.unwrap().unwrap(), b"12345");
        drop(keep);
    }

    /// Wi-Fi drops out for a moment, the connection goes, and a new one replaces it.
    /// The paste carries on from the byte it had reached.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_carries_on_where_it_was_after_the_connection_goes() {
        let content: Vec<u8> = (0..1_000_000u32).map(|at| (at % 251) as u8).collect();
        let (first_server, first_client, keep_first) = linked().await;
        let (second_server, second_client, keep_second) = linked().await;
        let links = Links::default();
        let owner = version(1).owner;
        let insert = |links: &Links, blobs: BlobChannel| {
            links.write().unwrap().insert(owner, blobs);
        };
        insert(&links, first_client);

        let sent = content.clone();
        let reconnect = links.clone();
        let server = tokio::spawn(async move {
            let content_answer = hk_proto::encode_file_answer(&FileAnswer::Content).unwrap();

            // The first connection gets partway, and then it's gone.
            let (request, mut writer) = first_server
                .next_request()
                .await
                .unwrap()
                .read()
                .await
                .unwrap();
            assert_eq!(hk_proto::decode_file_request(&request).unwrap().offset, 0);
            writer.write_message(&content_answer).await.unwrap();
            writer.write(&sent[..300_000]).await.unwrap();
            // Long enough for all of that to be read before the failure overtakes it.
            tokio::time::sleep(Duration::from_millis(300)).await;
            writer.abort();
            reconnect.write().unwrap().remove(&owner);

            // For a moment there's no connection at all.
            tokio::time::sleep(Duration::from_millis(400)).await;
            reconnect.write().unwrap().insert(owner, second_client);

            let (request, mut writer) = second_server
                .next_request()
                .await
                .unwrap()
                .read()
                .await
                .unwrap();
            let offset = hk_proto::decode_file_request(&request).unwrap().offset;
            assert_eq!(offset, 300_000, "it should pick up where it was");
            writer.write_message(&content_answer).await.unwrap();
            writer.write(&sent[offset as usize..]).await.unwrap();
            writer.finish().unwrap();
        });

        let mut reader = remote_file(links, content.len() as u64);
        let received = tokio::task::spawn_blocking(move || {
            let mut received = Vec::new();
            reader.read_to_end(&mut received).unwrap();
            received
        })
        .await
        .unwrap();
        server.await.unwrap();
        assert!(received == content, "the file changed on the way");
        drop((keep_first, keep_second));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_is_final_and_not_retried() {
        let (server, client, keep) = linked().await;
        let links = Links::default();
        links.write().unwrap().insert(version(1).owner, client);
        tokio::spawn(async move {
            let (_, writer) = server.next_request().await.unwrap().read().await.unwrap();
            refuse(writer, "no longer shared").await.unwrap();
        });

        let mut reader = remote_file(links, 10);
        let started = Instant::now();
        let error = tokio::task::spawn_blocking(move || {
            let mut buffer = [0; 10];
            reader.read(&mut buffer).unwrap_err()
        })
        .await
        .unwrap();
        assert!(error.to_string().contains("no longer shared"), "{error}");
        assert!(started.elapsed() < RECONNECT_WAIT / 2, "it kept trying");
        drop(keep);
    }

    #[test]
    fn asking_for_a_position_past_the_end_reads_nothing() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut reader = RemoteFiles {
            runtime: runtime.handle().clone(),
            links: Links::default(),
            accepts_input: Arc::new(AtomicBool::new(true)),
            control: None,
            version: version(1),
            sizes: vec![10],
        }
        .open(0, 10);
        assert_eq!(reader.read(&mut [0; 4]).unwrap(), 0);
    }
}
