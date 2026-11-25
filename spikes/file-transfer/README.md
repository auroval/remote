# Spike: files on the Mac's pasteboard and drops at a point (#154)

Questions a–e of spike S in [docs/PRD-file-transfer.md](../../docs/PRD-file-transfer.md).
`spike.swift` is one small app, "Auroval File Spike", with a command per
question. It runs as an app (through `open`), so that macOS attributes
permission prompts to it rather than to the terminal.

```zsh
./build.zsh                     # build/Auroval File Spike.app, ad-hoc signed
./run.zsh a <file>              # file URL of a temp copy on the pasteboard
./run.zsh b                     # a file promise on the pasteboard
./run.zsh d                     # read what Finder copied
./run.zsh e <file>              # drop <file> where the mouse is, after 5 s
```

`e` needs the app under Privacy & Security → Accessibility. An ad-hoc
signed app loses that grant with every rebuild: remove it and add it again.

Question c is on Linux, with the installed Flatpak:

```nu
let key = (wl-paste -t application/vnd.portal.filetransfer)
flatpak run --command=gdbus nl.stormlantern.Auroval call --session --dest org.freedesktop.portal.Documents --object-path /org/freedesktop/portal/documents --method org.freedesktop.portal.FileTransfer.RetrieveFiles $key '{}'
```

## Results (MacBook Pro M3, macOS 27.2.1; CachyOS, Dolphin 26.08; 2026-11-25)

| | Question | Answer |
|---|---|---|
| a | Does Finder paste a file URL that points into the agent's temp directory? | **Yes.** Cmd-V in a Finder window copied the file there. |
| b | Does Finder accept a file promise from the general pasteboard? | **No.** Finder beeps on Cmd-V and never asks the promise for its file. |
| c | Does the Flatpak get paths from Dolphin through `RetrieveFiles`? | **Yes.** It got one path under `/run/user/<uid>/doc/…`, readable inside the sandbox, with no dialog. |
| d | Does reading a file Finder copied from a protected folder prompt? | **Yes.** The read (a file in Downloads) blocked until the user answered the folder-access prompt for the app, then succeeded. The copy gives no implicit access. Reading the pasteboard itself did not prompt (`accessBehavior: alwaysAllow`). |
| e | Can the agent drop files at a point, so the app under it gets an ordinary drop? | **Yes**, for a Finder folder and a Slack channel (both accepted with "copy"). The Desktop was not tried. |

Details:

- **e:** a mouse-down posted with `CGEventPost` never reached the
  spike's window, and no other app saw it either. A drag the app starts
  itself works: a borderless opaque 24-pt window at the point, a
  synthesized `NSEvent` mouse-down passed to `beginDraggingSession`, and
  the window then hidden so that the drop lands on what's underneath. The
  session ends within ~250 ms with the operation the target chose.
- **d:** besides the file URL, Finder offers the file's **name** as plain
  text (`public.utf8-plain-text`), not its path (open question 2).

What this means for the design:

- **Linux → Mac copy and paste is eager.** The Mac has no lazy paste
  (b), so the file must be on the Mac before Cmd-V. The client sends it
  when the user copies, and the agent puts a file URL to its temp copy on
  the pasteboard (a).
- **Mac → Linux can be lazy.** The client can offer the files and fetch
  them only on paste. The first file from Desktop, Documents or Downloads
  makes macOS ask once whether Auroval may read that folder (d). The user
  sees that prompt on the Mac's screen, in the client window.
- **F10 can be built** (e).
