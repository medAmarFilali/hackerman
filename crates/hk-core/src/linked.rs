//! Linked copies of handed-off files: the same file on two computers, kept the same by sending
//! only the pieces that changed (ADR 2).

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::clipboard::ClipVersion;
use crate::device::Fingerprint;

/// Pieces are cut where the content says, so unchanged bytes give the same pieces wherever they
/// have moved to. Never shorter than this except at the end, never longer than the most.
const SHORTEST_PIECE: usize = 256 * 1024;
const LONGEST_PIECE: usize = 4 * 1024 * 1024;
/// A cut comes where these bits of the rolling hash are clear: about every 1 MiB.
const CUT_BITS: u64 = ((1 << 20) - 1) << 44;
/// A file with more pieces than this, about 4 TB, isn't one to link.
pub const MOST_PIECES: usize = 1_000_000;

/// Numbers for the rolling hash, one for each byte, the same on every computer.
const GEAR: [u64; 256] = {
    let mut table = [0; 256];
    let mut seed: u64 = 0;
    let mut at = 0;
    while at < 256 {
        // SplitMix64.
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = seed;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        table[at] = mixed ^ (mixed >> 31);
        at += 1;
    }
    table
};

/// One version of a file: its size and what's in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileVersion {
    pub size: u64,
    pub hash: [u8; 32],
}

/// A piece of a file, named by what's in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Piece {
    pub hash: [u8; 32],
    pub len: u32,
}

/// A file here linked to a copy on another computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    /// The handing off that made it, the same on both computers.
    pub id: ClipVersion,
    pub peer: Fingerprint,
    /// The file here. It never comes from the other computer.
    pub path: PathBuf,
    /// The version both last had.
    pub shared: FileVersion,
}

/// Cuts what's read into pieces, and gives the version of the whole.
pub fn pieces_of(mut reader: impl Read) -> io::Result<(FileVersion, Vec<Piece>)> {
    let (mut whole, mut piece) = (Sha256::new(), Sha256::new());
    let mut pieces = Vec::new();
    let (mut size, mut length, mut rolling) = (0u64, 0usize, 0u64);
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let bytes = &buffer[..read];
        whole.update(bytes);
        size += read as u64;
        let mut start = 0;
        for (at, &byte) in bytes.iter().enumerate() {
            rolling = (rolling << 1).wrapping_add(GEAR[usize::from(byte)]);
            length += 1;
            if (length >= SHORTEST_PIECE && rolling & CUT_BITS == 0) || length == LONGEST_PIECE {
                piece.update(&bytes[start..=at]);
                pieces.push(Piece {
                    hash: piece.finalize_reset().into(),
                    len: length as u32,
                });
                (start, length, rolling) = (at + 1, 0, 0);
            }
        }
        piece.update(&bytes[start..]);
        if pieces.len() > MOST_PIECES {
            return Err(io::Error::other("it's too large to link"));
        }
    }
    if length > 0 {
        pieces.push(Piece {
            hash: piece.finalize().into(),
            len: length as u32,
        });
    }
    let version = FileVersion {
        size,
        hash: whole.finalize().into(),
    };
    Ok((version, pieces))
}

/// The parts of another version to fetch, as places in it and lengths: the pieces this
/// computer's copy doesn't have, with neighbours joined up.
pub fn missing(theirs: &[Piece], mine: &[Piece]) -> Vec<(u64, u64)> {
    let have: std::collections::HashSet<[u8; 32]> = mine.iter().map(|piece| piece.hash).collect();
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut at = 0u64;
    for piece in theirs {
        let len = u64::from(piece.len);
        if !have.contains(&piece.hash) {
            match ranges.last_mut() {
                Some((start, length)) if *start + *length == at => *length += len,
                _ => ranges.push((at, len)),
            }
        }
        at += len;
    }
    ranges
}

/// Where each of this computer's pieces is in its copy, to reuse them.
pub fn places(pieces: &[Piece]) -> HashMap<[u8; 32], (u64, u32)> {
    let mut at = 0u64;
    let mut places = HashMap::new();
    for piece in pieces {
        places.entry(piece.hash).or_insert((at, piece.len));
        at += u64::from(piece.len);
    }
    places
}

/// What to do when the other computer says its copy is now `theirs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incoming {
    /// This copy is that already.
    Same,
    /// Only the other one changed: take it.
    Take,
    /// Both changed, and theirs wins: keep this one beside it as a conflicted copy, then take it.
    TakeKeepingMine,
    /// Both changed, and this one wins: the other computer will take it.
    KeepMine,
}

