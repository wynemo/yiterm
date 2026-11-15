import AppKit
import ApplicationServices
import ScreenCaptureKit
import CoreMedia
import CoreVideo

// Linked into YiTerm itself. Capture and encoding share one process/TCC identity.
public typealias FrameCallback = @convention(c) (UnsafePointer<UInt8>?, Int, Int32, Int32, UnsafePointer<CChar>?, UnsafeMutableRawPointer?) -> Void
var frameCallback: FrameCallback?
var callbackContext: UnsafeMutableRawPointer?
var captureArguments = [String]()
var captureFailed = false
struct CaptureFailure: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}
func reportFailure(_ message: String) {
    captureFailed = true
    message.withCString { frameCallback?(nil, 0, 0, 0, $0, callbackContext) }
    stopCapture()
}
@available(macOS 13.0, *)
final class Capture: NSObject, SCStreamOutput, SCStreamDelegate {
    let queue = DispatchQueue(label: "yiterm.capture")
    var stream: SCStream?
    var pixels = [UInt8]()
    var displayID: CGDirectDisplayID?
    var windowID: CGWindowID?
    var windowPID: pid_t?
    var controlEnabled = false
    var sourceAspect = 1.0
    var lastClick: (Int32, CGPoint, TimeInterval, Int64)?
    func stream(_ stream: SCStream, didStopWithError error: Error) {
        reportFailure("Capture stopped: \(error.localizedDescription)")
    }
    func stream(_ stream: SCStream, didOutputSampleBuffer sample: CMSampleBuffer,
                of type: SCStreamOutputType) {
        guard type == .screen, sample.isValid,
              let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let status = attachments.first?[.status] as? Int,
              status == SCFrameStatus.complete.rawValue,
              let buffer = sample.imageBuffer else { return }
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(buffer) else { return }
        let w = CVPixelBufferGetWidth(buffer), h = CVPixelBufferGetHeight(buffer)
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        if pixels.count != w * h * 3 { pixels = [UInt8](repeating: 0, count: w * h * 3) }
        let source = base.assumingMemoryBound(to: UInt8.self)
        for y in 0..<h {
            for x in 0..<w {
                let s = y * stride + x * 4, d = (y * w + x) * 3
                pixels[d] = source[s + 2]; pixels[d + 1] = source[s + 1]; pixels[d + 2] = source[s]
            }
        }
        pixels.withUnsafeBufferPointer {
            frameCallback?($0.baseAddress, $0.count, Int32(w), Int32(h), nil, callbackContext)
        }
    }
}
@available(macOS 13.0, *)
@MainActor func start() async {
    let args = captureArguments
    func value(_ flag: String) -> String? {
        guard let i = args.firstIndex(of: flag), i + 1 < args.count else { return nil }
        return args[i + 1]
    }
    do {
        // Let ScreenCaptureKit make the actual access decision. Preflight alone
        // can disagree with the current launch context and is not a capture call.
        let content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)
        guard !Task.isCancelled else { return }
        if args.contains("--list") {
            for d in content.displays { print("display\t\(d.displayID)\t\(d.width)x\(d.height)") }
            for w in content.windows where w.owningApplication != nil {
                print("window\t\(w.windowID)\t\(w.owningApplication?.applicationName ?? "")\t\((w.title ?? "").replacingOccurrences(of: "\n", with: " "))")
            }
            stopCapture()
            return
        }
        let control = args.contains("--control")
        if control {
            let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
            guard AXIsProcessTrustedWithOptions(options) else {
                throw CaptureFailure(message: "Remote clicks require Accessibility permission. In System Settings > Privacy & Security > Accessibility authorize the application responsible for this session (for SSH this may be /usr/libexec/sshd-keygen-wrapper), then reconnect. Screen Recording alone does not permit control. Use --share-read-only for viewing without remote clicks.")
            }
        }
        let filter: SCContentFilter
        let width: Int, height: Int
        var selectedDisplay: CGDirectDisplayID?
        var selectedWindow: CGWindowID?
        var selectedPID: pid_t?
        if let id = value("--window").flatMap(UInt32.init) {
            guard let window = content.windows.first(where: { $0.windowID == id }) else { throw CaptureFailure(message: "Window not found: \(id)") }
            filter = SCContentFilter(desktopIndependentWindow: window)
            selectedWindow = window.windowID; selectedPID = window.owningApplication?.processID
            let pixelScale: Double
            if #available(macOS 14.0, *) { pixelScale = Double(filter.pointPixelScale) }
            else { pixelScale = Double(NSScreen.main?.backingScaleFactor ?? 1) }
            width = max(1, Int(window.frame.width * pixelScale)); height = max(1, Int(window.frame.height * pixelScale))
        } else {
            let id = value("--display").flatMap(UInt32.init)
            guard let display = content.displays.first(where: { id == nil || $0.displayID == id }) else { throw CaptureFailure(message: "No available display / active desktop session") }
            filter = SCContentFilter(display: display, excludingWindows: [])
            selectedDisplay = display.displayID
            width = CGDisplayPixelsWide(display.displayID); height = CGDisplayPixelsHigh(display.displayID)
        }
        let requestedWidth = Int(value("--width") ?? "0") ?? 0
        let maxWidth = requestedWidth == 0 ? 3840 : requestedWidth
        let scale = min(1.0, Double(maxWidth) / Double(width), 16384.0 / Double(height),
                        sqrt(Double(64 * 1024 * 1024) / (Double(width) * Double(height) * 3)))
        let config = SCStreamConfiguration()
        config.width = max(1, Int(Double(width) * scale)); config.height = max(1, Int(Double(height) * scale))
        config.pixelFormat = kCVPixelFormatType_32BGRA
        config.colorSpaceName = CGColorSpace.sRGB
        config.minimumFrameInterval = CMTime(value: 1, timescale: Int32(value("--fps") ?? "15") ?? 15)
        config.queueDepth = 3; config.showsCursor = true; config.capturesAudio = false
        if #available(macOS 14.0, *) { config.scalesToFit = true; config.ignoreShadowsSingleWindow = true }
        let capture = Capture()
        capture.displayID = selectedDisplay; capture.windowID = selectedWindow; capture.windowPID = selectedPID; capture.controlEnabled = control
        capture.sourceAspect = Double(width) / Double(height)
        let stream = SCStream(filter: filter, configuration: config, delegate: capture)
        capture.stream = stream
        try stream.addStreamOutput(capture, type: .screen, sampleHandlerQueue: capture.queue)
        try await stream.startCapture()
        if Task.isCancelled { try? await stream.stopCapture(); return }
        // Keep delegate and stream alive while AppKit's main run loop services ScreenCaptureKit.
        activeCapture = capture
    } catch {
        let ns = error as NSError
        var message = "\(ns.domain) (\(ns.code)): \(error.localizedDescription)"
        if ns.domain == "com.apple.ScreenCaptureKit.SCStreamErrorDomain" && ns.code == -3801 {
            let env = ProcessInfo.processInfo.environment
            if env["SSH_CONNECTION"] != nil || env["SSH_CLIENT"] != nil {
                message += "\nStarted over SSH: macOS can attribute this request to /usr/libexec/sshd-keygen-wrapper instead of yiterm. In System Settings > Privacy & Security > Screen & System Audio Recording, add and allow /usr/libexec/sshd-keygen-wrapper, then reconnect SSH. Granting yiterm alone does not authorize the SSH responsible process. Full Disk Access is a different permission and does not grant screen recording."
            } else {
                message += "\nScreen Recording was denied for this launch context. Executable: \(Bundle.main.executablePath ?? CommandLine.arguments[0]). Authorize this executable or the terminal application responsible for launching it in System Settings > Privacy & Security > Screen Recording, then restart it."
            }
        }
        reportFailure(message)
    }
}
var activeCapture: AnyObject?
var captureTask: Task<Void, Never>?
var stopping = false

