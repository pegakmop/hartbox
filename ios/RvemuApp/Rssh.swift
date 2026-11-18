// rssh support for the app. The guest's `rssh` script talks to this over a control
// port that only exists on the phone's loopback (the guest reaches it as 10.0.2.2:2223):
//
//   expose on NAME   listen on 2222 on every interface (forwarded to the guest's sshd)
//                    and announce NAME on the network (Bonjour _rssh._tcp)
//   expose off       stop both
//   list             the devices announced on the network
//   background on    keep the app running with the screen off
//   background off|status, name, status
//
// Nothing here runs unless the guest asks, and none of it is on the emulation path.

import Foundation
import Network
#if os(iOS)
import CoreLocation
import UIKit
#endif

final class Rssh: NSObject {
    static let shared = Rssh()

    static let controlPort: UInt16 = 2223
    static let sshPort: UInt16 = 2222
    static let guestSshPort: UInt16 = 22
    static let serviceType = "_rssh._tcp."

    private let queue = DispatchQueue(label: "rvemu.rssh.control")
    private var listener: NWListener?
    private var service: NetService?
    private var exposedName: String?
    private var lister: Lister?
    #if os(iOS)
    private let keeper = BackgroundKeeper()
    #endif

    /// Start the control port. Safe to call more than once.
    func start() {
        guard listener == nil else { return }
        let params = NWParameters.tcp
        params.requiredInterfaceType = .loopback
        params.allowLocalEndpointReuse = true
        guard let port = NWEndpoint.Port(rawValue: Self.controlPort),
              let l = try? NWListener(using: params, on: port)
        else { return }
        l.newConnectionHandler = { [weak self] connection in self?.accept(connection) }
        l.start(queue: queue)
        listener = l
        #if os(iOS)
        DispatchQueue.main.async { self.keeper.resume() }
        #endif
    }

    // MARK: - Connections

    private func accept(_ c: NWConnection) {
        c.start(queue: queue)
        var received = Data()
        func receive() {
            c.receive(minimumIncompleteLength: 1, maximumLength: 256) { data, _, done, error in
                if let data = data { received.append(data) }
                let complete = received.contains(10) || received.count >= 256 || done || error != nil
                if complete {
                    let line = String(decoding: received.prefix { $0 != 10 }, as: UTF8.self)
                        .trimmingCharacters(in: .whitespacesAndNewlines)
                    DispatchQueue.main.async {
                        self.handle(line) { reply in
                            c.send(content: Data(reply.utf8), completion: .contentProcessed { _ in c.cancel() })
                        }
                    }
                } else {
                    receive()
                }
            }
        }
        receive()
        queue.asyncAfter(deadline: .now() + 5) { c.cancel() }
    }

    /// One request, one reply. Runs on the main thread (NetService needs a run loop).
    func handle(_ line: String, reply: @escaping (String) -> Void) {
        let words = line.split(separator: " ").map(String.init)
        switch (words.first ?? "", words.dropFirst().first ?? "") {
        case ("name", _):
            reply("ok name=\(Self.deviceName())\n")
        case ("status", _):
            reply("ok expose=\(exposedName != nil ? "on" : "off") name=\(exposedName ?? Self.deviceName()) background=\(backgroundState())\n")
        case ("expose", "on"):
            let wanted = words.count > 2 ? Self.clean(words[2]) : ""
            reply(expose(name: wanted.isEmpty ? Self.deviceName() : wanted))
        case ("expose", "off"):
            unexpose()
            reply("ok\n")
        case ("list", _):
            list(reply)
        case ("background", let mode):
            reply(background(mode))
        default:
            reply("err unknown command\n")
        }
    }

    // MARK: - expose

    private func expose(name: String) -> String {
        if rvemu_ios_expose(Self.sshPort, Self.guestSshPort) != 0 {
            return "err cannot listen on port \(Self.sshPort) (in use?)\n"
        }
        service?.stop()
        let s = NetService(domain: "local.", type: Self.serviceType, name: name, port: Int32(Self.sshPort))
        s.setTXTRecord(NetService.data(fromTXTRecord: ["user": Data("root".utf8), "ver": Data("1".utf8)]))
        s.publish()
        service = s
        exposedName = name
        return "ok name=\(name) port=\(Self.sshPort)\n"
    }

    private func unexpose() {
        rvemu_ios_unexpose()
        service?.stop()
        service = nil
        exposedName = nil
    }

    // MARK: - list

    private func list(_ reply: @escaping (String) -> Void) {
        let l = Lister(skip: exposedName)
        lister = l
        l.run { [weak self] lines in
            self?.lister = nil
            reply(lines.map { "dev\t\($0)\n" }.joined() + "ok \(lines.count)\n")
        }
    }

    // MARK: - background

    private func backgroundState() -> String {
        #if os(iOS)
        return keeper.wanted ? "on" : "off"
        #else
        return "off"
        #endif
    }

    private func background(_ mode: String) -> String {
        #if os(iOS)
        switch mode {
        case "on": return keeper.set(true)
        case "off": return keeper.set(false)
        case "status": return "ok background=\(keeper.wanted ? "on" : "off") \(keeper.detail())\n"
        default: return "err usage: background on|off|status\n"
        }
        #else
        return "err background mode needs iOS\n"
        #endif
    }

    // MARK: - names

    /// Lower case letters, digits and dashes, at most 30 characters.
    static func clean(_ s: String) -> String {
        var out = ""
        for ch in s.lowercased() {
            if ch.isASCII, ch.isLetter || ch.isNumber { out.append(ch) } else if !out.hasSuffix("-") { out.append("-") }
        }
        return String(out.trimmingCharacters(in: CharacterSet(charactersIn: "-")).prefix(30))
    }