/// `theirs_wins` settles both changing at once the same way on both computers.
pub fn incoming(
    shared: FileVersion,
    mine: FileVersion,
    theirs: FileVersion,
    theirs_wins: bool,
) -> Incoming {
    if mine == theirs {
        Incoming::Same
    } else if mine == shared {
        Incoming::Take
    } else if theirs_wins {
        Incoming::TakeKeepingMine
    } else {
        Incoming::KeepMine
    }
}

/// Whose version stays when both changed: the device whose fingerprint sorts first.
pub fn wins(device: Fingerprint, other: Fingerprint) -> bool {
    device.to_string() < other.to_string()
}

/// `report.docx` made on MAIN, kept as `report (conflicted copy from MAIN).docx`.
pub fn conflicted_copy(path: &Path, made_on: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    let made_on: String = made_on
        .chars()
        .filter(|c| {
            !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') && !c.is_control()
        })
        .collect();
    let name = match path.extension() {
        Some(extension) => format!(
            "{stem} (conflicted copy from {made_on}).{}",
            extension.to_string_lossy()
        ),
        None => format!("{stem} (conflicted copy from {made_on})"),
    };
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same every time, and different enough everywhere for cuts to fall by content.
    fn noise(length: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn cut(bytes: &[u8]) -> (FileVersion, Vec<Piece>) {
        pieces_of(bytes).unwrap()
    }

    #[test]
    fn pieces_add_up_to_the_whole_and_stay_in_bounds() {
        let bytes = noise(9 * 1024 * 1024 + 123, 1);
        let (version, pieces) = cut(&bytes);
        assert_eq!(version.size, bytes.len() as u64);
        assert_eq!(
            pieces.iter().map(|p| u64::from(p.len)).sum::<u64>(),
            version.size
        );
        assert!(pieces.len() >= 3, "{} pieces", pieces.len());
        let (last, rest) = pieces.split_last().unwrap();
        assert!(
            rest.iter()
                .all(|p| (SHORTEST_PIECE..=LONGEST_PIECE).contains(&(p.len as usize)))
        );
        assert!(last.len as usize <= LONGEST_PIECE);
        assert_eq!(cut(&bytes), (version, pieces), "the same on every computer");
        assert_eq!(cut(&[]).1, []);
    }

    /// The point of cutting by content: bytes put in near the start only change the pieces
    /// around them, not every piece after.
    #[test]
    fn inserting_bytes_only_changes_the_pieces_around_them() {
        let before = noise(20 * 1024 * 1024, 2);
        let mut after = before.clone();
        after.splice(1000..1000, *b"a few new bytes in the middle");
        let (_, old) = cut(&before);
        let (_, new) = cut(&after);
        let fetched: u64 = missing(&new, &old).iter().map(|(_, length)| length).sum();
        assert!(
            fetched <= LONGEST_PIECE as u64 * 2,
            "fetched {fetched} of {}",
            after.len()
        );
        assert_eq!(missing(&old, &old), []);
    }

    #[test]
    fn missing_pieces_next_to_each_other_are_fetched_together() {
        let piece = |byte: u8, len| Piece {
            hash: [byte; 32],
            len,
        };
        let theirs = [piece(1, 10), piece(2, 20), piece(3, 30), piece(4, 40)];
        let mine = [piece(1, 10), piece(4, 40)];
        assert_eq!(missing(&theirs, &mine), [(10, 50)]);
        assert_eq!(places(&mine)[&[4; 32]], (10, 40));
    }

    #[test]
    fn what_to_do_with_the_other_computers_version() {
        let version = |byte: u8| FileVersion {
            size: 1,
            hash: [byte; 32],
        };
        let (shared, mine, theirs) = (version(0), version(1), version(2));
        assert_eq!(incoming(shared, theirs, theirs, false), Incoming::Same);
        assert_eq!(incoming(shared, shared, theirs, false), Incoming::Take);
        assert_eq!(
            incoming(shared, mine, theirs, true),
            Incoming::TakeKeepingMine
        );
        assert_eq!(incoming(shared, mine, theirs, false), Incoming::KeepMine);
    }

    #[test]
    fn both_computers_agree_whose_version_stays() {
        let (a, b) = (
            Fingerprint::from_bytes([1; 32]),
            Fingerprint::from_bytes([2; 32]),
        );
        assert!(wins(a, b) != wins(b, a));
    }

    #[test]
    fn a_conflicted_copy_says_where_it_was_made() {
        assert_eq!(
            conflicted_copy(Path::new(r"C:\docs\report.docx"), "MAIN"),
            PathBuf::from(r"C:\docs\report (conflicted copy from MAIN).docx")
        );
        assert_eq!(
            conflicted_copy(Path::new(r"C:\docs\notes"), r#"a/b:c"#),
            PathBuf::from(r"C:\docs\notes (conflicted copy from abc)")
        );
    }
}
