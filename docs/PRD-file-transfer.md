# PRD — Files between the client and the Mac

*Copy a file in Dolphin, paste it in Finder, and the other way round, during a session.* Shared principles, technology and priority words: [PRD.md](PRD.md). The direction this applies to first: [PRD-mac-host.md](PRD-mac-host.md).

**State (November 2026):** proposed. Nothing is built. File transfer was a non-goal of the Mac host until now. The receipts below come from our own code, from one measurement on the Linux desk, and from SDL's, Flatpak's and Apple's documentation. Everything else is marked as a target, an assumption or a spike.

## The decision this document is for

1. **Drop the non-goal**, and build copy and paste of files during a session, in both directions.
2. **What ships first:** Linux → Mac by copy and paste (P1), then Mac → Linux (P2). Dropping files on the window comes after that (P3). Dragging out of the window is not planned.
3. **How the bytes travel:** on their own QUIC streams inside the existing session, never on the control stream and never through a second channel (ssh, SMB, a web server).

## Summary

Today the clipboard carries plain text only ([#23](https://github.com/auroval/remote/issues/23)). Copying a file in Dolphin and pasting it in Finder does nothing: Dolphin offers no text, so the client sends nothing (receipts below). This adds files. When the user copies files on one side during a session, the other side's clipboard offers them. Pasting there gives real copies of the files. The bytes go over the session's QUIC connection, behind video and input.

## Goals

1. Move a file between the two machines without leaving the session: no AirDrop, no `scp`, no USB stick.
2. It works the way each desktop already works: copy in the file manager, paste in the other file manager.
3. A transfer never makes the session itself worse. Video and input stay as they are while files move.
4. Nothing leaves a machine unless the user copied it there during this session.

## Non-goals

- **Dragging out of the client window** (Mac → Linux by drag). SDL3 has no API to start a drag (receipts). Copy and paste covers the same need.
- **A file browser** of the other machine, in the style of RustDesk's file manager. The user picks files in their own file manager.
- **Sync, or keeping files up to date.** A paste is a one-time copy.
- **Cut and move.** Cutting in Dolphin or Finder is treated as copying. The original stays where it was.
- **Resuming a transfer** after a reconnect. A transfer cut off by a lost connection is cancelled, and the user copies again.
- **Images and rich text as clipboard data** (a screenshot pasted straight into a document). That's its own feature, with its own formats. This document is about files.
- **The login window and the lock screen.** There is no user session to put files in or take files from. Same as the text clipboard ([pasteboard.rs](../crates/mac-host/src/pasteboard.rs) runs "never at the login window").
- **The Linux host** ([PRD-linux-host.md](PRD-linux-host.md)). The protocol part is shared (principle 6), so that host gets it later with its own clipboard code. It is not planned here.

## Constraints, with receipts

| Constraint | Source |
|---|---|
| The client sends only text today. For files it sends nothing: `if !clipboard.has_clipboard_text() { return; // an image or files: text only for now }`. | [window.rs](../crates/linux-client/src/window.rs), `copied_here` |
| Text goes on the control stream, at most `MAX_CLIPBOARD` = 1 MiB a copy. That stream also carries keyframe requests and pings, so files can't go there. | [proto/src/lib.rs](../crates/proto/src/lib.rs) |
| **Dolphin offers no text for copied files.** After copying a file in Dolphin 26.08.3 (Qt 6.12), `wl-paste --list-types` showed `text/uri-list`, `application/x-kde4-urilist`, `application/vnd.portal.filetransfer` and `application/x-kde-source-id`. There was no `text/plain`. | Measured on the Linux desk, November 2026 |
| SDL3 can read the clipboard in any MIME type, and can offer data lazily for MIME types it lists: "Once another application requests the data the callback function will be called". Available since SDL 3.2.0. | [SDL_GetClipboardData](https://wiki.libsdl.org/SDL3/SDL_GetClipboardData), [SDL_SetClipboardData](https://wiki.libsdl.org/SDL3/SDL_SetClipboardData) |
| SDL3 receives drops (`SDL_EVENT_DROP_FILE`, with "the file name for SDL_EVENT_DROP_FILE"), but has no function to start a drag. | [SDL_DropEvent](https://wiki.libsdl.org/SDL3/SDL_DropEvent); none found in [CategoryClipboard](https://wiki.libsdl.org/SDL3/CategoryClipboard) or [CategoryEvents](https://wiki.libsdl.org/SDL3/CategoryEvents) |
| **The Flatpak can't open paths it gets as text.** The sandbox has "No access to any host files except the runtime", the app and its own data directories. Our manifest grants only `xdg-config/auroval`. | [Flatpak sandbox permissions](https://docs.flatpak.org/en/latest/sandbox-permissions.html); [nl.stormlantern.Auroval.yml](../packaging/flatpak/nl.stormlantern.Auroval.yml) |
| The way in for the Flatpak is the FileTransfer portal, which "operates as a middle-man between apps when transferring files via drag-and-drop or copy-paste". The source puts a key under `application/vnd.portal.filetransfer`. The receiver calls `RetrieveFiles` and gets paths under `/run/user/$UID/doc/`. Dolphin offers that MIME type (row above). | [FileTransfer portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.FileTransfer.html), [Documents portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Documents.html) |
| **macOS asks before apps read Desktop, Documents, Downloads, iCloud Drive, and network or removable volumes**: "all apps have obtained user consent before accessing files". The documentation names no exception for a file the user copied in Finder. Not verified for the agent. | [Apple Platform Security](https://support.apple.com/guide/security/controlling-app-access-to-files-secddd1d86a6/web) |
| Since macOS 15.4, "The default behavior for the General pasteboard is to ask upon programmatic access". Access that is "both user originated and paste related" is always allowed. The agent polls the pasteboard for text today, and no prompt has been reported in testing (#23, #119). Whether reading file URLs changes that is not verified. | [NSPasteboard.AccessBehavior](https://developer.apple.com/documentation/appkit/nspasteboard/accessbehavior-swift.enum) |
| `NSFilePromiseProvider` is for files that don't exist yet ("a possible future file of a specified type"). Apple describes it with drag and drop, and doesn't say whether Finder accepts a promise from the general (copy and paste) pasteboard. Not verified. | [NSFilePromiseProvider](https://developer.apple.com/documentation/appkit/nsfilepromiseprovider) |
| Pasteboard work runs in the agent, in the user's GUI session. The root daemon has none. | [PRD-mac-host.md](PRD-mac-host.md#constraints-with-receipts), [pasteboard.rs](../crates/mac-host/src/pasteboard.rs) |

## Ruled out

| Option | Why not |
|---|---|
| Files as clipboard text on the control stream | 1 MiB limit, and a large file would hold up keyframe requests and pings, so the picture would stall. |
| A second channel (`scp` or SMB to the Mac, a web server on the client) | It needs its own authentication and open ports, next to the paired QUIC connection. That breaks principles 2 and 6 ([PRD.md](PRD.md#2-principles-both-directions)). |
| A `--filesystem=home` permission for the Flatpak, instead of the portal | Every client install could then read the whole home directory. The portal gives access only to the files the user copied. Keep it as a fallback (see *What would change this plan*), not as the plan. |
| A file-manager panel in the client | A second UI to build and keep up, when both desktops already have file managers (non-goals). |

## Requirements

Priority words as in [PRD.md](PRD.md#4-priority-words).

| # | Requirement | Prio | Fit criterion |
|---|---|---|---|
| F1 | Files copied on Linux can be pasted on the Mac. | Must | Copy a 10 MB file and a folder of 50 small files in Dolphin, then paste in Finder: both arrive with the same names, the same bytes (checksum), and a mod time no later than the copy. |
| F2 | Files copied on the Mac can be pasted on Linux. | Must | Same test, in Finder → Dolphin. Also pasting into a Nautilus window, so it's not Dolphin-only. |
| F3 | It works from the Flatpak, without extra permissions. | Must | F1 and F2 pass with the Flatpak client, whose manifest has no new `--filesystem` entry. |
| F4 | Video and input stay as they are during a transfer. | Must | During a 1 GB transfer on the LAN, the median capture-to-screen latency (the client's per-stage split) rises by at most 10 ms, and no keystroke is lost. *Target, not measured.* |
| F5 | Only what the user copied during this session, and only while the Mac is unlocked. | Must | Files copied before connecting are not offered. A paste while the Mac is locked or at the login window offers no files. A session started without the clipboard (`Intent::Connect { clipboard: false }`) moves no files. |
| F6 | Progress shows, and a transfer can be cancelled. | Should | The overlay shows the name, bytes done and total while a transfer runs longer than a second. A cancel key stops it, and the paste then ends with an error instead of a half-written file. |
| F7 | A big copy doesn't start by surprise. | Should | Above a limit (open question 2), the side that would send asks first: on the client in the overlay, on the Mac in the menu-bar item's notification. Below the limit there's no question. |
| F8 | Nothing is left behind. | Must | After the session ends, no temporary copies remain on either machine (in the agent's temp directory, or the client's runtime or cache directory). Pasted files are real copies and stay. |
| F9 | A clear message when it can't work. | Should | A file macOS doesn't let the agent read (privacy), or a path the Flatpak can't open, gives a named error in the overlay or the host log. It never gives a silent nothing, as today. |
| F10 | Files dropped on the client window go to the Mac. | Could | Drop a file from Dolphin on the window: it arrives in a folder on the Mac (open question 3), and the menu-bar item says where. |

## Phasing

| Phase | What | Useful on its own because | Answers for the next |
|---|---|---|---|
| **S** (spike) | Four questions, each settled by a small test: (a) does Finder paste file URLs that point at files in the agent's temp directory; (b) does it accept a promise from the general pasteboard; (c) does the Flatpak get paths from Dolphin through `RetrieveFiles`; (d) does reading a file Finder copied from Documents prompt for the agent. | It decides the design; nothing ships. | Whether files are copied **eagerly** at copy time or **lazily** at paste time (open question 1). |
| **P1** | Linux → Mac: F1, F3 (that direction), F4, F5, F8, F9. | Covers the case that started this (Dolphin → Finder). | How fast it is in practice, and the limit for F7. |
| **P2** | Mac → Linux: F2, F3 (that direction), F6, F7. | Both directions. | Whether drops (F10) are still wanted. |
| **P3** | F10, drops on the window. | One gesture instead of two. | — |

## Success metrics

- For two weeks of daily use, the maintainer moves files between the machines with Auroval, without AirDrop or `scp`.
- No reported case of a session stalling or dropping during a transfer.
- No temporary copies found after sessions (F8 checked by hand once a week during those two weeks).

## Open questions

1. **Eager or lazy.** *Eager:* copying sends the files at once. Then the paste is instant, but every file copy during a session moves bytes, even when the user never pastes on the other side. *Lazy:* the paste fetches them. That's cheaper, but it needs Finder to wait on a promise (spike b), and it needs SDL's clipboard callback to block on the network while the other app waits. The current lean is lazy where the platform allows, eager with a limit where it doesn't. *Owner: the developer; settled by spike S.*
2. **The size limit for F7.** A proposal: ask first above 500 MB in total. *Owner: the maintainer.*
3. **Where dropped files go on the Mac (F10).** Downloads, the Desktop, or a folder of its own, `~/Downloads/Auroval`. *Owner: the maintainer; only needed for P3.*
4. **What Finder's copy offers as text today.** Copying a file in Finder may also put its name on the pasteboard as text, which the agent would then send to the client as text. Not checked. *Owner: the developer, during spike S.*

## What would change this plan

- **Finder doesn't paste file URLs from another process's temp directory** (spike a fails). Then the files have to go to a folder the user can see first, such as Downloads, and the paste becomes "a copy has arrived there". That's closer to F10 than to copy and paste.
- **Qt's clipboard doesn't hand files to a Flatpak through the portal** (spike c fails). Then F3 needs `--filesystem=home:ro` (ruled out above) or waits for a fix upstream. The native client would work in the meantime.
- **macOS prompts for every file the agent reads** (spike d). Then Mac → Linux needs the user to give the agent Full Disk Access once. That's a heavy request for a remote-desktop host, and would push P2 back.
- **QUIC stream priorities can't keep video smooth during a transfer** (F4 fails). Then transfers need a bandwidth cap of their own, below what the video leaves free.
