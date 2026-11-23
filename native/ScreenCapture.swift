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
    let virtualDesktopCapture = VirtualDesktopCapture()
    var isVirtualDisplay = false
    var stream: SCStream?
    var configuration: SCStreamConfiguration?
    var sessionPaused = false // main thread
    var paused = false // capture queue
    var announceRecovery = false // capture queue
    var displayID: CGDirectDisplayID?
    var windowID: CGWindowID?
    var windowPID: pid_t?
    var controlEnabled = false
    var sourceAspect = 1.0
    var heldButtons = Set<Int32>()
    var lastPointer = CGPoint.zero
    var lastClick: (Int32, CGPoint, TimeInterval, Int64)?
    func stream(_ stream: SCStream, didStopWithError error: Error) {
        if sessionLockState() == 1 { return }
        reportFailure("Capture stopped: \(error.localizedDescription)")
    }
    func stream(_ stream: SCStream, didOutputSampleBuffer sample: CMSampleBuffer,
                of type: SCStreamOutputType) {
        guard !paused, sessionLockState() != 1, type == .screen, sample.isValid,
              let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let status = attachments.first?[.status] as? Int,
              status == SCFrameStatus.complete.rawValue,
              let buffer = sample.imageBuffer else { return }
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(buffer) else { return }
        let w = CVPixelBufferGetWidth(buffer), h = CVPixelBufferGetHeight(buffer)
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        // Borrow ScreenCaptureKit's BGRA storage while locked. Rust copies it
        // once; only dirty tiles are converted to RGB by the encoder workers.
        if announceRecovery {
            frameCallback?(nil, 0, -1, 0, nil, callbackContext)
            announceRecovery = false
        }
        frameCallback?(base.assumingMemoryBound(to: UInt8.self), stride*h, Int32(w), Int32(h), nil, callbackContext)
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
        var content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)
        let virtualID = value("--virtual-display-id").flatMap(UInt32.init)
        if let virtualID {
            for _ in 0..<20 where !content.displays.contains(where: { $0.displayID == virtualID }) {
                try await Task.sleep(nanoseconds: 100_000_000)
                guard !Task.isCancelled else { return }
                content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)
            }
            guard CGDisplayVendorNumber(virtualID) == 0x594954,
                  content.displays.contains(where: { $0.displayID == virtualID }),
                  value("--display").flatMap(UInt32.init) == virtualID else {
                throw CaptureFailure(message: "The iPad virtual display is unavailable; physical display capture was not started")
            }
        }
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
            guard sessionLockState() >= 0 else { throw CaptureFailure(message: "Remote control requires the current logged-in console user") }
            let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
            guard AXIsProcessTrustedWithOptions(options) else {
                throw CaptureFailure(message: "Remote control requires Accessibility permission. In System Settings > Privacy & Security > Accessibility authorize the application responsible for this session (for SSH this may be /usr/libexec/sshd-keygen-wrapper), then reconnect. Screen Recording alone does not permit control. Use --share-read-only for viewing without remote input.")
            }
        }
        let filter: SCContentFilter
        var width: Int, height: Int
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
        if #available(macOS 14.0, *) {
            let rect=filter.contentRect
            let pixelScale=Double(filter.pointPixelScale)
            guard rect.width>0,rect.height>0,pixelScale>0 else { throw CaptureFailure(message:"Invalid source capture dimensions") }
            width=max(1,Int(rect.width*pixelScale)); height=max(1,Int(rect.height*pixelScale))
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
        config.queueDepth = 3; config.showsCursor = !control; config.capturesAudio = false
        if #available(macOS 14.0, *) { config.scalesToFit = true; config.preservesAspectRatio = true; config.ignoreShadowsSingleWindow = true }
        let capture = Capture()
        capture.displayID = selectedDisplay; capture.windowID = selectedWindow; capture.windowPID = selectedPID; capture.controlEnabled = control
        capture.isVirtualDisplay = virtualID != nil
        capture.sourceAspect = Double(width) / Double(height)
        capture.configuration = config
        let stream = SCStream(filter: filter, configuration: config, delegate: capture)
        capture.stream = stream
        try stream.addStreamOutput(capture, type: .screen, sampleHandlerQueue: capture.queue)
        try await stream.startCapture()
        if Task.isCancelled { try? await stream.stopCapture(); return }
        // Keep delegate and stream alive while AppKit's main run loop services ScreenCaptureKit.
        activeCapture = capture
        monitorTask = Task { @MainActor in
            while !Task.isCancelled && !stopping {
                try? await Task.sleep(nanoseconds: 500_000_000)
                if Task.isCancelled || stopping { break }
                if sessionLockState() == 1 && !capture.sessionPaused {
                    releasePointer(capture)
                    capture.sessionPaused = true
                    capture.queue.sync {
                        capture.paused = true
                        frameCallback?(nil, 0, -1, 1, nil, callbackContext)
                    }
                    try? await capture.stream?.stopCapture()
                    capture.stream = nil
                } else if sessionLockState() == 0 && capture.sessionPaused {
                    do {
                        let sources = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)
                        guard !Task.isCancelled && !stopping else { break }
                        let filter: SCContentFilter
                        if let id = capture.displayID, let source = sources.displays.first(where: { $0.displayID == id }) {
                            filter = SCContentFilter(display: source, excludingWindows: [])
                        } else if let id = capture.windowID, let source = sources.windows.first(where: { $0.windowID == id }) {
                            filter = SCContentFilter(desktopIndependentWindow: source)
                        } else { throw CaptureFailure(message: "Original capture source no longer exists after unlock; restart and select it again") }
                        guard let config = capture.configuration else { throw CaptureFailure(message: "Missing capture configuration") }
                        let stream = SCStream(filter: filter, configuration: config, delegate: capture)
                        capture.stream = stream
                        capture.queue.sync { capture.paused = false; capture.announceRecovery = true }
                        try stream.addStreamOutput(capture, type: .screen, sampleHandlerQueue: capture.queue)
                        try await stream.startCapture()
                        capture.sessionPaused = false
                    } catch {
                        if sessionLockState() == 1 { continue }
                        reportFailure("Could not restore original capture after unlock: \(error.localizedDescription)")
                        break
                    }
                }
            }
        }
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
var monitorTask: Task<Void, Never>?
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
            monitorTask?.cancel()
            await monitorTask?.value
            monitorTask = nil
            captureTask?.cancel()
            await captureTask?.value
            captureTask = nil
            if #available(macOS 13.0, *), let capture = activeCapture as? Capture {
                releasePointer(capture)
                try? await capture.stream?.stopCapture()
                capture.virtualDesktopCapture.cancel()
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
@available(macOS 13.0, *)
func releasePointer(_ capture: Capture) {
    for button in capture.heldButtons {
        let type: CGEventType = button == 0 ? .leftMouseUp : (button == 2 ? .rightMouseUp : .otherMouseUp)
        let mouse: CGMouseButton = button == 0 ? .left : (button == 2 ? .right : .center)
        if let event=CGEvent(mouseEventSource:nil,mouseType:type,mouseCursorPosition:capture.lastPointer,mouseButton:mouse) {
            event.flags=[]; event.post(tap:.cghidEventTap)
        }
    }
    capture.heldButtons.removeAll()
}
private let pointerLock=NSLock()
private var pendingPointer:(Double,Double,Int32,Int32)?
private var pointerScheduled=false
@_cdecl("yiterm_capture_pointer")
public func pointerCapture(_ x:Double,_ y:Double,_ button:Int32,_ phase:Int32,_ modifiers:Int32) {
    guard x.isFinite,y.isFinite,(0...4).contains(phase) else { return }
    if phase == 0 {
        pointerLock.lock()
        pendingPointer=(x,y,button,modifiers)
        let schedule = !pointerScheduled
        pointerScheduled=true
        pointerLock.unlock()
        if schedule { DispatchQueue.main.async {
            pointerLock.lock(); let point=pendingPointer; pendingPointer=nil; pointerScheduled=false; pointerLock.unlock()
            if let point { applyPointer(point.0,point.1,point.2,0,point.3) }
        } }
    } else { DispatchQueue.main.async { applyPointer(x,y,button,phase,modifiers) } }
}
private func applyPointer(_ x:Double,_ y:Double,_ button:Int32,_ phase:Int32,_ modifiers:Int32) {
        guard #available(macOS 13.0,*), let capture=activeCapture as? Capture else { return }
        if phase == 3 { releasePointer(capture); return }
        guard !stopping,sessionLockState()==0,capture.controlEnabled,AXIsProcessTrusted(),x>=0,x<1,y>=0,y<1 else {
            if phase == 2 { releasePointer(capture) }; return
        }
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
            if phase == 1 {
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
            }
            bounds = frame
        } else { return }
        let point=CGPoint(x:bounds.minX+x*bounds.width,y:bounds.minY+y*bounds.height)
        capture.lastPointer=point
        var flags=CGEventFlags()
        if modifiers&4 != 0 { flags.insert(.maskShift) }
        if modifiers&8 != 0 { flags.insert(.maskAlternate) }
        if modifiers&16 != 0 { flags.insert(.maskControl) }
        if phase == 4 {
            guard button == 1 || button == -1,
                  let event = CGEvent(scrollWheelEvent2Source: nil, units: .line,
                                      wheelCount: 1, wheel1: button, wheel2: 0, wheel3: 0) else { return }
            event.location = point
            event.flags = flags
            if let pid = capture.windowPID {
                guard keyboardAllowed() else { return }
                event.postToPid(pid)
            } else {
                event.post(tap: .cghidEventTap)
            }
            return
        }
        let actual: Int32
        let type: CGEventType
        if phase == 0 {
            actual=capture.heldButtons.sorted().first ?? 0
            type=capture.heldButtons.isEmpty ? .mouseMoved : (actual==0 ? .leftMouseDragged : (actual==2 ? .rightMouseDragged : .otherMouseDragged))
        } else {
            guard (0...2).contains(button) else { if phase==2 { releasePointer(capture) }; return }
            actual=button
            if phase==1 {
                if capture.heldButtons.contains(button) { return }
                capture.heldButtons.insert(button)
                let now=ProcessInfo.processInfo.systemUptime
                var clicks:Int64=1
                if let last=capture.lastClick,last.0==button,now-last.2<=NSEvent.doubleClickInterval,hypot(point.x-last.1.x,point.y-last.1.y)<=4 { clicks=min(last.3+1,3) }
                capture.lastClick=(button,point,now,clicks)
            } else {
                guard capture.heldButtons.remove(button) != nil else { return }
            }
            type=phase==1 ? (actual==0 ? .leftMouseDown : (actual==2 ? .rightMouseDown : .otherMouseDown)) : (actual==0 ? .leftMouseUp : (actual==2 ? .rightMouseUp : .otherMouseUp))
        }
        let mouse:CGMouseButton=actual==0 ? .left : (actual==2 ? .right : .center)
        if let event=CGEvent(mouseEventSource:nil,mouseType:type,mouseCursorPosition:point,mouseButton:mouse) {
            event.flags=flags
            if phase != 0 { event.setIntegerValueField(.mouseEventClickState,value:capture.lastClick?.3 ?? 1) }
            event.post(tap:.cghidEventTap)
        }
}

