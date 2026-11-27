//! Linked copies of handed-off files, on this computer's side: reading a version of one, fetching
//! the other computer's version, and answering its requests for this one's (ADR 2).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::time::Duration;

use anyhow::Result;
use hk_core::clipboard::ClipVersion;
use hk_core::device::Fingerprint;
use hk_core::linked::{self, FileVersion, MOST_PIECES, Piece};
use hk_net::{DataReader, DataWriter};
use hk_proto::{LinkAnswer, LinkRequest, LinkWant};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::runtime::Handle;

use crate::files::Links;

/// How long the other computer can go quiet in the middle of sending.
const QUIET_FOR_AT_MOST: Duration = Duration::from_secs(30);
const BUFFER: usize = 64 * 1024;

pub fn version_of(path: &Path) -> io::Result<(FileVersion, Vec<Piece>)> {
    linked::pieces_of(File::open(path)?)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Fetched {
    /// The file is now this version.
    Done(FileVersion),
    /// A program has the file open, so the new version waits beside it, to be put in place later.
    Waiting { new: PathBuf, version: FileVersion },
}

/// Where a new version waits until it can be put in place.
pub fn waiting_beside(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.hackerman"))
}

/// Fetches the other computer's version of a linked file, and puts it in place of `path`. With
/// `keep_mine`, this computer's version is copied there first. Blocks, waiting on the network
/// through `runtime`.
pub fn fetch(
    runtime: &Handle,
    links: &Links,
    peer: Fingerprint,
    link: ClipVersion,
    path: &Path,
    keep_mine: Option<&Path>,
) -> Result<Fetched, String> {
    let connection = links
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&peer)
        .cloned()
        .ok_or("the other PC isn't connected")?;
    let text = |error: &dyn std::fmt::Display| error.to_string();
    let ask = |want: LinkWant| -> Result<(FileVersion, DataReader), String> {
        runtime.block_on(async {
            let request =
                hk_proto::encode_link_request(&LinkRequest { link, want }).map_err(|e| text(&e))?;
            let mut stream = connection.request(&request).await.map_err(|e| text(&e))?;
            let answer = stream.read_message().await.map_err(|e| text(&e))?;
            match hk_proto::decode_link_answer(&answer).map_err(|e| text(&e))? {
                LinkAnswer::Pieces { version } => Ok((version, stream)),
                LinkAnswer::Bytes => Ok((
                    FileVersion {
                        size: 0,
                        hash: [0; 32],
                    },
                    stream,
                )),
                LinkAnswer::Refused(reason) => Err(reason),
            }
        })
    };

    let (version, mut stream) = ask(LinkWant::Pieces)?;
    let mut list = Vec::new();
    // Pieces are 32 bytes of hash and a length of at most 5.
    read_to_end(runtime, &mut stream, &mut list, MOST_PIECES * 37)?;
    let theirs = hk_proto::decode_pieces(&list).map_err(|e| text(&e))?;
    if theirs.iter().map(|piece| u64::from(piece.len)).sum::<u64>() != version.size {
        return Err("the other PC's list of pieces doesn't add up".into());
    }

    // With no copy here, every piece is fetched.
    let mine = version_of(path)
        .map(|(_, pieces)| pieces)
        .unwrap_or_default();
    let ranges = linked::missing(&theirs, &mine);
    let (_, mut stream) = ask(LinkWant::Bytes { version, ranges })?;
    let places = linked::places(&mine);
    let new = waiting_beside(path);
    let built = (|| -> Result<(), String> {
        let mut mine = File::open(path).ok();
        let mut out =
            File::create(&new).map_err(|e| format!("can't write {}: {e}", new.display()))?;
        let mut whole = Sha256::new();
        let mut buffer = vec![0; BUFFER];
        for piece in &theirs {
            let mut left = u64::from(piece.len);
            match places.get(&piece.hash).zip(mine.as_mut()) {
                Some((&(at, _), file)) => {
                    file.seek(SeekFrom::Start(at)).map_err(|e| text(&e))?;
                    while left > 0 {
                        let want = buffer.len().min(left as usize);
                        file.read_exact(&mut buffer[..want]).map_err(|e| text(&e))?;
                        whole.update(&buffer[..want]);
                        out.write_all(&buffer[..want]).map_err(|e| text(&e))?;
                        left -= want as u64;
                    }
                }
                None => {
                    while left > 0 {
                        let want = buffer.len().min(left as usize);
                        let read = read_some(runtime, &mut stream, &mut buffer[..want])?;
                        whole.update(&buffer[..read]);
                        out.write_all(&buffer[..read]).map_err(|e| text(&e))?;
                        left -= read as u64;
                    }
                }
            }
        }
        out.sync_all().map_err(|e| text(&e))?;
        let hash: [u8; 32] = whole.finalize().into();
        if hash != version.hash {
            return Err("what arrived didn't match the other PC's version".into());
        }
        Ok(())
    })();
    if let Err(reason) = built {
        let _ = std::fs::remove_file(&new);
        return Err(reason);
    }
    if let Some(keep) = keep_mine {
        std::fs::copy(path, keep).map_err(|e| format!("can't keep this PC's version: {e}"))?;
    }
    match std::fs::rename(&new, path) {
        Ok(()) => Ok(Fetched::Done(version)),
        // A program has it open.
        Err(_) if path.exists() => Ok(Fetched::Waiting { new, version }),
        Err(error) => {
            let _ = std::fs::remove_file(&new);
            Err(error.to_string())
        }
    }
}

