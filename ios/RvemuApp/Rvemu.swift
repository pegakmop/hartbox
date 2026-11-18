// Bridge to the Rust emulator (rvemu-ios static library) and app state.

import Combine
import Foundation
import UIKit

final class Console: ObservableObject {
    enum Phase: Equatable {
        case preparing
        case running
        case halted(Int32)
        case failed(String)
    }

    /// Console geometry. The guest tty is set to the same size by the
    /// emulator, so line wrapping matches the screen.
    static let cols = 64
    static let rows = 30
    private static let ramMB: UInt32 = 512
    /// Kernel modules copied into the initramfs overlay: virtio transport, the
    /// entropy source, and virtio-net.
    private static let modules = [
        "virtio_ring", "virtio", "virtio_mmio", "virtio-rng",
        "failover", "net_failover", "virtio_net",
    ]

    @Published private(set) var phase: Phase = .preparing
    @Published private(set) var mips: Double = 0
    @Published private(set) var bootedAt: Double?

    let terminal = Terminal(cols: Console.cols, rows: Console.rows)

    private var poll: Timer?
    private var mipsTimer: Timer?
    private var startTime: Date?
    private var lastInsns: UInt64 = 0
    private var lastIdle: UInt64 = 0
    private var lastInsnsTime: Date = Date()
    private var sawOutput = false
    private var started = false
    private var lifecycleObserver: NSObjectProtocol?
    private var backgroundTask: UIBackgroundTaskIdentifier = .invalid
    /// Set once the guest has booted through the full initramfs, so the root
    /// disk is known to be populated.
    private var usedMinimal = false
    private var bootDeadline: Date?
    private var retryWithFullRootfs = false

    // MARK: - Lifecycle

    func boot() {
        guard !started else { return }
        started = true
        watchLifecycle()
        phase = .preparing
        mips = 0
        bootedAt = nil
        sawOutput = false
        terminal.responder = { [weak self] bytes in
            self?.write(bytes)
        }

        guard let kernel = Self.resource(named: "vmlinuz-lts", extension: nil),
              let rootfs = Self.resource(named: "alpine-minirootfs-3.24.2-riscv64", extension: "tar.gz")
        else {
            phase = .failed("kernel or rootfs missing from the app bundle")
            started = false
            return
        }
        guard let overlay = Self.stageOverlay() else {
            phase = .failed("could not stage the kernel modules")
            started = false
            return
        }

        startTime = Date()
        let disk = Self.rootDiskPath()
        // Boot straight from the disk once it is known to be populated; the
        // initramfs then carries no root filesystem and the kernel skips the
        // unpack. The watchdog below falls back if that ever fails.
        let diskExists = FileManager.default.fileExists(atPath: disk)
        if !diskExists { Self.clearDiskReady() }
        usedMinimal = diskExists && Self.diskReady
        bootDeadline = usedMinimal ? Date().addingTimeInterval(20) : nil
        let result: Int32 = kernel.withUnsafeBytes { kbuf in
            rootfs.withUnsafeBytes { rbuf in
                overlay.withCString { overlayPath in
                    disk.withCString { diskPath in
                        rvemu_ios_start(
                            kbuf.bindMemory(to: UInt8.self).baseAddress,
                            kernel.count,
                            rbuf.bindMemory(to: UInt8.self).baseAddress,
                            rootfs.count,
                            overlayPath,
                            diskPath,
                            usedMinimal ? 1 : 0,
                            Self.ramMB)
                    }
                }
            }
        }
        guard result == 0 else {
            let message = lastError() ?? "start failed (\(result))"
            phase = .failed(message)
            started = false
            return
        }
        phase = .running
        startTimers()
    }

    /// iOS suspends the app soon after it leaves the screen and may kill it later
    /// without a word. The guest sends its pending writes to the disk image once a
    /// second and the host writes its cache back once a second, so asking for a few
    /// seconds of background time gets everything onto the image before that.
    private func watchLifecycle() {
        guard lifecycleObserver == nil else { return }
        lifecycleObserver = NotificationCenter.default.addObserver(
            forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: .main
        ) { [weak self] _ in self?.holdForWriteback() }
    }

    private func holdForWriteback() {
        guard backgroundTask == .invalid else { return }
        backgroundTask = UIApplication.shared.beginBackgroundTask(withName: "rvemu-writeback") { [weak self] in
            self?.endHold()
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 4) { [weak self] in self?.endHold() }
    }

    private func endHold() {
        guard backgroundTask != .invalid else { return }
        UIApplication.shared.endBackgroundTask(backgroundTask)
        backgroundTask = .invalid
    }

    /// Ask the guest to shut down (the boot session's shell runs as PID 1's
    /// session leader, so `exit` powers the machine off).
    func shutdown() {
        guard started, case .running = phase else { return }
        write(Data("exit\n".utf8))
    }

    func write(_ data: Data) {
        guard !data.isEmpty else { return }
        data.withUnsafeBytes { buf in
            rvemu_ios_write(buf.bindMemory(to: UInt8.self).baseAddress, data.count)
        }
    }

    // MARK: - Polling