@available(macOS 13.0, *)
func keyboardAllowed() -> Bool {
    guard !stopping, sessionLockState() == 0, let capture = activeCapture as? Capture, capture.controlEnabled,
          AXIsProcessTrusted() else { return false }
    // A window share only types into that application's already-focused window.
    if let pid = capture.windowPID {
        guard NSWorkspace.shared.frontmostApplication?.processIdentifier == pid,
              let id = capture.windowID,
              let infos = CGWindowListCopyWindowInfo(.optionIncludingWindow, id) as? [[String: Any]],
              let dict = infos.first?[kCGWindowBounds as String] as? [String: Any],
              let expected = CGRect(dictionaryRepresentation: dict as CFDictionary) else { return false }
        let app = AXUIElementCreateApplication(pid)
        var focused: CFTypeRef?
        guard AXUIElementCopyAttributeValue(app, kAXFocusedWindowAttribute as CFString, &focused) == .success,
              let focused, CFGetTypeID(focused) == AXUIElementGetTypeID() else { return false }
        let window = unsafeBitCast(focused, to: AXUIElement.self)
        var position: CFTypeRef?, size: CFTypeRef?
        guard AXUIElementCopyAttributeValue(window, kAXPositionAttribute as CFString, &position) == .success,
              AXUIElementCopyAttributeValue(window, kAXSizeAttribute as CFString, &size) == .success,
              let position, let size, CFGetTypeID(position) == AXValueGetTypeID(), CFGetTypeID(size) == AXValueGetTypeID() else { return false }
        var origin = CGPoint.zero, extent = CGSize.zero
        guard AXValueGetValue(unsafeBitCast(position, to: AXValue.self), .cgPoint, &origin),
              AXValueGetValue(unsafeBitCast(size, to: AXValue.self), .cgSize, &extent),
              abs(origin.x-expected.minX) < 2, abs(origin.y-expected.minY) < 2,
              abs(extent.width-expected.width) < 2, abs(extent.height-expected.height) < 2 else { return false }
    }
    return true
}
func keyboardFlags(_ modifiers: UInt32) -> CGEventFlags {
    var flags = CGEventFlags()
    if modifiers & 1 != 0 { flags.insert(.maskShift) }
    if modifiers & 2 != 0 { flags.insert(.maskAlternate) }
    if modifiers & 4 != 0 { flags.insert(.maskControl) }
    if modifiers & 8 != 0 { flags.insert(.maskCommand) }
    return flags
}
func postKey(_ code: CGKeyCode, _ flags: CGEventFlags) {
    guard let down = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: true),
          let up = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: false) else { return }
    for event in [down, up] { event.flags = flags; event.post(tap: .cghidEventTap) }
}
// UTF-16 chunks never split a surrogate pair. No clipboard or credential storage.
func postText(_ text: String, _ flags: CGEventFlags = []) {
    let utf16 = Array(text.utf16)
    var offset = 0
    while offset < utf16.count {
        guard #available(macOS 13.0, *), keyboardAllowed() else { return }
        var end = min(offset + 20, utf16.count)
        if end < utf16.count && (0xD800...0xDBFF).contains(utf16[end-1]) { end -= 1 }
        let chunk = Array(utf16[offset..<end])
        guard let down = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: true),
              let up = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: false) else { return }
        for event in [down, up] {
            event.flags = flags
            chunk.withUnsafeBufferPointer { event.keyboardSetUnicodeString(stringLength: $0.count, unicodeString: $0.baseAddress!) }
            event.post(tap: .cghidEventTap)
        }
        offset = end
    }
}
@_cdecl("yiterm_capture_text")
public func textCapture(_ bytes: UnsafePointer<UInt8>, _ length: Int) {
    guard length > 0, length <= 65536, let text = String(bytes: UnsafeBufferPointer(start: bytes, count: length), encoding: .utf8) else { return }
    DispatchQueue.main.async {
        guard #available(macOS 13.0, *), keyboardAllowed() else { return }
        postText(text)
    }
}
@_cdecl("yiterm_capture_key")
public func keyCapture(_ code: UInt32, _ modifiers: UInt32) {
    guard modifiers <= 15 else { return }
    DispatchQueue.main.async {
        guard #available(macOS 13.0, *), keyboardAllowed() else { return }
        var flags = keyboardFlags(modifiers)
        let special: [UInt32: CGKeyCode] = [
            0xe000:53, 0xe001:36, 0xe002:48, 0xe003:51, 0xe004:117, 0xe005:114,
            0xe006:115, 0xe007:119, 0xe008:116, 0xe009:121,
            0xe00a:126, 0xe00b:125, 0xe00c:123, 0xe00d:124,
            0xe010:122, 0xe011:120, 0xe012:99, 0xe013:118, 0xe014:96, 0xe015:97,
            0xe016:98, 0xe017:100, 0xe018:101, 0xe019:109, 0xe01a:103, 0xe01b:111]
        if let key = special[code] { postKey(key, flags); return }
        guard let scalar = UnicodeScalar(code) else { return }
        var character = Character(scalar)
        if character.isUppercase { character = Character(String(character).lowercased()); flags.insert(.maskShift) }
        let shifted = Array("~!@#$%^&*()_+{}|:\"<>?")
        let unshifted = Array("`1234567890-=[]\\;',./")
        if let index = shifted.firstIndex(of: character) { character = unshifted[index]; flags.insert(.maskShift) }
        let keys: [Character: CGKeyCode] = [
            "a":0,"s":1,"d":2,"f":3,"h":4,"g":5,"z":6,"x":7,"c":8,"v":9,"b":11,
            "q":12,"w":13,"e":14,"r":15,"y":16,"t":17,"1":18,"2":19,"3":20,"4":21,
            "6":22,"5":23,"=":24,"9":25,"7":26,"-":27,"8":28,"0":29,
            "]":30,"o":31,"u":32,"[":33,"i":34,"p":35,"l":37,"j":38,"'":39,"k":40,
            ";":41,"\\":42,",":43,"/":44,"n":45,"m":46,".":47," ":49,"`":50]
        if let key = keys[character] { postKey(key, flags) }
        else { postText(String(scalar), flags) }
    }
}
