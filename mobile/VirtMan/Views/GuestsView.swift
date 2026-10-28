import Foundation
import SwiftUI

struct GuestsView: View {
    let server: ServerRecord
    let connection: Connection
    @EnvironmentObject private var store: ServerStore

    @State private var vms: [Guest] = []
    @State private var containers: [Guest] = []
    @State private var error: String?
    @State private var loaded = false
    @State private var pendingStop: PendingStop?

    private struct PendingStop: Identifiable {
        let id = UUID()
        let guest: Guest
        let kind: GuestKind
    }

    private var client: APIClient? { store.client(for: server) }

    var body: some View {
        List {
            if let error {
                Section {
                    Text(error).foregroundStyle(.red)
                }
            }

            HostStatsLine(server: server, connection: connection)

            if !connection.isOnline {
                Section {
                    Text(connection.offlineError ?? "This daemon is offline.")
                        .foregroundStyle(.orange)
                }
            }

            Section("Virtual Machines") {
                if vms.isEmpty {
                    Text(loaded ? "No virtual machines are visible to this account." : "Loading…")
                        .foregroundStyle(.secondary)
                }
                ForEach(vms) { guest in
                    guestRow(guest, kind: .vm)
                }
            }

            Section("Containers") {
                if containers.isEmpty {
                    Text(loaded ? "No containers are visible to this account." : "Loading…")
                        .foregroundStyle(.secondary)
                }
                ForEach(containers) { guest in
                    guestRow(guest, kind: .container)
                }
            }
        }
        .navigationTitle(connection.name)
        .refreshable { await refresh() }
        .task(id: connection.id) { await poll() }
        .confirmationDialog(
            "Force stop \(pendingStop?.guest.label ?? "")?",
            isPresented: Binding(
                get: { pendingStop != nil },
                set: { if !$0 { pendingStop = nil } }
            ),
            presenting: pendingStop
        ) { stop in
            Button("Force Stop", role: .destructive) {
                perform(stop.guest, stop.kind, "destroy")
            }
            Button("Cancel", role: .cancel) { pendingStop = nil }
        } message: { _ in
            Text("This is like pulling the power plug; the guest may lose unsaved data.")
        }
    }

    @ViewBuilder
    private func guestRow(_ guest: Guest, kind: GuestKind) -> some View {
        NavigationLink {
            GuestDetailView(server: server, connection: connection, guest: guest, kind: kind)
        } label: {
            GuestRow(guest: guest)
        }
        .contextMenu {
            if !guest.running {
                Button {
                    perform(guest, kind, "start")
                } label: {
                    Label("Start", systemImage: "play.fill")
                }
            }
            if guest.running {
                Button {
                    perform(guest, kind, "shutdown")
                } label: {
                    Label("Shut Down", systemImage: "power")
                }
                Button {
                    perform(guest, kind, "reboot")
                } label: {
                    Label("Reboot", systemImage: "arrow.clockwise")
                }
                Button(role: .destructive) {
                    pendingStop = PendingStop(guest: guest, kind: kind)
                } label: {
                    Label("Force Stop", systemImage: "stop.fill")
                }
            }
        }
    }

    private func poll() async {
        await refresh()
        while !Task.isCancelled {
            try? await Task.sleep(nanoseconds: 3_000_000_000)
            if Task.isCancelled { return }
            if connection.isOnline { await refresh() }
        }
    }

    @MainActor
    private func refresh() async {
        guard let client else {
            error = "The API key for this server is missing. Delete it and add it again."
            return
        }
        guard connection.isOnline else { return }
        // Fetch independently: an account may be allowed to see VMs but not
        // containers, and one refusal must not hide the other list.
        var messages: [String] = []
        do {
            vms = try await client.guests(conn: connection.id, kind: .vm)
        } catch {
            messages.append("VMs: \(error.localizedDescription)")
        }
        do {
            containers = try await client.guests(conn: connection.id, kind: .container)
        } catch {
            messages.append("Containers: \(error.localizedDescription)")
        }
        self.error = messages.isEmpty ? nil : messages.joined(separator: "\n")
        loaded = true
    }

    private func perform(_ guest: Guest, _ kind: GuestKind, _ verb: String) {
        Task {
            do {
                try await client?.action(conn: connection.id, guest: guest, kind: kind, verb: verb)
                await refresh()
            } catch {
                await MainActor.run { self.error = error.localizedDescription }
            }
        }
    }
}

struct GuestRow: View {
    let guest: Guest

    private var color: Color {
        switch guest.state {
        case "running": return .green
        case "paused": return .orange
        case "stopped": return .secondary
        default: return .yellow
        }
    }

    var body: some View {
        HStack(spacing: 10) {
            Circle()
                .fill(color)
                .frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 2) {
                Text(guest.label)
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Text(guest.stateLabel)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private var detail: String {
        var parts: [String] = []
        if let vcpus = guest.vcpus, vcpus > 0 { parts.append("\(vcpus) vCPU") }
        if let memory = guest.memoryMb, memory > 0 { parts.append("\(memory) MiB") }
        if guest.privileged == true { parts.append("privileged") }
        if let owner = guest.ownerName, !owner.isEmpty { parts.append(owner) }
        return parts.isEmpty ? guest.name : parts.joined(separator: " · ")
    }
}

/// Host CPU and memory, sampled from the same counters the web UI uses.
struct HostStatsLine: View {
    let server: ServerRecord
    let connection: Connection
    @EnvironmentObject private var store: ServerStore

    @State private var line = "—"

    var body: some View {
        Section("Host") {
            Text(line)
                .font(.callout.monospacedDigit())
                .foregroundStyle(.secondary)
        }
        .task(id: connection.id) { await poll() }
    }

    private func poll() async {
        guard connection.isOnline, let client = store.client(for: server) else { return }
        var previous: HostStats?
        while !Task.isCancelled {
            if let sample = try? await client.hostStats(conn: connection.id) {
                var cpuText = "CPU —"
                if let total = sample.cpuTotal, let idle = sample.cpuIdle,
                   let previous, let totalBefore = previous.cpuTotal,
                   let idleBefore = previous.cpuIdle, total > totalBefore {
                    let used = ((total - totalBefore) - (idle - idleBefore)) / (total - totalBefore) * 100
                    cpuText = String(format: "CPU %.0f%%", max(0, min(100, used)))
                }
                var memoryText = "RAM —"
                if let total = sample.memTotalKib, let available = sample.memAvailableKib, total > 0 {
                    let used = (total - available) / 1_048_576
                    memoryText = String(format: "RAM %.1f / %.1f GiB", used, total / 1_048_576)
                }
                line = "\(cpuText)  ·  \(memoryText)"
                previous = sample
            }
            try? await Task.sleep(nanoseconds: 2_000_000_000)
        }
    }
}