fn read_some(
    runtime: &Handle,
    stream: &mut DataReader,
    buffer: &mut [u8],
) -> Result<usize, String> {
    // The timer has to be made inside, where the runtime is.
    let read = runtime
        .block_on(async { tokio::time::timeout(QUIET_FOR_AT_MOST, stream.read(buffer)).await });
    match read {
        Ok(Ok(0)) => Err("the other PC stopped sending".into()),
        Ok(Ok(count)) => Ok(count),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("the other PC went quiet".into()),
    }
}

fn read_to_end(
    runtime: &Handle,
    stream: &mut DataReader,
    into: &mut Vec<u8>,
    most: usize,
) -> Result<(), String> {
    let mut buffer = vec![0; BUFFER];
    loop {
        match read_some(runtime, stream, &mut buffer) {
            Ok(count) => into.extend_from_slice(&buffer[..count]),
            Err(reason) if reason == "the other PC stopped sending" => return Ok(()),
            Err(reason) => return Err(reason),
        }
        if into.len() > most {
            return Err("the other PC's list of pieces is too long".into());
        }
    }
}

/// Answers the other computer's request for this computer's copy of a linked file.
pub async fn answer(want: LinkWant, mut writer: DataWriter, path: PathBuf) -> Result<()> {
    let refuse = |reason: String| hk_proto::encode_link_answer(&LinkAnswer::Refused(reason));
    match want {
        LinkWant::Pieces => {
            let read = tokio::task::spawn_blocking(move || version_of(&path)).await?;
            let (version, pieces) = match read {
                Ok(read) => read,
                Err(error) => {
                    writer
                        .write_message(&refuse(format!("can't read it: {error}"))?)
                        .await?;
                    return Ok(writer.finish()?);
                }
            };
            let answer = hk_proto::encode_link_answer(&LinkAnswer::Pieces { version })?;
            writer.write_message(&answer).await?;
            writer.write(&hk_proto::encode_pieces(&pieces)?).await?;
        }
        LinkWant::Bytes { ranges, .. } => {
            let mut file = match tokio::fs::File::open(&path).await {
                Ok(file) => file,
                Err(error) => {
                    writer
                        .write_message(&refuse(format!("can't read it: {error}"))?)
                        .await?;
                    return Ok(writer.finish()?);
                }
            };
            writer
                .write_message(&hk_proto::encode_link_answer(&LinkAnswer::Bytes)?)
                .await?;
            let mut buffer = vec![0; BUFFER];
            for (start, length) in ranges {
                file.seek(SeekFrom::Start(start)).await?;
                let mut left = length;
                while left > 0 {
                    let want = buffer.len().min(usize::try_from(left).unwrap_or(BUFFER));
                    let read = file.read(&mut buffer[..want]).await?;
                    if read == 0 {
                        // It got shorter since: what arrives won't match, and is fetched again.
                        writer.abort();
                        return Ok(());
                    }
                    writer.write(&buffer[..read]).await?;
                    left -= read as u64;
                }
            }
        }
    }
    Ok(writer.finish()?)
}
