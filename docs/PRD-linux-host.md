# PRD — Linux (Hyprland) host → macOS and Linux clients

*Log in to a Linux machine from a Mac or another Linux machine, the way RDP does: a login screen, then a desktop session of your own.* Shared principles, technology and priority words: [PRD.md](PRD.md). The built direction: [PRD-mac-host.md](PRD-mac-host.md).

**State (November 2026):** proposed. No host is built. Spikes S1, S2, S5 and S6 are done ([#63](https://github.com/auroval/remote/issues/63)). The receipts below come from reading Hyprland's, greetd's and SDL's source or manuals at the versions we use, from Apple's SDK headers, and from short measurements on our own machines. Everything else is marked as a budget, an assumption or a spike.

**Changed in November 2026.** The October version borrowed the session at the desk: stream the logged-in Hyprland, unlock it through hyprlock, lock it when the client leaves. A session of its own was pushed to a later phase (L4) and the root-daemon design was listed as ruled out. The maintainer has now made the own session the goal. When the user is already logged in at the desk, the client takes that session over instead (R9), with the desk kept dark so nobody there can watch; the spikes done for the old plan (S1, S2) back that path.

## The decision this document is for

1. **Build it, and in what order.** The new spikes (S8–S12) say whether a session of its own works on Hyprland at all; each phase after that is useful on its own.
2. **The architecture:** a root daemon that shows a login screen to a paired client and starts a **separate, headless Hyprland session** for whoever logs in, instead of streaming the session at the desk.

## Summary

The Linux machine runs `auroval-daemon` as a systemd *system* service. A paired client connects and sees a **login screen** (a greeter), drawn in a small headless compositor that only the stream shows. The user picks an account and types the password; PAM checks it. The daemon then starts a **new Hyprland session** for that user, on a virtual output the size of the client window. It never appears on the physical monitors, and the keyboard and mouse at the desk don't reach it. When the client leaves, the session keeps running, disconnected; the next connection goes through the login screen again and lands back in it.

That's the RDP model. The Mac host can't do it (macOS has one GUI session per screen), so here the directions differ on purpose.

## Goals

1. **A login screen first.** Connecting never lands in a desktop directly: the client sees a greeter and the host's PAM decides.
2. **A session of its own.** The remote session is separate from the desk: the monitors stay as they are, and what happens at the desk doesn't reach it. When the user is already logged in at the desk, that session is taken over instead, and the desk goes dark.
3. **Clients on macOS and Linux.** The same client code on both; Cmd+Tab and `$mainMod` go to the host while grabbed.
4. **The same security as the Mac host:** only paired clients reach the login screen, the session needs the password every time a client connects, and no password is checked by our own code.
5. **Reuse:** the same protocol, pairing and discovery.

## Non-goals

- **Several users at once on one machine, as a terminal server.** One remote session per user is the goal. Two different users each with their own session may work out of the design, but isn't tested or promised.
- **Our own password checking** (password files, SSH keys, U2F in our code). PAM checks the password; we pass it through and act on PAM's answer.
- **Remote login over a physical seat or VT.** The remote session has no seat; it never takes over the monitors.
- **Testing and supporting compositors other than Hyprland, for now.** The remote session runs Hyprland. That's a scope limit, not a design choice (R16).
- **Internet access without a VPN, file transfer, several displays at once.** Same as the Mac host. Audio follows the Mac host's later work ([#82](https://github.com/auroval/remote/issues/82)).
- **Mac App Store distribution.** An event tap and Accessibility don't fit the sandbox.

## Constraints and findings, with receipts

### Spikes done ([#63](https://github.com/auroval/remote/issues/63))

| Spike | Result |
|---|---|
| S1 — capture the lock screen | Works (nested Hyprland 0.56.2, hyprlock 0.9.6): `ext-image-copy-capture` keeps delivering frames that show the lock screen. |
| S2 — type into hyprlock | Works: wrong password stays locked, right password unlocks; failures count toward `pam_faillock`. |
| S5 — encode latency | 1440p NVENC, capture → packet, median: 6.9 ms HEVC 4:2:0, 14.0 ms HEVC 4:4:4 (CPU conversion), 8.8 ms H.264. |
| S6 — the client on macOS | Builds and runs on macOS 27.2 with SDL3 and ffmpeg/VideoToolbox; all three codecs at a steady 30 fps from the fake host. |

S1 and S2 were for borrowing the desk's session; they now back taking it over (R9).

### greetd (0.10.3, installed on the Linux machine)

| Finding | Receipt |
|---|---|
| greetd always runs on a VT: `vt = num\|"next"\|"current"`, with no option for none. With `switch = false` it *"will wait for vt to become active, before doing anything including starting greeter"*. So greetd itself can't serve a greeter that only a remote client sees without taking a VT. | `man 5 greetd`, `[terminal]` section |
| Greeters (regreet, gtkgreet, tuigreet) talk to greetd over a small JSON IPC on the socket in `GREETD_SOCK`. A daemon that speaks that IPC could run an existing greeter unchanged. | **Not verified** against greetd's IPC spec; spike S10. |

### Hyprland (0.56.2 when measured; 0.57.1 now)

| Finding | Receipt |
|---|---|
| The capture and input protocols we'd use are compiled in: `ext_image_copy_capture_manager_v1` with `ext_output_image_capture_source_manager_v1`, `zwlr_screencopy_manager_v1`, `zwp_virtual_keyboard_manager_v1`, `zwlr_virtual_pointer_manager_v1`, plus `ext_session_lock_manager_v1` and `zwp_keyboard_shortcuts_inhibit_manager_v1`. | Interface names in `/usr/bin/Hyprland` (`strings`). Not yet checked against what the running compositor advertises (`wayland-info` isn't installed). |
| Hyprland has a permission system, **off by default**: `ecosystem:enforce_permissions`, default `false`. | `src/config/values/ConfigValues.cpp:620` at `v0.56.2` |
| When it's on, screen capture is checked **per client program** (`PERMISSION_TYPE_SCREENCOPY`), and virtual keyboards go through the same per-device keyboard check as real ones (`PERMISSION_TYPE_KEYBOARD`, by device name). Rules match a binary path, e.g. `hl.permission("/usr/(lib\|libexec\|lib64)/xdg-desktop-portal-hyprland", "screencopy", "allow")` in the shipped example. | `src/managers/screenshare/ScreenshareFrame.cpp:154`, `src/managers/input/InputManager.cpp:1127–1244` |
| Virtual outputs exist: `hyprctl output create headless [name]`. | `src/debug/HyprCtl.cpp:1777–1784` |
| Dark monitors stay dark on local input by default: `misc:key_press_enables_dpms` and `misc:mouse_move_enables_dpms` both default to `false` (an idle daemon such as hypridle can still wake them). Keyboards and pointers can each be switched off with the per-device `enabled` setting. | `src/config/values/ConfigValues.cpp:485–486`, `src/managers/input/InputManager.cpp:1233, 1358` |
| The xdg portal for Hyprland (`xdg-desktop-portal-hyprland` 1.4.1) offers `Screenshot`, `ScreenCast`, `GlobalShortcuts` and `InputCapture`, **not `RemoteDesktop`**. | `/usr/share/xdg-desktop-portal/portals/hyprland.portal`, `Interfaces=` line |
| Capture of a locked output shows the lock screen, and virtual-keyboard input reaches hyprlock (S1, S2 above). **Not verified:** capture while the physical outputs are off or disabled. | Spikes S1, S2; S7 for the rest |

### GPU encode on the host (RTX 4070 Ti, driver 615.78)

Throughput, not latency: 600 frames of 2560×1440 `testsrc2` through `ffmpeg -c:v <enc> -preset p1 -tune ull`, wall time including frame generation and upload on the CPU.

| Encoder | Wall time for 600 frames |
|---|---|
| `h264_nvenc` 4:2:0 | 1.31 s |
| `hevc_nvenc` 4:2:0 | 0.92 s |
| `hevc_nvenc` 4:4:4 (`rext`) | 1.57 s |
| `av1_nvenc` 4:2:0 | 0.95 s |
| `av1_nvenc` 4:4:4 | fails: *Error while opening encoder* |

All supported formats run several times faster than 60 fps. Per-frame encode latency, the number that matters, is spike S5.

### macOS client (MacBook Pro M3 Pro, macOS 27)

| Finding | Receipt |
|---|---|
| VideoToolbox decodes **HEVC 4:2:0, HEVC 4:4:4 (Rext) and AV1** in hardware: with `-hwaccel videotoolbox` all three land in `videotoolbox_vld` surfaces. | `ffmpeg` 9.0.2 on the Mac, 1080p samples encoded by NVENC above |
| Grabbing Cmd+Tab needs an **event tap at session level with Accessibility**. Apple: *"Taps may only be placed at `kCGHIDEventTap` by a process running as the root user."* Session-level taps only receive key events if the process has Accessibility. The old PRD's `kCGHIDEventTap` is therefore wrong for a normal client. | `CoreGraphics.framework/Headers/CGEvent.h`, comment on `CGEventTapCreate` |
| **SDL's keyboard grab doesn't help on macOS.** SDL 3.4.18's `Cocoa_SetWindowKeyboardGrab` calls the private `CGSSetGlobalHotKeyOperatingMode` only when SDL is built with `SDL_MAC_NO_SANDBOX`, which the build system doesn't set. Otherwise it does nothing. | `src/video/cocoa/SDL_cocoakeyboard.m:596` at `release-3.4.18` |
| **Not verified:** whether a session tap can swallow Cmd+Tab, Cmd+Space and Mission Control on macOS 27 before the system acts on them. | Spike S4 |

## Proposed shape

```
 client (macOS or Linux)                 Linux machine
 ┌────────────────────────────┐  QUIC   ┌────────────────────────────────────────────────────┐
 │ window, hardware decode    │◄───────►│ auroval-daemon (root, systemd system service)        │
 │ keyboard grab (event tap   │         │  • pairing, QUIC, mDNS, picks what to stream         │
 │   on macOS), host key      │         │  • PAM: checks the password, opens the session       │
 │ pairing, mDNS, clipboard   │         │  ├─ login screen: headless kiosk compositor + greeter│
 └────────────────────────────┘         │  │   as an unprivileged greeter user                 │
                                        │  └─ per user: headless Hyprland in its own logind    │
                                        │      session, plus auroval-agent inside it           │
                                        │      (capture → NVENC, virtual keyboard/pointer)     │
                                        └────────────────────────────────────────────────────┘
```

- **The daemon** is the only part that listens on the network, and the only root part. It holds the host identity and the paired clients, root-only, as on the Mac.
- **The login screen** runs in its own throwaway compositor as an unprivileged user. Its greeter talks to the daemon (greetd's IPC if S10 works, our own otherwise), never to PAM directly.
- **The user's session** is opened by the daemon through PAM (`pam_systemd` gives it a logind session, `XDG_RUNTIME_DIR` and the user's systemd). Hyprland runs in it with no physical outputs; the agent inside it captures and injects input and hands the stream to the daemon, as the Mac's agent does.

## Requirements

| # | Requirement | Prio | Fit criterion |
|---|---|---|---|
| R1 | Connecting shows a login screen, not a desktop. | Must | A paired client connects to a machine where nobody is logged in remotely: the client shows the greeter within 3 s. |
| R2 | The password is checked by PAM, with its limits. | Must | Right password → session; wrong password → stays at the greeter; after the `pam_faillock` limit the account is refused, as at the desk. No password is stored or logged by our code (checked by grepping the logs). |
| R3 | Logging in starts a separate Hyprland session for that user. | Must | After login the client shows a fresh Hyprland desktop; at the desk, the monitors and the local session (or greeter) are unchanged. `loginctl list-sessions` shows a new session without a seat. |
| R4 | The session survives the client leaving, and comes back. | Must | Open a terminal, quit the client, reconnect, log in again: the same terminal is there. |
| R5 | Only paired clients reach the login screen. | Must | Same tests as the Mac host (unpaired client refused, imposter host refused), against the Linux host. |
| R6 | Clients on macOS and Linux. | Must | Both clients log in and work 10 minutes; on the Mac, Cmd+Tab and Cmd+Space don't act on the Mac while grabbed, and the host key hands them back. |
| R7 | The session is the size of the client window. | Should | The headless output starts at the client window's size, and resizing the window resizes it within a second. |
| R8 | Latency close to the Mac host's. | Should | Capture to on-screen, median of 15 frames, LAN, HEVC 4:2:0 at 1440p: **budget ≤ 45 ms** (the Mac host measured ~41 ms). Not measured: nothing is built. |
| R9 | When the user is already logged in at the desk, logging in from the client takes that session over, and nobody at the desk can watch or type. | Must | Logged in at the desk, log in from the client: the client shows that same session (open windows and all); the desk's monitors go dark (disabled, not just DPMS) and the desk's keyboard and mouse don't reach the session or light the monitors. When the client leaves, the session is locked (hyprlock) before the monitors come back, and local input works again. |
| R10 | Ending the session from the client. | Should | Logging out of Hyprland in the remote session ends it; the client returns to the login screen. |
| R11 | Sharp text. | Should | HEVC 4:4:4 by default when both sides support it. |
| R12 | Pairing and discovery as on the Mac. | Should | `auroval pair` on either client finds the Linux host over mDNS; a code shown by the host pairs it. |
| R13 | Plain-text clipboard both ways. | Should | Multi-line text and emoji survive both directions. |
| R14 | Installable without building. | Should | One package for the host (Arch package or release tarball, [#37](https://github.com/auroval/remote/issues/37)); the Mac client as a cask, the Linux client as today's Flatpak. |
| R15 | A disconnected session ends after a set time. | Could | With a timeout set, a session without a client for longer than that is logged out. Off by default. |
| R16 | Hyprland-specific parts stay in one place. | Should | Hyprland-only calls (headless outputs, its config, hyprlock) appear in one module behind a compositor interface; another compositor means a new implementation, not changes across the host. |
| R17 | Works on AMD/Intel GPUs (VA-API), not just NVIDIA. | Could | The same fit as R8 on a VA-API machine. |

## Phasing

- **L0 — spikes.** Done: S1, S2, S5, S6. Still to do, in this order, because each one can sink the plan:
  - **S8** **Headless Hyprland in a seatless session.** Start Hyprland from a system service, as a user, with no seat or VT and only a headless output; does it render on the NVIDIA GPU, can our capture read it, and does virtual input reach it?
  - **S9** **A session from PAM.** Open a session through PAM with `pam_systemd` (class user, type wayland, no seat) and start S8's Hyprland in it: does `loginctl` show it, do `XDG_RUNTIME_DIR`, D-Bus, portals and app launching work?
  - **S10** **The login screen.** A greeter (regreet or gtkgreet) in a kiosk compositor as a greeter user, talking to a socket our daemon serves in greetd's IPC; the daemon checks the password with PAM. Does an unmodified greeter work, and does `pam_faillock` count failures?
  - **S11** **Same user, two sessions.** R9 avoids this when the desk session comes first, but not the other way round: a disconnected remote session is running and the user logs in at the desk. What breaks? PipeWire, portals, the keyring and `systemd --user` are per user, not per session; Hyprland's instance signature, `hyprctl` and autostart may clash. The answer decides whether both may run, or the desk login has to wait or end the remote one.
  - **S12** **Disconnected and back.** The session runs without a client (capture stopped) for an hour, then a new connection and login land in it with everything still open.
  - **S7** (from the October plan, now for R9) **Dark desk.** In a logged-in desk session: add a headless output, move all workspaces to it, disable the physical outputs and the local keyboard and mouse. Does the session keep rendering and taking our input? Does anything at the desk (keys, mouse, hypridle, a cable plugged in) light the monitors or reach the session? On leaving: hyprlock first, then the monitors back, and nothing of the session visible in between?
  - **S4** (unchanged) **Cmd+Tab on the Mac**: a session event tap with Accessibility swallows Cmd+Tab, Cmd+Space, Ctrl+Up and Cmd+Q, and releases them on the host key.
- **L1 — log in from the Linux client.** R1, R2, R3, R5, R8. The Linux client already exists, so this is the cheapest way to prove the whole path.
- **L2 — the Mac client, and coming back.** R6 (with S4), R4, R7, R12.
- **L3 — taking over the desk's session.** R9, after S7.
- **L4 — fit and finish.** R10, R11, R13, R14, then the Coulds.

S3 (waking a dark output) from the October plan is dropped: neither path needs the desk's outputs on. S7 stays, now for R9.

## Success metrics

- After L2: the Linux machine is used from the Mac at least weekly for real work, through the login screen, without walking to it.
- Nobody at the desk can see or type into a remote session, in any test so far.

## Ruled out, and why

| Option | Why not |
|---|---|
| **Borrowing the desk's session as the main model** (this PRD's October 2026 plan) | It needs someone logged in at the desk. Kept for that case only (R9). |
| **A curtain over the desk's monitors** (a full-screen layer on top, as macOS Remote Management and Chrome Remote Desktop do) | On Hyprland a curtain would be in the captured picture too, unless we capture a different output. Moving the workspaces to a headless output and disabling the physical ones gives the same privacy with nothing on top to get wrong; it's what Windows does in effect when RDP takes over the console (the desk shows the lock screen). |
| **greetd itself as the login service** | Always runs on a VT and waits for it to become active (`man 5 greetd`), so it can't serve a greeter only the client sees. Its greeters may still be usable through its IPC (S10). |
| **Portal capture and input** (`ScreenCast` + `RemoteDesktop`, libei) | The Hyprland portal has no `RemoteDesktop`, and `ScreenCast` asks through a picker, which doesn't suit an unattended host. Still the route for GNOME/KDE later. |
| **wayvnc or xrdp as the host** | A second protocol with its own authentication next to ours: no pairing, no codec negotiation, no shared client. |
| **`kCGHIDEventTap` on the Mac** | Root only (`CGEvent.h`). |
| **SDL's keyboard grab on the Mac** | A no-op unless SDL is built with `SDL_MAC_NO_SANDBOX`, and then it relies on a private API. Fallback if S4 fails, not the plan. |
| **AV1 as the default** | NVENC can't do AV1 4:4:4; HEVC 4:4:4 works on both sides. |

## Prior art

GNOME offers this since GNOME 46: "Remote Login" lets an RDP client reach GDM's login screen and get a headless session of its own, through gnome-remote-desktop. **Not verified** in detail here (how it handles a user already logged in at the desk is the interesting part for S11); worth reading before S9–S11.

## Open questions

| Question | Owner | Settled by |
|---|---|---|
| ~~Where will the Mac be when this is used?~~ **Answered:** mostly over the VPN, LAN too. So the narrow-path handling (#58) matters and R7 is a Should. | Maintainer | Answered October 2026 |
| ~~When the user is logged in at the desk: take over, or a new session?~~ **Answered:** take over, with the desk dark (R9). Whether "a new session next to it" is ever offered depends on S11. | Maintainer | Answered November 2026 |
| A remote session is running and the user logs in at the desk: run both, or not? | Maintainer | S11 |
| Which greeter: an existing one through greetd's IPC, or our own? | Claude | S10 |
| Does the daemon start the session's Hyprland, or a user unit that the daemon only triggers? | Claude | S9 |
| One client crate for Linux and macOS, or a macOS client of its own? | Claude | S6 says one crate works; settle in L2. |
| Which permission prompts does the Mac client hit, and do they survive updates? | Claude | S4, then an update test |

## What would change this plan

- **S8 fails** (Hyprland won't run headless without a seat, or not on NVIDIA): try another headless compositor for the session, which R16 allows; if none works, fall back to the October plan (borrow the desk's session) and keep the dark-desk problem.
- **S9 fails** (no usable logind session without a seat): the remote session runs without logind's help, which loses lock handling and per-session cleanup. Needs a new design, not a patch.
- **S10 fails** (no existing greeter works through our socket): we write a small greeter of our own. More work, same shape.
- **S7 fails** (Hyprland stops rendering or taking input with the physical outputs disabled, or something at the desk lights them): R9 falls back to refusing the takeover ("log out at the desk first") rather than showing the session at the desk.
- **S11 shows a second session of the same user breaks things badly**: one session per user at a time; R9 then means taking over or refusing, not running both.
- **S5 numbers don't hold for a headless output**: zero-copy capture (DMA-BUF into NVENC) becomes a requirement.
