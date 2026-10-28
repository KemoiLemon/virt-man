import SwiftUI

@main
struct VirtManApp: App {
    @StateObject private var store = ServerStore()

    var body: some Scene {
        WindowGroup {
            ServersView()
                .environmentObject(store)
                .tint(Color(red: 0.996, green: 0.396, blue: 0.396))
        }
    }
}
