// Adapted from myremoteapp's manual loginwindow unlock flow.
import AppKit
import ApplicationServices
import Security
import IOKit.pwr_mgt
import Darwin

func sessionLockState() -> Int32 {
    let uid = getuid()
    guard uid != 0, uid == geteuid(),
          let session = CGSessionCopyCurrentDictionary() as? [String: Any],
          (session["kCGSSessionUserIDKey"] as? NSNumber)?.uint32Value == uid,
          (session["kCGSSessionOnConsoleKey"] as? Bool) == true,
          (session["kCGSessionLoginDoneKey"] as? Bool) == true else { return -1 }
    return (session["CGSSessionScreenIsLocked"] as? Bool) == true ? 1 : 0
}
@_cdecl("yiterm_capture_locked")
public func captureLocked() -> Int32 { sessionLockState() }

private struct UnlockUnavailable: Error {}
private func ax(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success else { return nil }
    return value
}
private func signedLoginwindow(_ pid: pid_t) -> Bool {
    var code: SecCode?, requirement: SecRequirement?
    let flags = SecCSFlags(rawValue: 0)
    guard SecCodeCopyGuestWithAttributes(nil, [kSecGuestAttributePid as String: pid] as CFDictionary, flags, &code) == errSecSuccess,
          SecRequirementCreateWithString("identifier \"com.apple.loginwindow\" and anchor apple" as CFString, flags, &requirement) == errSecSuccess,
          let code, let requirement else { return false }
    return SecCodeCheckValidity(code, flags, requirement) == errSecSuccess
}
private struct UnlockNode {
    let element: AXUIElement
    let id: String
    let role: String
    let subrole: String
    let parent: Int?
}
private struct UnlockTarget {
    let pid: pid_t
    let window: AXUIElement
    let field: AXUIElement?
    let button: AXUIElement?
}
private func walkUnlock(_ element: AXUIElement, _ parent: Int?, _ depth: Int,
                        _ deadline: TimeInterval, _ nodes: inout [UnlockNode]) throws {
    guard ProcessInfo.processInfo.systemUptime < deadline, depth <= 12, nodes.count < 256,
          !nodes.contains(where: { CFEqual($0.element, element) }) else { throw UnlockUnavailable() }
    AXUIElementSetMessagingTimeout(element, 0.1)
    let index = nodes.count
    nodes.append(UnlockNode(element: element, id: ax(element, "AXIdentifier") as? String ?? "",
        role: ax(element, "AXRole") as? String ?? "", subrole: ax(element, "AXSubrole") as? String ?? "", parent: parent))
    if let children = ax(element, "AXChildren") {
        guard let children = children as? [AXUIElement], children.count <= 256 else { throw UnlockUnavailable() }
        for child in children { try walkUnlock(child, index, depth+1, deadline, &nodes) }
    }
}
private func unlockTarget() throws -> UnlockTarget {
    guard sessionLockState() == 1, AXIsProcessTrusted() else { throw UnlockUnavailable() }
    let apps = NSRunningApplication.runningApplications(withBundleIdentifier: "com.apple.loginwindow")
    guard apps.count == 1, let app = apps.first,
          app.executableURL?.path == "/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow",
          signedLoginwindow(app.processIdentifier) else { throw UnlockUnavailable() }
    let root = AXUIElementCreateApplication(app.processIdentifier)
    AXUIElementSetMessagingTimeout(root, 0.1)
    guard let windows = ax(root, "AXWindows") as? [AXUIElement], !windows.isEmpty, windows.count <= 16 else { throw UnlockUnavailable() }
    let names = [NSUserName(), NSFullUserName()]
    let deadline = ProcessInfo.processInfo.systemUptime + 2
    var targets = [UnlockTarget]()
    for window in windows where ax(window, "AXIdentifier") as? String == "login" {
        var nodes = [UnlockNode]()
        try walkUnlock(window, nil, 0, deadline, &nodes)
        let fields = nodes.filter { $0.id == "UserPasswordTextField" }
        let labels = nodes.filter { $0.id == "FocusedUser" }
        guard labels.count <= 1 else { throw UnlockUnavailable() }
        if let label = labels.first {
            guard label.role == "AXStaticText", let name = ax(label.element, "AXValue") as? String,
                  names.contains(name) else { throw UnlockUnavailable() }
        }
        if fields.isEmpty {
            if !labels.isEmpty { targets.append(UnlockTarget(pid: app.processIdentifier, window: window, field: nil, button: nil)) }
            continue
        }
        guard fields.count == 1, let field = fields.first,
              field.role == "AXTextField", field.subrole == "AXSecureTextField",
              (ax(field.element, "AXEnabled") as? Bool) == true else { throw UnlockUnavailable() }
        var settable = DarwinBoolean(false)
        guard AXUIElementIsAttributeSettable(field.element, "AXValue" as CFString, &settable) == .success,
              settable.boolValue else { throw UnlockUnavailable() }
        let buttons = nodes.filter { $0.id == "LUIBUTTON_GO" }
        guard buttons.count <= 1 else { throw UnlockUnavailable() }
        var button: AXUIElement?
        if let candidate = buttons.first {
            guard candidate.role == "AXButton", candidate.parent == field.parent else { throw UnlockUnavailable() }
            if (ax(candidate.element, "AXEnabled") as? Bool) == true { button = candidate.element }
        }
        targets.append(UnlockTarget(pid: app.processIdentifier, window: window, field: field.element, button: button))
    }
    let withField = targets.filter { $0.field != nil }
    if withField.count == 1 { return withField[0] }
    guard withField.isEmpty, targets.count == 1 else { throw UnlockUnavailable() }
    return targets[0]
}
private func sameUnlockTarget(_ a: UnlockTarget, _ b: UnlockTarget) -> Bool {
    guard a.pid == b.pid, CFEqual(a.window,b.window), let af = a.field, let bf = b.field else { return false }
    return CFEqual(af,bf)
}
private func revealUnlockTarget() throws -> UnlockTarget {
    var assertion: IOPMAssertionID = 0
    guard IOPMAssertionDeclareUserActivity("YiTerm manual unlock" as CFString, kIOPMUserActiveLocal, &assertion) == kIOReturnSuccess else { throw UnlockUnavailable() }
    defer { IOPMAssertionRelease(assertion) }
    let deadline = ProcessInfo.processInfo.systemUptime + 3
    var revealed = false
    while ProcessInfo.processInfo.systemUptime < deadline {
        if let target = try? unlockTarget() {
            if target.field != nil { return target }
            if !revealed {
                let next = try unlockTarget()
                guard target.pid == next.pid, CFEqual(target.window, next.window), sessionLockState() == 1 else { throw UnlockUnavailable() }
                postKey(36, []) // Reveal only; no secret is emitted as a key event.
                revealed = true
            }
        }
        Thread.sleep(forTimeInterval: 0.1)
    }
    throw UnlockUnavailable()
}
@_cdecl("yiterm_capture_unlock_ready")
public func captureUnlockReady() -> Int32 {
    let body = { () -> Int32 in
        if sessionLockState() == 0 { return 0 }
        return (try? revealUnlockTarget()) == nil ? -1 : 1
    }
    return Thread.isMainThread ? body() : DispatchQueue.main.sync(execute: body)
}
public typealias UnlockCancelled = @convention(c) () -> Int32
private var unlockAttempts = [TimeInterval]()
@_cdecl("yiterm_capture_unlock")
public func captureUnlock(_ bytes: UnsafePointer<UInt8>, _ length: Int, _ cancelled: UnlockCancelled?) -> Int32 {
    guard length > 0, length <= 4096, !UnsafeBufferPointer(start: bytes, count: length).contains(0),
          let password = String(bytes: UnsafeBufferPointer(start: bytes, count: length), encoding: .utf8) else { return -1 }
    let body = { () -> Int32 in
        func active() throws { if (cancelled?() ?? 0) != 0 { throw UnlockUnavailable() } }
        do {
            try active()
            if sessionLockState() == 0 { return 0 }
            guard AXIsProcessTrusted() else { return -1 }
            let now = ProcessInfo.processInfo.systemUptime
            unlockAttempts.removeAll { now-$0 >= 60 }
            guard unlockAttempts.count < 3, unlockAttempts.last.map({ now-$0 >= 5 }) ?? true else { return -2 }
            unlockAttempts.append(now)
            let original = try revealUnlockTarget()
            try active()
            let current = try unlockTarget()
            guard sameUnlockTarget(original,current), let field = current.field else { return -1 }
            defer {
                if sessionLockState() == 1, signedLoginwindow(current.pid),
                   ax(field,"AXIdentifier") as? String == "UserPasswordTextField",
                   ax(field,"AXSubrole") as? String == "AXSecureTextField" {
                    AXUIElementSetAttributeValue(field,"AXValue" as CFString,"" as CFString)
                }
            }
            try active()
            guard AXUIElementSetAttributeValue(field,"AXValue" as CFString,password as CFString) == .success else { return -1 }
            let deadline = ProcessInfo.processInfo.systemUptime + 2
            var ready: UnlockTarget?
            while ProcessInfo.processInfo.systemUptime < deadline {
                try active()
                let next = try unlockTarget()
                guard sameUnlockTarget(original,next) else { return -1 }
                if next.button != nil { ready = next; break }
                Thread.sleep(forTimeInterval: 0.05)
            }
            guard let ready, let button = ready.button else { return -1 }
            let final = try unlockTarget()
            guard sameUnlockTarget(original,final), let finalButton = final.button, CFEqual(button,finalButton) else { return -1 }
            try active()
            let status = AXUIElementPerformAction(finalButton,"AXPress" as CFString)
            guard status == .success || status == .cannotComplete else { return -1 }
            let completionDeadline = ProcessInfo.processInfo.systemUptime + 7
            while ProcessInfo.processInfo.systemUptime < completionDeadline {
                try active()
                if sessionLockState() == 0 { return 0 }
                Thread.sleep(forTimeInterval: 0.1)
            }
            return 1 // Rejected/timed out. Never submit the same password again.
        } catch { return -1 }
    }
    return Thread.isMainThread ? body() : DispatchQueue.main.sync(execute: body)
}
