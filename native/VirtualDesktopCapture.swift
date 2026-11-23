import AppKit
import CoreGraphics
import ScreenCaptureKit

// The listener owns the virtual display. Its capture child only follows that
// display's dimensions; it never configures a physical display.
@available(macOS 13.0, *)
final class VirtualDesktopCapture {
    private var pending: Task<Void, Never>?
    private var lastRequest = ""

    @MainActor func request(width: Int, height: Int, scale: Int) {
        guard (320...1920).contains(width), (320...1920).contains(height),
              (100...400).contains(scale) else { return }
        let key = "\(width)x\(height)@\(scale)"
        guard key != lastRequest else { return }
        lastRequest = key
        pending?.cancel()
        pending = Task { @MainActor in
            do { try await Task.sleep(nanoseconds: 200_000_000) } catch { return }
            guard !stopping, sessionLockState() == 0,
                  let capture = activeCapture as? Capture, capture.isVirtualDisplay,
                  let id = capture.displayID, CGDisplayVendorNumber(id) == 0x594954,
                  let configuration = capture.configuration, let stream = capture.stream else { return }
            releasePointer(capture)
            do {
                let sources = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)
                guard !Task.isCancelled, !stopping,
                      let source = sources.displays.first(where: { $0.displayID == id }) else { return }
                let filter = SCContentFilter(display: source, excludingWindows: [])
                let w: Double, h: Double
                if #available(macOS 14.0, *) {
                    w = filter.contentRect.width * Double(filter.pointPixelScale)
                    h = filter.contentRect.height * Double(filter.pointPixelScale)
                } else {
                    guard let mode = CGDisplayCopyDisplayMode(id) else { return }
                    w = Double(mode.pixelWidth); h = Double(mode.pixelHeight)
                }
                guard w > 0, h > 0, w <= 3840, h <= 3840 else {
                    throw CaptureFailure(message: "Virtual display exceeds the capture memory budget")
                }
                configuration.width = Int(w); configuration.height = Int(h)
                try await stream.updateContentFilter(filter)
                try await stream.updateConfiguration(configuration)
                capture.sourceAspect = w / h
            } catch {
                if !Task.isCancelled {
                    reportFailure("Could not update iPad virtual display capture: \(error.localizedDescription)")
                }
            }
        }
    }

    func cancel() { pending?.cancel(); pending = nil }
}

@_cdecl("yiterm_capture_resize_desktop")
public func resizeDesktop(_ width: Int32, _ height: Int32, _ scale: Int32) {
    DispatchQueue.main.async {
        guard #available(macOS 13.0, *), let capture = activeCapture as? Capture,
              capture.isVirtualDisplay else { return }
        capture.virtualDesktopCapture.request(width: Int(width), height: Int(height), scale: Int(scale))
    }
}
