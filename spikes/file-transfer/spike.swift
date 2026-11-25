// Spike for docs/PRD-file-transfer.md, questions a, b, d and e (#154).
// Each command logs to /tmp/auroval-file-spike.log; run it through run.zsh so
// it runs as the app and macOS attributes permission prompts to the app.
//
//   a <file>  copy <file> into our temp directory, put its file URL on the
//             general pasteboard. Then paste in Finder: does the file arrive?
//   b         put a file promise on the general pasteboard and wait 120 s.
//             Then paste in Finder: is the promise asked for and kept?
//   d         read the file URLs on the general pasteboard (copy a file in
//             Finder from ~/Documents first) and try to read each file.
//             Logs every type and the text Finder offers, too.
//   e <file>  after 5 s, drop <file> (copied to temp) where the mouse is:
//             start a drag there with synthetic mouse events and let go.

import AppKit
import ApplicationServices
import UniformTypeIdentifiers

let logPath = "/tmp/auroval-file-spike.log"
let started = Date()

func log(_ s: String) {
    let line = String(format: "%7.3f ", Date().timeIntervalSince(started)) + s + "\n"
    FileHandle.standardError.write(line.data(using: .utf8)!)
    if let h = FileHandle(forWritingAtPath: logPath) {
        h.seekToEndOfFile()
        h.write(line.data(using: .utf8)!)
        try? h.close()
    } else {
        FileManager.default.createFile(atPath: logPath, contents: line.data(using: .utf8))
    }
}

func finish(_ code: Int32 = 0, after: TimeInterval = 0) {
    DispatchQueue.main.asyncAfter(deadline: .now() + after) {
        log("exit \(code)")
        exit(code)
    }
}

/// Copies `path` into our own temp directory, as the agent would after
/// receiving the file from the client.
func stage(_ path: String) -> URL? {
    let src = URL(fileURLWithPath: path)
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("auroval-spike", isDirectory: true)
    let dst = dir.appendingPathComponent(src.lastPathComponent)
    do {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try? FileManager.default.removeItem(at: dst)
        try FileManager.default.copyItem(at: src, to: dst)
        log("staged \(src.lastPathComponent) in our temp dir (\(dir.path.hasPrefix("/var/folders") ? "/var/folders/…" : dir.path))")
        return dst
    } catch {
        log("stage failed: \(error)")
        return nil
    }
}

func logPasteboardAccess() {
    if #available(macOS 15.4, *) {
        let b = NSPasteboard.general.accessBehavior
        let name: String
        switch b {
        case .default: name = "default"
        case .ask: name = "ask"
        case .alwaysAllow: name = "alwaysAllow"
        case .alwaysDeny: name = "alwaysDeny"
        @unknown default: name = "unknown(\(b.rawValue))"
        }
        log("pasteboard accessBehavior: \(name)")
    }
}

// MARK: a — file URL of a temp copy

func cmdA(_ path: String) {
    guard let url = stage(path) else { return finish(1) }
    let pb = NSPasteboard.general
    pb.clearContents()
    let ok = pb.writeObjects([url as NSURL])
    log("a: wrote file URL to pasteboard: \(ok); types: \(pb.types?.map(\.rawValue) ?? [])")
    log("a: now paste (cmd-V) in a Finder window; this process exits in 60 s")
    finish(0, after: 60)
}

// MARK: b — file promise

final class Promise: NSObject, NSFilePromiseProviderDelegate {
    func filePromiseProvider(_ p: NSFilePromiseProvider, fileNameForType fileType: String) -> String {
        log("b: asked for a file name (type \(fileType))")
        return "auroval-promise.txt"
    }

    func filePromiseProvider(_ p: NSFilePromiseProvider, writePromiseTo url: URL, completionHandler: @escaping (Error?) -> Void) {
        log("b: asked to write the promise to \(url.deletingLastPathComponent().lastPathComponent)/\(url.lastPathComponent)")
        do {
            try "written by the Auroval file spike at paste time\n".write(to: url, atomically: true, encoding: .utf8)
            log("b: written")
            completionHandler(nil)
        } catch {
            log("b: write failed: \(error)")
            completionHandler(error)
        }
        finish(0, after: 3)
    }
}

let promise = Promise()

func cmdB() {
    let provider = NSFilePromiseProvider(fileType: UTType.plainText.identifier, delegate: promise)
    let pb = NSPasteboard.general
    pb.clearContents()
    let ok = pb.writeObjects([provider])
    log("b: wrote file promise to pasteboard: \(ok); types: \(pb.types?.map(\.rawValue) ?? [])")
    log("b: now paste (cmd-V) in a Finder window; waiting up to 120 s")
    finish(2, after: 120)
}

// MARK: d — read what Finder copied

func cmdD() {
    logPasteboardAccess()
    let pb = NSPasteboard.general
    let t0 = Date()
    let types = pb.types?.map(\.rawValue) ?? []
    log("d: types (\(String(format: "%.2f", Date().timeIntervalSince(t0))) s): \(types)")
    if let text = pb.string(forType: .string) {
        let home = NSHomeDirectory()
        log("d: text offered: \(text.replacingOccurrences(of: home, with: "~").debugDescription)")
    } else {
        log("d: no text offered")
    }
    let urls = pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL] ?? []
    log("d: \(urls.count) file URL(s)")
    for u in urls {
        let shown = u.path.replacingOccurrences(of: NSHomeDirectory(), with: "~")
        let t = Date()
        do {
            let h = try FileHandle(forReadingFrom: u)
            let head = try h.read(upToCount: 16) ?? Data()
            try h.close()
            log("d: read \(shown): ok, \(head.count) bytes, took \(String(format: "%.2f", Date().timeIntervalSince(t))) s")
        } catch {
            log("d: read \(shown): FAILED after \(String(format: "%.2f", Date().timeIntervalSince(t))) s: \(error.localizedDescription)")
        }
    }
    finish(0)
}

