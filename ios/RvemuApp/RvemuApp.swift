// rvemu for iOS: an Alpine Linux RISC-V machine in your pocket.

import SwiftUI

@main
struct RvemuApp: App {
    init() {
        // The port the guest's `rssh` talks to; idle until the guest asks for something.
        Rssh.shared.start()
    }

    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}