    private func startTimers() {
        poll?.invalidate()
        poll = Timer.scheduledTimer(withTimeInterval: 1.0 / 60.0, repeats: true) { [weak self] _ in
            self?.pumpOutput()
        }
        RunLoop.main.add(poll!, forMode: .common)

        mipsTimer?.invalidate()
        mipsTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            self?.updateMips()
        }
        RunLoop.main.add(mipsTimer!, forMode: .common)
    }

    private func pumpOutput() {
        var buffer = [UInt8](repeating: 0, count: 1 << 16)
        while true {
            let n = buffer.withUnsafeMutableBytes { raw in
                rvemu_ios_read(raw.bindMemory(to: UInt8.self).baseAddress, raw.count)
            }
            if n == 0 { break }
            if !sawOutput {
                sawOutput = true
                bootDeadline = nil
                bootedAt = Date().timeIntervalSince(startTime ?? Date())
                if !usedMinimal { Self.markDiskReady() }
                // The shell is up: bring the keyboard up with it.
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) {
                    NotificationCenter.default.post(name: .rvemuFocusKeyboard, object: nil)
                }
            }
            terminal.feed(Data(buffer[0..<n]))
        }

        // A minimal-initramfs boot that produces nothing is a broken disk:
        // stop it, forget the marker and come up the slow, safe way.
        if let deadline = bootDeadline, !sawOutput, Date() > deadline {
            bootDeadline = nil
            Self.clearDiskReady()
            retryWithFullRootfs = true
            rvemu_ios_stop()
            return
        }

        if rvemu_ios_running() == 0 {
            let code = rvemu_ios_exit_code()
            poll?.invalidate(); poll = nil
            mipsTimer?.invalidate(); mipsTimer = nil
            if case .failed = phase {} else {
                if let message = lastError() {
                    phase = .failed(message)
                } else {
                    phase = .halted(code)
                }
                started = false
            }
        }
    }

    private func updateMips() {
        let now = Date()
        let insns = rvemu_ios_insns()
        let idle = rvemu_ios_idle_ns()
        let dt = now.timeIntervalSince(lastInsnsTime)
        // Time the guest spent idle is not interpreter time: count the rate
        // over the part of the interval the machine was actually running.
        let idleSecs = Double(idle &- lastIdle) / 1_000_000_000
        let busy = max(dt - idleSecs, 0.02)
        if dt > 0.2, insns >= lastInsns {
            let rate = Double(insns - lastInsns) / busy / 1_000_000
            mips = mips == 0 ? rate : mips * 0.6 + rate * 0.4
        }
        lastInsns = insns
        lastIdle = idle
        lastInsnsTime = now
    }

    // MARK: - Helpers

    private func lastError() -> String? {
        var buffer = [UInt8](repeating: 0, count: 512)
        let n = buffer.withUnsafeMutableBytes { raw in
            rvemu_ios_last_error(raw.bindMemory(to: UInt8.self).baseAddress, raw.count)
        }
        guard n > 0 else { return nil }
        return String(decoding: buffer[0..<n], as: UTF8.self)
    }

    private static var documents: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    }

    /// The guest's root filesystem lives here, so files survive a reboot.
    private static func rootDiskPath() -> String {
        documents.appendingPathComponent("rvemu-root.img").path
    }

    /// Marker written once the disk holds a complete root filesystem.
    private static var diskReadyMarker: URL {
        documents.appendingPathComponent("root-disk-ready")
    }

    private static var diskReady: Bool {
        FileManager.default.fileExists(atPath: diskReadyMarker.path)
    }

    private static func markDiskReady() {
        FileManager.default.createFile(atPath: diskReadyMarker.path, contents: Data())
    }

    private static func clearDiskReady() {
        try? FileManager.default.removeItem(at: diskReadyMarker)
    }

    private static func resource(named name: String, extension ext: String?) -> Data? {
        if let url = Bundle.main.url(forResource: name, withExtension: ext) {
            return try? Data(contentsOf: url, options: .mappedIfSafe)
        }
        return nil
    }

    /// Write the bundled kernel modules into an overlay directory and return its
    /// path, or nil when the bundle has no modules.
    private static func stageOverlay() -> String? {
        let fm = FileManager.default
        let root = fm.temporaryDirectory.appendingPathComponent("rvemu-overlay", isDirectory: true)
        let modules = root.appendingPathComponent("lib/rvemu", isDirectory: true)
        try? fm.removeItem(at: root)
        do {
            try fm.createDirectory(at: modules, withIntermediateDirectories: true)
        } catch {
            return nil
        }
        if let bundled = Bundle.main.url(forResource: "Modules", withExtension: nil) {
            let files = (try? fm.contentsOfDirectory(at: bundled, includingPropertiesForKeys: nil)) ?? []
            var copied = 0
            for file in files where file.pathExtension == "ko" {
                let target = modules.appendingPathComponent(file.lastPathComponent)
                try? fm.removeItem(at: target)
                if (try? fm.copyItem(at: file, to: target)) != nil { copied += 1 }
            }
            if copied == 0 { return nil }
        } else {
            return nil
        }
        return root.path
    }
}
