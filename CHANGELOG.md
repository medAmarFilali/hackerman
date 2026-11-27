# Changelog

Downloads are on the [releases page](https://github.com/medAmarFilali/hackerman/releases).
Versions before 0.10.0 weren't published.

## 0.13.1

### Added

- **Ctrl+Shift+B keeps every computer active** while nobody uses it, so that programs like
  Teams don't show you as away. Each computer moves its cursor 10 pixels and back now and then.
  One whose keyboard is in use on another computer stays active without its cursor moving.
  Pressing it again, on any computer, stops it. It isn't in Settings, isn't remembered after a
  restart, and takes Ctrl+Shift+B from other programs, such as VS Code's Run Build Task. While
  it's on, computers don't lock themselves or go to sleep.

### Known limits

- It needs this version on every computer, and has only been tried between simulated computers
  so far.

## 0.13.0

### Added

- **Handing off to another computer.** Dragging an app's icon onto another computer's screen on
  the canvas opens what it has open there: the page in a browser's tab in front, a VS Code
  project, a File Explorer folder, or a copy of the file any other app has open. The original
  stays open. [ADR 1](docs/adr/0001-hand-off-apps-by-reopening.md) says why it works this way.
- **Files kept the same on two computers.** A handed-off file stays the same on both: a change
  on either goes to the other once it's saved, sending only the parts that changed. A file open
  in a program is updated when it's closed, and if both changed, both versions are kept.
  **Settings > Clipboard & files** lists them, with a button to stop.
  [ADR 2](docs/adr/0002-linked-copies-of-handed-off-files.md) has the details.

### Changed

- Updated two libraries the app's window uses, `url` and `cookie`, past published advisories
  (RUSTSEC-2026-0592 and RUSTSEC-2026-0594).

### Known limits

- Finding which file an app has open can miss, and Hackerman then says so.
- Handing off and keeping files the same have only been tried between simulated computers so
  far, and reading what's open only on one real computer.

## 0.12.0

### Added

- **Developer mode**, off until turned on in **Settings > Developer**. A **Developer** button
  shows, for this computer or another one that allows it:
  - the programs using the most memory, each with everything it started, such as a Next.js dev
    server with its project's name, with a button to stop it
  - build helpers left running after a build, to stop them all
  - the ports in use and what holds each one, to free a port
  - Docker's containers, to start, stop or restart them, and the size of Docker's disk file, with a
    way to shrink it
  - each drive's free space, and the build folders (`node_modules`, `target`, `bin` and `obj`,
    `.next`) and package caches taking space
  - the versions of Node.js, npm, pnpm, .NET, Rust, Docker and Git installed
- **Cleaning up** build folders, package caches and Docker's data, after a warning to read. What
  can lose data, like Docker volumes, is hidden until asked for and needs **DELETE** typed.
  Projects changed lately, and anything a running program is using, are left out.
- **Memory alerts**, off until turned on: a notification when one program uses more memory than
  the amount chosen.
- Each computer chooses which others can use developer mode on it, none at first, and is told
  afterwards what they did.

### Known limits

- Developer mode works on Windows only.
- Shrinking Docker's disk file, and cleaning up on another computer, have had very little use on
  real computers so far.

## 0.11.0

### Added

- **One monitor plugged into two computers.** It's one screen on the canvas instead of two, as
  the computer it's showing, and its corner says which. After switching the monitor with its own
  button, choose the computer it's showing there. Moving onto it goes to that computer, and the
  cursor never gets lost on the copy that isn't showing. It works for monitors that give a serial
  number, and needs this version on all the computers it's plugged into.

### Fixed

- **Programs running as administrator.** The cursor froze over a program running as
  administrator, and a computer's keyboard and mouse couldn't go to another one while such a
  program was in front. Both work once **Settings > General > Work at the lock screen and in
  admin programs** is on for that computer. If it was on already, use the **Update it** button
  there.

## 0.10.0

### Added

- **See what's on your screens.** Each screen on the canvas can show a live picture of what's on
  it, and open larger. Nothing leaves a computer until you allow it, one computer at a time, under
  **Settings > Screens**. A computer being looked at says so. Nothing can be seen of a locked
  computer, and pictures are only taken while the window of the computer looking is open.
- **Bring up apps from the canvas.** Clicking an app brings it to the front on its computer.
- **Move apps between screens.** Dragging an app to another screen of the same computer moves its
  windows there. **Settings > Apps & media** can stop other computers doing either of these.

### Fixed

- Long file names no longer stretch the side panel past the edge of the window.

### Known limits

- Bringing an app to the front, and moving windows between two real screens, have had very little
  use on real computers so far.
- Screens are captured in a simple way that suits occasional pictures. Full-screen games and some
  videos may not show properly, and films protected against copying usually show as black.

## 0.9.0

### Added

- **What's open.** Each screen on the canvas shows the icons of the apps open on it. Only names and
  icons are sent, never window titles.
- **Media buttons.** A screen with something playing has back, pause and forward buttons, for
  players that report to Windows, such as Spotify or a video in a browser, and for Stremio.
- **Media shortcuts.** Ctrl+Alt+Space, Page Up and Page Down control whatever is playing on any
  computer. They can be changed or turned off in **Settings > Shortcuts**.
- A warning when another program that shares a keyboard and mouse is running, such as Input
  Director, since the two can hold each other up.

### Changed

- Input is more robust when Windows is busy. The input hooks run at the highest priority and are
  put back if Windows removes them, input from other computers is replayed on a thread of its own,
  and the program isn't slowed down to save power.
- The parked cursor waits at the top left, away from where notifications come up.
- The log now says when input arrived late, when hooks were removed, and when Windows refused input.

### Known limits

- Stremio doesn't report to Windows, so its button can't show whether it's playing, and controlling
  it while it isn't the window in front may not work.

## 0.8.0

Everything up to this point:

- One canvas for every monitor of every computer, with free placement, DPI-correct crossing, and a
  saved layout for each set of screens
- Encrypted peer-to-peer connections, paired with a 6-digit code, and no server involved
- A shared clipboard, including files of any size
- Dropping files on a screen to save them on that computer, asking the first time a computer sends
  files, with a notification when they arrive
- Shortcuts for jumping between computers and keeping the cursor where it is, and rules that can
  hold the cursor back
- An optional Windows service that lets approved computers reach the lock and sign-in screens
