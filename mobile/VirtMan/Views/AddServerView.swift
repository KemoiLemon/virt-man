import SwiftUI

struct AddServerView: View {
    @EnvironmentObject private var store: ServerStore
    @Environment(\.dismiss) private var dismiss

    private enum AuthMode: String, CaseIterable, Identifiable {
        case password = "Password"
        case key = "API Key"
        var id: String { rawValue }
    }

    private struct Pending: Identifiable {
        let id = UUID()
        let url: URL
        let pin: String
    }

    @State private var name = ""
    @State private var address = ""
    @State private var mode: AuthMode = .password
    @State private var username = ""
    @State private var password = ""
    @State private var apiKey = ""
    @State private var busy = false
    @State private var error: String?
    @State private var pending: Pending?

    var body: some View {
        NavigationStack {
            Form {
                Section("Server") {
                    TextField("Name", text: $name)
                    TextField("https://host:8006", text: $address)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                }

                Section("Sign in") {
                    Picker("Sign in with", selection: $mode) {
                        ForEach(AuthMode.allCases) { mode in
                            Text(mode.rawValue).tag(mode)
                        }
                    }
                    .pickerStyle(.segmented)

                    if mode == .password {
                        TextField("Username", text: $username)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        SecureField("Password", text: $password)
                    } else {
                        SecureField("vm_…", text: $apiKey)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                    }
                }

                if let error {
                    Section {
                        Text(error)
                            .foregroundStyle(.red)
                    }
                }
            }
            .navigationTitle("Add Server")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if busy {
                        ProgressView()
                    } else {
                        Button("Add") { start() }
                            .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
                    }
                }
            }
            .disabled(busy)
            .alert(
                "Trust this certificate?",
                isPresented: Binding(
                    get: { pending != nil },
                    set: { if !$0 { pending = nil } }
                ),
                presenting: pending
            ) { request in
                Button("Trust and Add") {
                    Task { await finish(request) }
                }
                Button("Cancel", role: .cancel) { pending = nil }
            } message: { request in
                Text("SHA-256\n\(request.pin)\n\nCompare it with\nopenssl x509 -in /var/lib/virt-man/tls/cert.pem -fingerprint -sha256")
            }
        }
    }

    private func start() {
        guard let url = APIClient.normalize(address) else {
            error = "Enter a server address such as https://host:8006."
            return
        }
        busy = true
        error = nil
        Task {
            let result = await APIClient.probe(url: url)
            await MainActor.run {
                busy = false
                switch result {
                case let .success(pin):
                    pending = Pending(url: url, pin: pin)
                case let .failure(failure):
                    error = failure.localizedDescription
                }
            }
        }
    }

    @MainActor
    private func finish(_ request: Pending) async {
        pending = nil
        busy = true
        error = nil
        do {
            let key: String
            switch mode {
            case .key:
                let pasted = apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
                guard !pasted.isEmpty else {
                    throw APIError.transport("Paste an API key.")
                }
                let client = APIClient(base: request.url, pin: request.pin, key: pasted)
                _ = try await client.me()
                key = pasted
            case .password:
                let probe = APIClient(base: request.url, pin: request.pin, key: nil)
                let status = try? await probe.authStatus()
                if status?.authDisabled == true {
                    // A loopback/desktop instance answers without a credential.
                    key = ""
                } else {
                    if let status, status.needsSetup {
                        throw APIError.transport(
                            "This server has no account yet. Finish the first-run setup in a browser first.")
                    }
                    key = try await APIClient.mintKey(
                        base: request.url,
                        pin: request.pin,
                        username: username.trimmingCharacters(in: .whitespaces),
                        password: password
                    )
                }
            }

            let record = ServerRecord(
                id: UUID(),
                name: name.trimmingCharacters(in: .whitespaces).isEmpty
                    ? (request.url.host ?? "Server") : name.trimmingCharacters(in: .whitespaces),
                baseURL: request.url.absoluteString,
                pin: request.pin
            )
            store.add(record, key: key)
            dismiss()
        } catch {
            busy = false
            self.error = error.localizedDescription
        }
    }
}