// MARK: e — drop at the mouse

final class DragView: NSView, NSDraggingSource {
    var file: URL!
    var started = false

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseDown(with event: NSEvent) {
        if started { return }
        started = true
        log("e: mouseDown reached our window; starting drag")
        let item = NSDraggingItem(pasteboardWriter: file as NSURL)
        item.setDraggingFrame(bounds, contents: NSImage(systemSymbolName: "doc", accessibilityDescription: nil))
        let session = beginDraggingSession(with: [item], event: event, source: self)
        session.animatesToStartingPositionsOnCancelOrFail = false
        // Get out of the way, so the drop lands on whatever is underneath.
        window?.ignoresMouseEvents = true
        window?.alphaValue = 0
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { moveAndRelease() }
    }

    func draggingSession(_ s: NSDraggingSession, sourceOperationMaskFor context: NSDraggingContext) -> NSDragOperation {
        .copy
    }

    func draggingSession(_ s: NSDraggingSession, endedAt p: NSPoint, operation: NSDragOperation) {
        log("e: drag ended, operation \(operation == [] ? "none (nothing took it)" : String(operation.rawValue))")
        finish(0, after: 0.5)
    }
}

var dropPoint = CGPoint.zero // global display coordinates, top-left origin
var window: NSWindow?
var globalMonitor: Any?

func post(_ type: CGEventType, _ p: CGPoint) {
    let ev = CGEvent(mouseEventSource: CGEventSource(stateID: .hidSystemState), mouseType: type, mouseCursorPosition: p, mouseButton: .left)
    ev?.post(tap: .cghidEventTap)
}

func moveAndRelease() {
    // Wiggle a few pixels so AppKit sees a drag, end exactly on the point.
    let steps: [CGFloat] = [2, 5, 8, 5, 2, 0]
    for (i, d) in steps.enumerated() {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05 * Double(i)) {
            post(.leftMouseDragged, CGPoint(x: dropPoint.x + d, y: dropPoint.y + d))
        }
    }
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.05 * Double(steps.count) + 0.3) {
        post(.leftMouseUp, dropPoint)
        log("e: released")
    }
}

func cmdE(_ path: String) {
    guard let url = stage(path) else { return finish(1) }
    let trusted = AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary)
    log("e: allowed to post input (Accessibility): \(trusted)")
    if !trusted { return finish(1, after: 1) }
    log("e: point the mouse at the drop target and keep it still; dropping in 5 s")
    DispatchQueue.main.asyncAfter(deadline: .now() + 5) {
        let m = NSEvent.mouseLocation // Cocoa: bottom-left of the main screen
        let mainHeight = NSScreen.screens.first?.frame.height ?? 0
        dropPoint = CGPoint(x: m.x, y: mainHeight - m.y)
        let size: CGFloat = 24
        let w = NSWindow(contentRect: NSRect(x: m.x - size / 2, y: m.y - size / 2, width: size, height: size),
                         styleMask: .borderless, backing: .buffered, defer: false)
        w.level = .screenSaver
        // Fully opaque: the window server lets clicks through (nearly)
        // transparent pixels, and then the press lands on the app below.
        w.isOpaque = true
        w.backgroundColor = .systemBlue
        let v = DragView(frame: NSRect(x: 0, y: 0, width: size, height: size))
        v.file = url
        w.contentView = v
        w.orderFrontRegardless()
        window = w
        log("e: mouse at \(Int(m.x)),\(Int(m.y)) (Cocoa), \(NSScreen.screens.count) screen(s), main \(NSScreen.screens.first.map { "\($0.frame)" } ?? "-")")
        log("e: window \(w.frame), visible \(w.isVisible), on screen \(w.occlusionState.contains(.visible)); pressing")
        globalMonitor = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown, .leftMouseUp]) { ev in
            log("e: \(ev.type == .leftMouseDown ? "mouseDown" : "mouseUp") went to another app at \(Int(ev.locationInWindow.x)),\(Int(ev.locationInWindow.y))")
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { post(.leftMouseDown, dropPoint) }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) {
            guard !v.started else { return }
            // The posted press didn't reach us: start the drag from a press
            // we make up ourselves, while the posted button is still down.
            log("e: fallback: starting the drag from a synthesized event")
            if let ev = NSEvent.mouseEvent(with: .leftMouseDown, location: NSPoint(x: size / 2, y: size / 2),
                                           modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                                           windowNumber: w.windowNumber, context: nil, eventNumber: 0,
                                           clickCount: 1, pressure: 1) {
                v.mouseDown(with: ev)
            }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 5) {
            log("e: no drag end after 5 s")
            post(.leftMouseUp, dropPoint)
            finish(1, after: 0.5)
        }
    }
}

// MARK: main

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let args = Array(CommandLine.arguments.dropFirst())
log("spike: \(args.first ?? "(none)") on macOS \(ProcessInfo.processInfo.operatingSystemVersionString)")
switch (args.first, args.count) {
case ("a", 2): cmdA(args[1])
case ("b", 1): cmdB()
case ("d", 1): cmdD()
case ("e", 2): cmdE(args[1])
default:
    log("usage: a <file> | b | d | e <file>")
    finish(64)
}
app.run()
