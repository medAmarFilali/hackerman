# Changelog

Downloads are on the [releases page](https://github.com/medAmarFilali/hackerman/releases).
Versions before 0.10.0 weren't published.

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
