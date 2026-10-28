import Foundation

enum APIError: LocalizedError {
    case http(Int, String)
    case transport(String)
    case untrusted(fingerprint: String?)
    case certificateChanged(fingerprint: String?)
    case badURL

    var errorDescription: String? {
        switch self {
        case let .http(code, message):
            if code == 401 { return "Credential rejected by the server." }
            return message.isEmpty ? "HTTP \(code)" : message
        case let .transport(message):
            return message
        case let .untrusted(fingerprint):
            if let fingerprint {
                return "The server certificate is not trusted yet. SHA-256: \(fingerprint)"
            }
            return "Could not read the server certificate."
        case let .certificateChanged(fingerprint):
            return "The server certificate changed. It no longer matches the pinned one"
                + (fingerprint.map { " (now \($0))" } ?? "") + "."
        case .badURL:
            return "That server address is not a URL."
        }
    }
}

/// Client for one `virt-man-web` origin. All daemon traffic goes through the
/// proxy paths (`/proxy/{conn}/...`); the app never dials a daemon.
final class APIClient: @unchecked Sendable {
    let base: URL
    let key: String?
    private let session: URLSession
    private let trust: PinnedTrustDelegate

    init(base: URL, pin: String?, key: String?) {
        self.base = base
        self.key = key
        let trust = PinnedTrustDelegate(pin: pin)
        self.trust = trust
        let config = URLSessionConfiguration.ephemeral
        config.timeoutIntervalForRequest = 15
        config.timeoutIntervalForResource = 30
        self.session = URLSession(configuration: config, delegate: trust, delegateQueue: nil)
    }

    deinit {
        session.invalidateAndCancel()
    }

    var observedFingerprint: String? { trust.observedFingerprint }

    // MARK: - Address handling