    /// The phone's name on the network ("lols-iPhone"), else the model with a short id.
    static func deviceName() -> String {
        var host = ProcessInfo.processInfo.hostName
        if host.hasSuffix(".local") { host = String(host.dropLast(6)) }
        let name = clean(host)
        if !name.isEmpty, name != "localhost" { return name }
        #if os(iOS)
        let id = UIDevice.current.identifierForVendor?.uuidString.prefix(4).lowercased() ?? "0000"
        return clean(UIDevice.current.model) + "-" + id
        #else
        return "device"
        #endif
    }
}

/// Browse the network for rssh devices and resolve each to an IPv4 address (the guest
/// cannot resolve .local names).
private final class Lister: NSObject, NetServiceBrowserDelegate, NetServiceDelegate {
    private let browser = NetServiceBrowser()
    private let skip: String?
    private var services: [NetService] = []
    private var lines: [String] = []
    private var pending = 0
    private var finished = false
    private var completion: (([String]) -> Void)?

    init(skip: String?) { self.skip = skip }

    func run(_ done: @escaping ([String]) -> Void) {
        completion = done
        browser.delegate = self
        browser.searchForServices(ofType: Rssh.serviceType, inDomain: "local.")
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) { self.resolveAll() }
        DispatchQueue.main.asyncAfter(deadline: .now() + 4.0) { self.finish() }
    }

    func netServiceBrowser(_ b: NetServiceBrowser, didFind s: NetService, moreComing: Bool) {
        services.append(s)
    }

    private func resolveAll() {
        browser.stop()
        let todo = services.filter { Rssh.clean($0.name) != skip }
        pending = todo.count
        if todo.isEmpty { finish(); return }
        for s in todo {
            s.delegate = self
            s.resolve(withTimeout: 2.5)
        }
    }

    func netServiceDidResolveAddress(_ s: NetService) {
        var address = ""
        for data in s.addresses ?? [] {
            data.withUnsafeBytes { raw in
                guard raw.count >= MemoryLayout<sockaddr_in>.size else { return }
                let sa = raw.bindMemory(to: sockaddr_in.self).baseAddress!.pointee
                guard sa.sin_family == sa_family_t(AF_INET) else { return }
                var addr = sa.sin_addr
                var buf = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
                if inet_ntop(AF_INET, &addr, &buf, socklen_t(INET_ADDRSTRLEN)) != nil, address.isEmpty {
                    address = String(cString: buf)
                }
            }
        }
        var user = ""
        if let txt = s.txtRecordData() {
            let dict = NetService.dictionary(fromTXTRecord: txt)
            if let u = dict["user"] { user = String(decoding: u, as: UTF8.self) }
        }
        if !address.isEmpty {
            lines.append("\(Rssh.clean(s.name))\t\(address)\t\(s.port)\t\(user)")
        }
        resolved()
    }

    func netService(_ s: NetService, didNotResolve errorDict: [String: NSNumber]) { resolved() }

    private func resolved() {
        pending -= 1
        if pending <= 0 { finish() }
    }

    private func finish() {
        guard !finished else { return }
        finished = true
        browser.stop()
        completion?(lines.sorted())
        completion = nil
    }
}

#if os(iOS)
/// Keeps the app running with the screen off by using the lowest-power background location
/// mode (a 3 km accuracy that uses no GPS). iOS shows its blue location indicator while on.
private final class BackgroundKeeper: NSObject, CLLocationManagerDelegate {
    private let manager = CLLocationManager()
    private let key = "rssh.background"
    private(set) var wanted: Bool

    override init() {
        wanted = UserDefaults.standard.bool(forKey: key)
        super.init()
        manager.delegate = self
    }

    /// Pick up the saved setting at launch.
    func resume() {
        if wanted { apply() }
    }

    func set(_ on: Bool) -> String {
        wanted = on
        UserDefaults.standard.set(on, forKey: key)
        if !on {
            apply()
            return "ok background=off\n"
        }
        switch manager.authorizationStatus {
        case .denied, .restricted:
            return "err location access is off: Settings > Hartbox > Location > While Using the App\n"
        case .notDetermined:
            apply()
            return "ok background=on (allow location access on the phone; it is only used to keep the app running)\n"
        default:
            apply()
            return "ok background=on (the blue location indicator shows while it is on)\n"
        }
    }

    func detail() -> String {
        switch manager.authorizationStatus {
        case .notDetermined: return "(location access not asked yet)"
        case .denied, .restricted: return "(location access denied)"
        default: return "(location access granted)"
        }
    }

    private func apply() {
        if wanted {
            if manager.authorizationStatus == .notDetermined {
                manager.requestWhenInUseAuthorization()
                return
            }
            guard manager.authorizationStatus != .denied, manager.authorizationStatus != .restricted else { return }
            manager.desiredAccuracy = kCLLocationAccuracyThreeKilometers
            manager.distanceFilter = 3000
            manager.pausesLocationUpdatesAutomatically = false
            manager.allowsBackgroundLocationUpdates = true
            manager.showsBackgroundLocationIndicator = true
            manager.startUpdatingLocation()
        } else {
            manager.stopUpdatingLocation()
            manager.allowsBackgroundLocationUpdates = false
        }
    }

    func locationManagerDidChangeAuthorization(_ manager: CLLocationManager) {
        if wanted { apply() }
    }

    func locationManager(_ manager: CLLocationManager, didUpdateLocations locations: [CLLocation]) {}
    func locationManager(_ manager: CLLocationManager, didFailWithError error: Error) {}
}
#endif
