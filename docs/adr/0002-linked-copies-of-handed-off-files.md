# 2. Linked copies of handed-off files

- Status: accepted, 2026-11-26. Built for 0.13.0.
- Related: [1. Hand off an app by reopening what it has open](0001-hand-off-apps-by-reopening.md)

## Context

Handing off a document (ADR 1) copies it to the other PC. The user wants it kept in sync
afterwards, "like Dropbox used to do it", and big files are the point: sending a whole 4 GB video
again for every small change isn't acceptable. Three ways were offered:

1. **A linked copy**: the file stays where it is, the other PC gets a copy, and changes on either
   side go to the other.
2. **A synced folder, like Dropbox**: one folder kept the same on every PC. A handed-off file
   would be copied into it, so a file on the Desktop wouldn't sync back.
3. **Both.**

The user chose the linked copy.

## Decision

- **The copy.** B saves the file in `Documents\Hackerman`, with the usual rule for a name that's
  taken, using the transfer that already sends files. The original on A doesn't move. When it's
  saved, both PCs record a link: the other PC, their own path, and the version both last had.
- **Noticing changes.** Each PC checks its linked files' size and time of change every few
  seconds. A change that has stayed the same for 2 seconds, so the program has finished saving,
  is read. If it differs from the version both last had, it's offered to the other PC.
- **Only the changed parts.** Files are cut into pieces where their content says, about 1 MB on
  average, and each piece is named by its SHA-256 hash. The other PC cuts its own copy the same
  way, asks only for the pieces it doesn't have, writes the new version beside the file, checks
  the whole file's hash, and then puts it in place. Cutting by content, unlike Dropbox's fixed
  4 MB blocks, still finds the unchanged pieces after bytes are added in the middle.
- **A file that's open.** If the file can't be replaced because a program has it open, the new
  version waits beside it and is tried again until it can be. The user is told once.
- **Both changed.** The version from the PC whose fingerprint sorts first becomes the linked
  file on both. The other version is kept beside it as `name (conflicted copy from PC).ext` on
  the PC that made it. Nothing is overwritten without a copy being kept.
- **Ending a link.** A linked file that's gone at two checks in a row ends the link, and the
  other copy is left alone. Some programs save by replacing the file, so it's briefly gone, which
  is why one check isn't enough. Forgetting a PC ends its links. Settings lists linked files with
  a button to stop syncing each one.
- **Away and back.** Links are kept in `links.json` and survive restarts. A change made while the
  other PC is away is offered when it's back.
- **Who can send what.** Changes are only accepted for a file that's already linked, which the
  receiving PC agreed to when the copy arrived. A link is between two PCs. Handing the copy on to
  a third PC makes a link of its own.

## Consequences

- Changes go directly between the two PCs, encrypted, never through a server.
- No new dependencies: `sha2` is already in the build, and cutting by content is a few lines of
  rolling hash.
- A program that keeps its file locked, like Word, holds an update back until it closes the file.
- There's no history beyond conflicted copies.
- Most compressed formats, like video and `.docx`, change throughout when edited, so for them it
  saves less than for formats like `.psd`, `.vhdx` or uncompressed media. It still never sends
  more than the whole file.
- A file linked to a PC that never comes back stays linked until the link is stopped in Settings
  or the PC is forgotten.