// Rust calls this on the process main thread; AppKit services ScreenCaptureKit
// while a Rust worker encodes frames and writes the terminal byte stream.
@_cdecl("yiterm_capture_run")
public func runCapture(_ argc: Int32, _ argv: UnsafePointer<UnsafePointer<CChar>?>,
                       _ callback: FrameCallback?, _ context: UnsafeMutableRawPointer?) -> Int32 {
    captureArguments = (0..<Int(argc)).compactMap { argv[$0].map { String(cString: $0) } }
    frameCallback = callback; callbackContext = context; captureFailed = false; stopping = false
    let app = NSApplication.shared
    app.setActivationPolicy(.prohibited)
    if #available(macOS 13.0, *) {
        captureTask = Task { await start() }
        app.run()
    } else {
        "macOS 13 or later is required".withCString { callback?(nil, 0, 0, 0, $0, context) }
        captureFailed = true
    }
    frameCallback = nil; callbackContext = nil
    return captureFailed ? 1 : 0
}

@_cdecl("yiterm_capture_stop")
public func stopCapture() {
    DispatchQueue.main.async {
        guard !stopping else { return }
        stopping = true
        Task { @MainActor in
            captureTask?.cancel()
            await captureTask?.value
            captureTask = nil
            if #available(macOS 13.0, *), let capture = activeCapture as? Capture {
                try? await capture.stream?.stopCapture()
                capture.queue.sync {} // Drain frame callbacks before Rust releases their context.
                activeCapture = nil
            }
            let app = NSApplication.shared
            app.stop(nil)
            if let event = NSEvent.otherEvent(with: .applicationDefined, location: .zero,
                    modifierFlags: [], timestamp: 0, windowNumber: 0, context: nil,
                    subtype: 0, data1: 0, data2: 0) {
                app.postEvent(event, atStart: true)
            }
        }
    }
}

