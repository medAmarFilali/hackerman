# 1. Hand off an app by reopening what it has open

- Status: accepted, 2026-11-26. Built for 0.13.0.
- Related: [2. Linked copies of handed-off files](0002-linked-copies-of-handed-off-files.md)

## Context

The user wants to move a window from one PC to another, the way a window moves between two
monitors of one PC. A window belongs to a program running on one PC, and Windows can't move a
running program to another machine. Three ways to get close were weighed:

1. **Make another PC's screen a monitor of this one.** It's a true window move, but needs a
   signed virtual display driver and video streaming, or Windows' own "Project to this PC",
   which needs Wi-Fi Direct and looks like a video call.
2. **Stream one window.** The program stays on the first PC, and the other shows a live video
   of it and sends input back, like RemoteApp. Clicks need the window in front on the first PC,
   which fights whoever is using it, and there's no sound.
3. **Reopen what the window has open on the other PC.** It isn't the same window, but it is the
   same page, folder or file.

The user chose the third, with files kept in sync between the two PCs.

## Decision

Dropping an app's icon from a screen of PC A onto a screen of PC B on the canvas hands it off.
A works out what the app's frontmost window on that screen has open, and B opens the same thing.

| In front on A | How A finds it | What B does |
|---|---|---|
| A Chromium browser: Edge, Chrome, Brave, Vivaldi, Opera | The address of the tab in front, read from the address bar with UI Automation | Opens it in the same browser if it's installed, or the default one |
| VS Code | The window's folder: the folder name in its title, matched against the open windows VS Code records in its `storage.json` | Opens the folder in VS Code |
| File Explorer | The window's folder, from the Shell's list of open windows | Opens the folder |
| Anything else | The file it has open: of the files named on its processes' command lines and in Recent items, the newest whose name is in the window's title | Gets a linked copy (ADR 2) and opens it with its default program |

- A folder is opened on B if it's there at the same path, or at the same place under the user's
  own folder, since user names differ between PCs. Otherwise B says it isn't there. Project
  folders aren't copied: git does that, and build folders can be huge.
- B chooses how to open things, never A. A file opens with B's default program for its type, and
  a file Windows would run as a program, like `.exe`, `.bat` or `.ps1`, is saved but not opened.
  Opening with the program A named would let A run a script with `powershell`. Only `http` and
  `https` addresses are opened, a browser is only started by name if it's one of the browsers
  above, and folders on other computers' shares (`\\server\...`) aren't opened.
- The original stays open on A, so nothing is lost if B can't open it.
- Only the tab in front goes. Every tab would need flipping through them visibly on A, or reading
  the browser's session file, whose format isn't published and changes.
- The PC whose canvas was used can be a third one. It sends `Message::HandOff` to A, and A sends
  `Message::Open`, or the file, to B. Each PC reports its own failures as a notification on its
  own screen, which is on the same desk.
- A only hands off if its setting that lets other computers bring up and move apps allows it, and
  B only opens what A sends if that setting on B allows it. Files also follow B's settings for
  receiving files, so B may ask first.
- The new messages need `FEATURE_HANDOFF`. A PC without it is said to need updating.

## Consequences

- It works for anything with an address, a folder or a file. What isn't saved doesn't go: text
  typed into a page, undo history, a document never saved.
- Finding the file is a guess that can fail, for example for a file an app opened from its own
  list of recent files without Windows hearing of it. A then says it couldn't tell which file.
- Address bars can hide `https://`. When an address has no scheme, `https://` is put back.
- File Explorer with tabs lists each tab for the same window. The first one is taken, which may
  not be the tab in front.
- Windows only, like the rest of Hackerman.
