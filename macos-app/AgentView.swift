import Cocoa
import ImageIO

final class AgentViewPanel: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

/// Native pixels only: no browser, remote input handlers, clipboard or script bridge.
final class AgentViewController: NSObject, NSWindowDelegate {
    let model: AgentViewModel
    let panel: AgentViewPanel
    private let defaults: UserDefaults
    private let image = NSImageView()
    private let displays = NSPopUpButton(frame: .zero, pullsDown: false)
    private let status = NSTextField(wrappingLabelWithString: "Choose an owned virtual display.")
    private let age = NSTextField(labelWithString: "No verified frame")
    private let sizeButton = NSButton(title: "Expand", target: nil, action: nil)
    private var timer: Timer?
    private var observers: [NSObjectProtocol] = []
    private var renderedMonitors: [AgentViewMonitor] = []
    private var renderedAt: TimeInterval?
    private var fitting = false
    private var expanded = false

    init(source: AgentViewSource, defaults: UserDefaults = .standard) {
        self.model = AgentViewModel(source: source); self.defaults = defaults
        panel = AgentViewPanel(contentRect: NSRect(x: 40, y: 80, width: 440, height: 340),
            styleMask: [.titled, .closable, .resizable, .nonactivatingPanel], backing: .buffered, defer: false)
        super.init()
        panel.title = "Agent View · Read-only"
        panel.isReleasedWhenClosed = false
        panel.delegate = self
        panel.level = .floating
        panel.isFloatingPanel = true
        panel.hidesOnDeactivate = false
        panel.becomesKeyOnlyIfNeeded = true
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.minSize = NSSize(width: 320, height: 260)
        panel.appearance = NSAppearance(named: .darkAqua)
        panel.standardWindowButton(.closeButton)?.toolTip = "Hide preview — the agent and display keep running"
        panel.standardWindowButton(.zoomButton)?.isHidden = true
        let root = NSView()
        panel.contentView = root
        let refresh = NSButton(title: "Refresh list", target: self, action: #selector(reload))
        let hide = NSButton(title: "Hide", target: self, action: #selector(hideView))
        sizeButton.target = self; sizeButton.action = #selector(toggleSize)
        displays.target = self; displays.action = #selector(selectDisplay)
        displays.addItem(withTitle: "Choose a virtual display…")
        displays.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let top = NSStackView(views: [displays, refresh]); top.orientation = .horizontal; top.spacing = 8
        let bottom = NSStackView(views: [sizeButton, hide]); bottom.orientation = .horizontal; bottom.spacing = 8
        status.font = NSFont.systemFont(ofSize: 12)
        age.font = NSFont.monospacedDigitSystemFont(ofSize: 11, weight: .regular)
        age.textColor = .secondaryLabelColor
        image.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        image.setContentCompressionResistancePriority(.defaultLow, for: .vertical)
        image.setContentHuggingPriority(.defaultLow, for: .horizontal)
        image.setContentHuggingPriority(.defaultLow, for: .vertical)
        image.imageScaling = .scaleProportionallyUpOrDown
        image.imageAlignment = .alignCenter
        image.isEditable = false; image.allowsCutCopyPaste = false
        image.wantsLayer = true; image.layer?.backgroundColor = NSColor.black.cgColor
        image.setAccessibilityLabel("Read-only snapshot of the selected virtual display")
        let note = NSTextField(labelWithString: "Hide only closes this preview. Agent status is not linked.")
        note.font = NSFont.systemFont(ofSize: 10); note.textColor = .secondaryLabelColor
        note.lineBreakMode = .byTruncatingTail
        note.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        for view in [top, image, status, age, bottom, note] as [NSView] {
            view.translatesAutoresizingMaskIntoConstraints = false; root.addSubview(view)
        }
        NSLayoutConstraint.activate([
            top.topAnchor.constraint(equalTo: root.topAnchor, constant: 10),
            top.leadingAnchor.constraint(equalTo: root.leadingAnchor, constant: 12),
            top.trailingAnchor.constraint(equalTo: root.trailingAnchor, constant: -12),
            image.topAnchor.constraint(equalTo: top.bottomAnchor, constant: 8),
            image.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            image.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            image.heightAnchor.constraint(greaterThanOrEqualToConstant: 80),
            status.topAnchor.constraint(equalTo: image.bottomAnchor, constant: 8),
            status.leadingAnchor.constraint(equalTo: top.leadingAnchor),
            status.trailingAnchor.constraint(equalTo: top.trailingAnchor),
            age.topAnchor.constraint(equalTo: status.bottomAnchor, constant: 4),
            age.leadingAnchor.constraint(equalTo: top.leadingAnchor),
            age.trailingAnchor.constraint(lessThanOrEqualTo: bottom.leadingAnchor, constant: -4),
            bottom.topAnchor.constraint(equalTo: status.bottomAnchor, constant: 2),
            bottom.trailingAnchor.constraint(equalTo: top.trailingAnchor),
            note.leadingAnchor.constraint(equalTo: top.leadingAnchor),
            note.trailingAnchor.constraint(lessThanOrEqualTo: top.trailingAnchor),
            note.topAnchor.constraint(equalTo: bottom.bottomAnchor, constant: 4),
            note.bottomAnchor.constraint(equalTo: root.bottomAnchor, constant: -8)
        ])
        model.changed = { [weak self] in self?.render() }
        let center = NotificationCenter.default
        observers.append(center.addObserver(forName: NSApplication.didHideNotification, object: nil, queue: .main) { [weak self] _ in self?.hide() })
        // Hotplug/rearrangement is not permission to relocate/rebind the view automatically.
        observers.append(center.addObserver(forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main) { [weak self] _ in self?.hide() })
    }
    deinit { timer?.invalidate(); observers.forEach { NotificationCenter.default.removeObserver($0) } }
    func show() {
        precondition(Thread.isMainThread)
        guard let screen = Self.primaryScreen else { return }
        if !panel.isVisible {
            let fallback = NSRect(x: screen.visibleFrame.maxX - 464, y: screen.visibleFrame.minY + 24, width: 440, height: 340)
            let saved = defaults.string(forKey: "agentView.frame").map(NSRectFromString) ?? fallback
            panel.setFrame(Self.clamp(saved, to: screen.visibleFrame), display: false)
        }
        panel.orderFrontRegardless() // Deliberately not makeKeyAndOrderFront or activate.
        model.show()
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            guard let self = self else { return }
            self.age.stringValue = self.model.freshness; self.model.tick()
        }
        if let timer = timer { RunLoop.main.add(timer, forMode: .common) }
    }
    func hide() {
        precondition(Thread.isMainThread)
        if panel.isVisible { saveFrame() }
        timer?.invalidate(); timer = nil
        model.hide(); image.image = nil; renderedAt = nil
        panel.orderOut(nil)
    }
    func shutdown() {
        hide()
        (model.source as? AgentViewTransport)?.close()
    }
    private static var primaryScreen: NSScreen? {
        NSScreen.screens.first { ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value == CGMainDisplayID() }
    }
    static func clamp(_ input: NSRect, to screen: NSRect) -> NSRect {
        guard [input.minX, input.minY, input.width, input.height].allSatisfy({ $0.isFinite }), input.width > 0, input.height > 0 else {
            return NSRect(x: screen.minX + 12, y: screen.minY + 12, width: min(440, screen.width), height: min(340, screen.height))
        }
        let width = min(max(320, input.width), screen.width)
        let height = min(max(260, input.height), screen.height)
        return NSRect(x: min(max(screen.minX, input.minX), screen.maxX - width),
                      y: min(max(screen.minY, input.minY), screen.maxY - height), width: width, height: height)
    }
    private func saveFrame() { defaults.set(NSStringFromRect(panel.frame), forKey: "agentView.frame") }
    func windowShouldClose(_ sender: NSWindow) -> Bool { hide(); return false }
    func windowDidMove(_ notification: Notification) { constrainFrame() }
    func windowDidResize(_ notification: Notification) { constrainFrame() }
    private func constrainFrame() {
        guard !fitting, panel.isVisible, let screen = Self.primaryScreen else { return }
        fitting = true
        let clamped = Self.clamp(panel.frame, to: screen.visibleFrame)
        if clamped != panel.frame { panel.setFrame(clamped, display: true) }
        saveFrame(); fitting = false
    }
    @objc private func hideView() { hide() }
    @objc private func reload() { model.reload() }
    @objc private func selectDisplay() {
        let index = displays.indexOfSelectedItem - 1
        model.select(model.monitors.indices.contains(index) ? model.monitors[index] : nil)
    }
    @objc private func toggleSize() {
        expanded.toggle(); sizeButton.title = expanded ? "Compact" : "Expand"
        var frame = panel.frame; let size = expanded ? NSSize(width: 840, height: 620) : NSSize(width: 440, height: 340)
        frame.origin.y += frame.height - size.height; frame.size = size
        if let screen = Self.primaryScreen { frame = Self.clamp(frame, to: screen.visibleFrame) }
        panel.setFrame(frame, display: true)
    }
    private func render() {
        if renderedMonitors != model.monitors {
            renderedMonitors = model.monitors
            displays.removeAllItems(); displays.addItem(withTitle: "Choose a virtual display…")
            displays.addItems(withTitles: renderedMonitors.map(\.label))
        }
        displays.selectItem(at: model.selected.flatMap { renderedMonitors.firstIndex(of: $0) }.map { $0 + 1 } ?? 0)
        displays.isEnabled = !renderedMonitors.isEmpty
        status.stringValue = model.message; age.stringValue = model.freshness
        if let frame = model.frame, renderedAt != model.receivedAt {
            let options: [CFString: Any] = [kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceThumbnailMaxPixelSize: 1024, kCGImageSourceShouldCache: false]
            if let source = CGImageSourceCreateWithData(frame.png as CFData, nil), CGImageSourceGetCount(source) == 1,
               let thumbnail = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) {
                image.image = NSImage(cgImage: thumbnail, size: .zero)
                image.toolTip = "Capture reported at \(frame.capturedAt)"
            } else {
                image.image = nil; status.stringValue = "The PNG frame could not be decoded."
            }
            renderedAt = model.receivedAt
        } else if model.frame == nil { image.image = nil; renderedAt = nil }
    }
}