// The renderer passes normalized positions in the displayed image. Use global
// Core Graphics points, not Retina pixels, to reach the selected display.
@_cdecl("yiterm_capture_click")
public func clickCapture(_ x: Double, _ y: Double, _ button: Int32, _ modifiers: Int32) {
    guard x.isFinite, y.isFinite, x >= 0, x < 1, y >= 0, y < 1, (0...2).contains(button) else { return }
    DispatchQueue.main.async {
        guard #available(macOS 13.0, *), !stopping,
              let capture = activeCapture as? Capture, capture.controlEnabled,
              AXIsProcessTrusted() else { return }
        let bounds: CGRect
        if let display = capture.displayID {
            guard CGDisplayIsActive(display) != 0 else { return }
            bounds = CGDisplayBounds(display)
        } else if let id = capture.windowID, let pid = capture.windowPID {
            guard let infos = CGWindowListCopyWindowInfo(.optionIncludingWindow, id) as? [[String: Any]],
                  let info = infos.first, let dict = info[kCGWindowBounds as String] as? [String: Any],
                  let frame = CGRect(dictionaryRepresentation: dict as CFDictionary) else { return }
            // A resized window can be letterboxed in the fixed capture buffer.
            // Ignore clicks until restarted rather than map to the wrong target.
            guard abs(frame.width / frame.height - capture.sourceAspect) < 0.01 else { return }
            // Raise the matching AX window before posting a click. Captured
            // windows can be occluded; never click through an unrelated window.
            let app = AXUIElementCreateApplication(pid)
            var value: CFTypeRef?
            guard AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &value) == .success,
                  let windows = value as? [AXUIElement] else { return }
            var raised = false
            for window in windows {
                var position: CFTypeRef?, size: CFTypeRef?
                guard AXUIElementCopyAttributeValue(window, kAXPositionAttribute as CFString, &position) == .success,
                      AXUIElementCopyAttributeValue(window, kAXSizeAttribute as CFString, &size) == .success,
                      let position, let size,
                      CFGetTypeID(position) == AXValueGetTypeID(), CFGetTypeID(size) == AXValueGetTypeID() else { continue }
                var origin = CGPoint.zero, extent = CGSize.zero
                guard AXValueGetValue(unsafeBitCast(position, to: AXValue.self), .cgPoint, &origin),
                      AXValueGetValue(unsafeBitCast(size, to: AXValue.self), .cgSize, &extent),
                      abs(origin.x-frame.minX) < 2, abs(origin.y-frame.minY) < 2,
                      abs(extent.width-frame.width) < 2, abs(extent.height-frame.height) < 2 else { continue }
                raised = AXUIElementPerformAction(window, kAXRaiseAction as CFString) == .success
                break
            }
            guard raised else { return }
            NSRunningApplication(processIdentifier: pid)?.activate(options: .activateIgnoringOtherApps)
            bounds = frame
        } else { return }
        let point = CGPoint(x: bounds.minX + x * bounds.width, y: bounds.minY + y * bounds.height)
        let mouseButton: CGMouseButton = button == 0 ? .left : (button == 2 ? .right : .center)
        let downType: CGEventType = button == 0 ? .leftMouseDown : (button == 2 ? .rightMouseDown : .otherMouseDown)
        let upType: CGEventType = button == 0 ? .leftMouseUp : (button == 2 ? .rightMouseUp : .otherMouseUp)
        let now = ProcessInfo.processInfo.systemUptime
        var clicks: Int64 = 1
        if let last = capture.lastClick, last.0 == button, now - last.2 <= NSEvent.doubleClickInterval,
           hypot(point.x-last.1.x, point.y-last.1.y) <= 4 {
            clicks = min(last.3 + 1, 3)
        }
        capture.lastClick = (button, point, now, clicks)
        var flags = CGEventFlags()
        if modifiers & 4 != 0 { flags.insert(.maskShift) }
        if modifiers & 8 != 0 { flags.insert(.maskAlternate) }
        if modifiers & 16 != 0 { flags.insert(.maskControl) }
        guard let down = CGEvent(mouseEventSource: nil, mouseType: downType, mouseCursorPosition: point, mouseButton: mouseButton),
              let up = CGEvent(mouseEventSource: nil, mouseType: upType, mouseCursorPosition: point, mouseButton: mouseButton) else { return }
        for event in [down, up] {
            event.flags = flags
            event.setIntegerValueField(.mouseEventClickState, value: clicks)
            event.post(tap: .cghidEventTap)
        }
    }
}