    /// `host`, `host:port` and a full URL all mean the origin. The web UI is
    /// HTTPS-only, so an entered http:// scheme is upgraded.
    static func normalize(_ input: String) -> URL? {
        var text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return nil }
        if !text.contains("://") { text = "https://" + text }
        guard
            var components = URLComponents(string: text),
            let host = components.host,
            !host.isEmpty
        else { return nil }
        components.scheme = "https"
        components.path = ""
        components.query = nil
        components.fragment = nil
        if components.port == nil { components.port = 8006 }
        return components.url
    }

    private static let segmentAllowed =
        CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-._~"))

    private static func segment(_ text: String) -> String {
        text.addingPercentEncoding(withAllowedCharacters: segmentAllowed) ?? text
    }

    func consoleURL(conn: String, guest: String) -> URL? {
        guard var components = URLComponents(url: base, resolvingAgainstBaseURL: false) else { return nil }
        components.scheme = base.scheme == "https" ? "wss" : "ws"
        components.path = "/proxy/\(Self.segment(conn))/domains/\(Self.segment(guest))/console/stream"
        return components.url
    }

    // MARK: - Transport

    private func decode<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        do {
            return try decoder.decode(type, from: data)
        } catch {
            throw APIError.transport("Malformed response from the server.")
        }
    }

    private func send(_ method: String, _ path: String, json: [String: Any]? = nil) async throws -> Data {
        guard let url = URL(string: path, relativeTo: base) else { throw APIError.badURL }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let key, !key.isEmpty {
            request.setValue("Bearer \(key)", forHTTPHeaderField: "Authorization")
        }
        if let json {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try? JSONSerialization.data(withJSONObject: json)
        }

        do {
            let (data, response) = try await session.data(for: request)
            guard let http = response as? HTTPURLResponse else {
                throw APIError.transport("No response from the server.")
            }
            guard (200..<300).contains(http.statusCode) else {
                if trust.certificateChanged {
                    throw APIError.certificateChanged(fingerprint: trust.observedFingerprint)
                }
                let body = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
                let message = body?["error"] as? String ?? ""
                throw APIError.http(http.statusCode, message)
            }
            return data
        } catch let error as APIError {
            throw error
        } catch let error as URLError {
            if trust.certificateChanged {
                throw APIError.certificateChanged(fingerprint: trust.observedFingerprint)
            }
            if error.code == .serverCertificateUntrusted
                || error.code == .secureConnectionFailed
                || error.code == .cancelled {
                throw APIError.untrusted(fingerprint: trust.observedFingerprint)
            }
            throw APIError.transport(error.localizedDescription)
        } catch {
            throw APIError.transport(error.localizedDescription)
        }
    }

    private func get<T: Decodable>(_ path: String) async throws -> T {
        let data = try await send("GET", path)
        return try decode(T.self, from: data)
    }

    private func post<T: Decodable>(_ path: String, json: [String: Any]? = nil) async throws -> T {
        let data = try await send("POST", path, json: json)
        return try decode(T.self, from: data)
    }

    // MARK: - Auth

    func health() async throws {
        _ = try await send("GET", "/health")
    }

    func authStatus() async throws -> AuthStatus {
        try await get("/auth/status")
    }

    func me() async throws -> Me {
        try await get("/auth/me")
    }

    func login(username: String, password: String) async throws -> Me {
        try await post("/auth/login", json: ["username": username, "password": password])
    }

    /// The plaintext key is returned exactly once, so save it immediately.
    func mintKey() async throws -> String {
        let response: ApiKeyResponse = try await post("/me/keys", json: ["name": "VirtMan iOS"])
        guard let key = response.key, !key.isEmpty else {
            throw APIError.transport("The server did not return an API key.")
        }
        return key
    }

    func logout() async {
        _ = try? await send("POST", "/auth/logout")
    }

    // MARK: - Resources

    func connections() async throws -> [Connection] {
        try await get("/connections")
    }

    func activity(limit: Int = 50) async throws -> [ActivityEntry] {
        let envelope: ActivityEnvelope = try await get("/activity?limit=\(limit)")
        return envelope.entries
    }

    func guests(conn: String, kind: GuestKind) async throws -> [Guest] {
        let list = kind == .vm ? "domains" : "containers"
        return try await get("/proxy/\(Self.segment(conn))/\(list)")
    }

    /// The live state only. The detail response is cheaper than re-reading the
    /// whole guest, and it keeps the list's owner annotation intact.
    func state(conn: String, guest: Guest, kind: GuestKind) async throws -> String {
        let list = kind == .vm ? "domains" : "containers"
        let response: GuestState = try await get(
            "/proxy/\(Self.segment(conn))/\(list)/\(Self.segment(guest.name))/state")
        return response.state
    }

    func hostStats(conn: String) async throws -> HostStats {
        try await get("/proxy/\(Self.segment(conn))/host/stats")
    }

    func guestStats(conn: String, guest: Guest, kind: GuestKind) async throws -> GuestStats {
        let list = kind == .vm ? "domains" : "containers"
        return try await get(
            "/proxy/\(Self.segment(conn))/\(list)/\(Self.segment(guest.name))/stats")
    }

    func graphics(conn: String, guest: Guest) async throws -> Graphics? {
        let envelope: GraphicsEnvelope = try await get(
            "/proxy/\(Self.segment(conn))/domains/\(Self.segment(guest.name))/graphics")
        return envelope.graphics
    }

    func action(conn: String, guest: Guest, kind: GuestKind, verb: String) async throws {
        let list = kind == .vm ? "domains" : "containers"
        let _: Empty = try await post(
            "/proxy/\(Self.segment(conn))/\(list)/\(Self.segment(guest.name))/\(verb)")
    }

    // MARK: - Trust probe

    /// Connect without a pin to learn the certificate fingerprint. The request
    /// itself fails (the delegate cancels the untrusted challenge); that is
    /// expected and the fingerprint is the result.
    static func probe(url: URL) async -> Result<String, APIError> {
        let client = APIClient(base: url, pin: nil, key: nil)
        do {
            try await client.health()
            // A trusted certificate never reaches our delegate, so there is no
            // fingerprint to show; the server is usable, but not pinnable.
            if let fingerprint = client.observedFingerprint {
                return .success(fingerprint)
            }
            return .failure(.transport("The server certificate was accepted without a fingerprint to pin."))
        } catch let error as APIError {
            if let fingerprint = client.observedFingerprint, !fingerprint.isEmpty {
                return .success(fingerprint)
            }
            return .failure(error)
        } catch {
            if let fingerprint = client.observedFingerprint, !fingerprint.isEmpty {
                return .success(fingerprint)
            }
            return .failure(.transport(error.localizedDescription))
        }
    }

    static func mintKey(
        base: URL, pin: String, username: String, password: String
    ) async throws -> String {
        let client = APIClient(base: base, pin: pin, key: nil)
        _ = try await client.login(username: username, password: password)
        let key = try await client.mintKey()
        await client.logout()
        return key
    }
}
