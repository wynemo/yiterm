import AppKit
import ScreenCaptureKit
import CoreMedia
import CoreVideo

// Private framed RGB pipe to the Rust encoder. stdout is never used for diagnostics.
@available(macOS 13.0, *)
final class Capture: NSObject, SCStreamOutput, SCStreamDelegate {
    var stream: SCStream?
    var pixels = [UInt8]()
    func stream(_ stream: SCStream, didStopWithError error: Error) {
        fail("Capture stopped: \(error.localizedDescription)")
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
        var header = [UInt32(w).littleEndian, UInt32(h).littleEndian]
        do {
            try header.withUnsafeBytes { try FileHandle.standardOutput.write(contentsOf: $0) }
            try pixels.withUnsafeBytes { try FileHandle.standardOutput.write(contentsOf: $0) }
        } catch { exit(0) } // Parent closed the pipe.
    }
}
func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data(("YiTerm screen sharing: " + message + "\n").utf8))
    exit(1)
}
@available(macOS 13.0, *)
func start() async {
    let args = Array(CommandLine.arguments.dropFirst())
    func value(_ flag: String) -> String? {
        guard let i = args.firstIndex(of: flag), i + 1 < args.count else { return nil }
        return args[i + 1]
    }
    guard CGPreflightScreenCaptureAccess() else {
        _ = CGRequestScreenCaptureAccess()
        fail("Screen Recording permission required. Allow YiTerm / its capture helper in System Settings > Privacy & Security > Screen Recording, then retry. SSH also requires an active desktop session.")
    }
    do {
        let content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)
        if args.contains("--list") {
            for d in content.displays { print("display\t\(d.displayID)\t\(d.width)x\(d.height)") }
            for w in content.windows where w.owningApplication != nil {
                print("window\t\(w.windowID)\t\(w.owningApplication?.applicationName ?? "")\t\((w.title ?? "").replacingOccurrences(of: "\n", with: " "))")
            }
            exit(0)
        }
        let filter: SCContentFilter
        let width: Int, height: Int
        if let id = value("--window").flatMap(UInt32.init) {
            guard let window = content.windows.first(where: { $0.windowID == id }) else { fail("Window not found: \(id)") }
            filter = SCContentFilter(desktopIndependentWindow: window)
            width = max(1, Int(window.frame.width)); height = max(1, Int(window.frame.height))
        } else {
            let id = value("--display").flatMap(UInt32.init)
            guard let display = content.displays.first(where: { id == nil || $0.displayID == id }) else { fail("No available display / active desktop session") }
            filter = SCContentFilter(display: display, excludingWindows: [])
            width = display.width; height = display.height
        }
        let maxWidth = Int(value("--width") ?? "1280") ?? 1280
        let scale = min(1.0, Double(maxWidth) / Double(width))
        let config = SCStreamConfiguration()
        config.width = max(1, Int(Double(width) * scale)); config.height = max(1, Int(Double(height) * scale))
        config.pixelFormat = kCVPixelFormatType_32BGRA
        config.colorSpaceName = CGColorSpace.sRGB
        config.minimumFrameInterval = CMTime(value: 1, timescale: Int32(value("--fps") ?? "15") ?? 15)
        config.queueDepth = 3; config.showsCursor = true; config.capturesAudio = false
        if #available(macOS 14.0, *) { config.scalesToFit = true }
        let capture = Capture()
        let stream = SCStream(filter: filter, configuration: config, delegate: capture)
        capture.stream = stream
        try stream.addStreamOutput(capture, type: .screen, sampleHandlerQueue: DispatchQueue(label: "yiterm.capture"))
        try await stream.startCapture()
        // Keep delegate and stream alive while AppKit's main run loop services ScreenCaptureKit.
        activeCapture = capture
    } catch { fail(error.localizedDescription) }
}
var activeCapture: AnyObject?
let app = NSApplication.shared
app.setActivationPolicy(.prohibited)
if #available(macOS 13.0, *) {
    Task { await start() }
    app.run()
} else { fail("macOS 13 or later is required") }
