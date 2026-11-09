# AetherDesk host for macOS (alpha)

The Mac side of AetherDesk: it streams this Mac's screen to the Linux client
and takes its keyboard and mouse. Apple Silicon, macOS 14 or later.

## Install

With Homebrew: `brew install --cask stormlantern-dev/tap/aetherdesk-host`.
From this tarball: put `aether-agent` somewhere on your `PATH`.

Then, as yourself (not with sudo; it asks for your password):

```sh
aether-agent install
```

This copies the binary to `/Library/Application Support/AetherDesk`, starts
a daemon and an agent per session, and adds an audio device, **AetherDesk**
(the Mac's sound pauses for a moment while it loads).

Once, grant that copy **Screen Recording** and **Accessibility** in System
Settings → Privacy & Security (+ button, then Cmd+Shift+G and paste
`/Library/Application Support/AetherDesk/aether-agent`). macOS only applies
them to a fresh start, so then:

```sh
aether-agent restart
```

`aether-agent status` shows whether it runs; a missing permission shows in
its log.

## Sound

During a session the Mac plays to the AetherDesk device, so its sound comes
out on Linux and the Mac stays quiet. Apps with their own speaker setting
(Teams, Zoom) can pick AetherDesk too. The Mac's volume keys do nothing
meanwhile; use the volume on Linux. AetherDesk is also the Mac's
microphone for the session: it hears the microphone on Linux (unless the
client runs with `--no-mic`). Afterwards the Mac's
own devices are back.

## Pair a client

```sh
sudo aether-agent pair
```

shows a 6-digit code for 2 minutes; type it into `aetherdesk pair` on Linux.
`sudo aether-agent clients` lists paired clients, `sudo aether-agent unpair NAME`
forgets one.

## Updates

After `brew upgrade`, run `aether-agent install` again: the installed copy is
what runs. Releases are signed with AetherDesk's Developer ID and notarized,
so the two permissions stay granted across updates. (Coming from an older,
ad hoc signed alpha, macOS asks for them once more.)

## Remove

```sh
aether-agent uninstall
```

removes the daemon, agents, audio device, logs and pairings. The entries in Privacy &
Security stay until you remove them there.
