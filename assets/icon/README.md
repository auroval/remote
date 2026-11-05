# AetherDesk icon

An aurora flowing through a screen, on a night sky. One script draws every
variant, so they can't drift apart:

```sh
python3 assets/icon/build.py   # needs rsvg-convert (librsvg)
```

Edit the geometry in `build.py`, run it, commit the outputs. Don't edit the
generated files by hand.

| File | What it is | Used by |
|---|---|---|
| `assets/icon/aetherdesk.svg` | Master, on Apple's grid: 1024 canvas, 824 px rounded-square body | README logo |
| `assets/icon/aetherdesk-small.svg` | Simplified (thicker lines, no thin ribbon) for 32 px and below | Source for the small PNGs |
| `assets/icon/wordmark-on-{light,dark}.svg` | "AetherDesk" in Inter SemiBold, as outlines (no font needed) | Docs, website, anywhere the name is shown as a logo |
| `assets/icon/social-preview.png` | 1280×640 GitHub social preview (`.svg` next to it is the source) | Repo **Settings → General → Social preview**, uploaded by hand |
| `packaging/linux/icons/nl.stormlantern.AetherDesk.svg` | Freedesktop scalable icon (body fills ~92% of the canvas) | Flatpak manifest, PKGBUILDs, release tarball, `INSTALL.md` |
| `packaging/linux/icons/nl.stormlantern.AetherDesk-{32,48,64,128,256}.png` | hicolor PNG sizes | Same |
| `packaging/macos/icon/AetherDesk.iconset/` | Every size `iconutil` expects, 16 to 512 @1x/@2x, macOS 11–15 template (with drop shadow) | Source for the `.icns` |
| `packaging/macos/icon/AetherDesk.icns` | The same sizes in one file | The `.app` bundle (#28) |
| `packaging/macos/icon/layers/{background,foreground}.svg` | Full-bleed layers for Icon Composer | macOS 26 `.icon` (#28) |
| `packaging/macos/icon/menubar/MenuBar{Idle,Connected}Template{.svg,.png,@2x.png}` | Menu-bar template icons, 18×18 pt: idle is an outline, connected is filled | The host's menu-bar item, once it has one |

## macOS

The host is a bare `aether-agent` binary today, so nothing uses the macOS
icon yet. When the host becomes an app bundle (#28):

- Copy `AetherDesk.icns` to `Contents/Resources/` and set
  `CFBundleIconFile` to `AetherDesk` in `Info.plist`. That covers macOS 14
  and 15, which the host supports.
- For macOS 26 (Tahoe), make an `AetherDesk.icon` in Icon Composer from the
  two layers (background, then foreground; Icon Composer applies the shape
  and the Liquid Glass look), add it to the bundle and set `CFBundleIconName`
  to `AetherDesk`. Without it, macOS 26 shows the `.icns`.
- On a Mac, `iconutil -c icns AetherDesk.iconset` rebuilds the same `.icns`
  as `build.py`.

### Menu bar

The menu-bar icons are template images: black on transparent, and macOS
tints them to match the menu bar (light, dark, highlighted). So the state
shows in the shape, not in colour: **idle** is an outlined screen,
**connected** a filled one with the aurora cut out.

- In an app bundle, put the PNGs (or the SVGs, in an asset catalog with
  *Render As: Template Image*) in `Contents/Resources/` and load them with
  `NSImage(named: "MenuBarIdleTemplate")`. The `Template` suffix makes
  NSImage treat them as templates.
- Loaded from a path instead, call `setTemplate(true)` on the image.
- Size the status item square (`NSStatusItem.squareLength`); the icons are
  drawn for an 18 pt square.
