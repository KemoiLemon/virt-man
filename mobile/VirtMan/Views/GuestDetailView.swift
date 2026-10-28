import Foundation
import SwiftUI

struct GuestDetailView: View {
    let server: ServerRecord
    let connection: Connection
    let kind: GuestKind
    @EnvironmentObject private var store: ServerStore

    @State private var guest: Guest
    @State private var stats: GuestStats?
    @State private var previous: GuestStats?
    @State private var cpuPercent: Double?
    @State private var graphics: Graphics?
    @State private var graphicsLoaded = false
    @State private var error: String?
    @State private var confirmingStop = false

    init(server: ServerRecord, connection: Connection, guest: Guest, kind: GuestKind) {
        self.server = server
        self.connection = connection
        self.kind = kind
        _guest = State(initialValue: guest)
    }

    private var client: APIClient? { store.client(for: server) }

    var body: some View {
        List {
            if let error {
                Section {
                    Text(error).foregroundStyle(.red)
                }
            }

            Section("State") {
                LabeledContent("State", value: guest.stateLabel)
                if let vcpus = guest.vcpus { LabeledContent("vCPUs", value: "\(vcpus)") }
                if let memory = guest.memoryMb { LabeledContent("Memory", value: "\(memory) MiB") }
                if let owner = guest.ownerName, !owner.isEmpty {
                    LabeledContent("Owner", value: owner)
                }
                if guest.suspended == true {
                    LabeledContent("Suspended", value: "read-only")
                }
            }

            Section("Usage") {
                LabeledContent("CPU", value: cpuPercent.map { String(format: "%.0f%%", $0) } ?? "—")
                LabeledContent("Memory", value: memoryText)
            }

            Section("Power") {
                if guest.running {
                    Button("Shut Down") { perform("shutdown") }
                    Button("Reboot") { perform("reboot") }
                    Button("Force Stop", role: .destructive) { confirmingStop = true }
                } else {
                    Button("Start") { perform("start") }
                }
            }

            Section("Console") {
                if kind == .container {
                    Text("Container text consoles are not supported in this version.")
                        .foregroundStyle(.secondary)
                } else if !guest.running {
                    Text("Start the guest to open its console.")
                        .foregroundStyle(.secondary)
                } else if let graphics, graphics.kind == "vnc", let client,
                          let key = store.key(for: server) {
                    NavigationLink {
                        ConsoleScreen(
                            connection: connection,
                            guest: guest,
                            client: client,
                            pin: server.pin,
                            key: key
                        )
                    } label: {
                        Label("Open Console", systemImage: "display")
                    }
                } else if !graphicsLoaded {
                    Text("Checking the display…").foregroundStyle(.secondary)
                } else if let graphics {
                    Text("\(graphics.kind.uppercased()) consoles open in virt-viewer.")
                        .foregroundStyle(.secondary)
                } else {
                    Text("This guest has no display.")
                        .foregroundStyle(.secondary)
                }
            }
        }
        .navigationTitle(guest.label)
        .navigationBarTitleDisplayMode(.inline)
        .refreshable { await load() }
        .task(id: guest.id) { await poll() }
        .confirmationDialog(
            "Force stop \(guest.label)?",
            isPresented: $confirmingStop
        ) {
            Button("Force Stop", role: .destructive) { perform("destroy") }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This is like pulling the power plug; the guest may lose unsaved data.")
        }
    }

    private var memoryText: String {
        if let kib = stats?.memoryKib, kib > 0 {
            return String(format: "%.0f MiB", kib / 1024)
        }
        if let max = stats?.maxMemoryKib, max > 0 {
            return String(format: "%.0f MiB", max / 1024)
        }
        return "—"
    }

    private func poll() async {
        await load()
        while !Task.isCancelled {
            try? await Task.sleep(nanoseconds: 2_000_000_000)
            if Task.isCancelled { return }
            await load()
        }
    }

    @MainActor
    private func load() async {
        guard let client, connection.isOnline else { return }
        do {
            let state = try await client.state(conn: connection.id, guest: guest, kind: kind)
            if state != guest.state { guest.state = state }
            let sample = try await client.guestStats(conn: connection.id, guest: guest, kind: kind)
            apply(sample)
            if kind == .vm, !graphicsLoaded {
                graphics = try await client.graphics(conn: connection.id, guest: guest)
                graphicsLoaded = true
            }
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func apply(_ sample: GuestStats) {
        if let previous,
           let cpuTime = sample.cpuTimeNs,
           let cpuTimeBefore = previous.cpuTimeNs,
           let sampledAt = sample.sampledAtMs,
           let sampledBefore = previous.sampledAtMs,
           sampledAt > sampledBefore {
            let vcpus = Double(max(sample.vcpus ?? 1, 1))
            let ratio = (cpuTime - cpuTimeBefore) / ((sampledAt - sampledBefore) * 1_000_000)
            cpuPercent = min(100, max(0, ratio / vcpus * 100))
        }
        previous = sample
        stats = sample
    }

    private func perform(_ verb: String) {
        guard let client else { return }
        Task {
            do {
                try await client.action(
                    conn: connection.id, guest: guest, kind: kind, verb: verb)
                await load()
            } catch {
                await MainActor.run { self.error = error.localizedDescription }
            }
        }
    }
}
