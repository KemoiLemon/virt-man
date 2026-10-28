import CryptoKit
import Foundation
import Security

/// Trust-on-first-use for the web server's self-signed certificate.
///
/// The browser asks the user to click through a warning; an app cannot. So the
/// first probe records the leaf certificate's SHA-256, the user confirms that
/// fingerprint out of band, and every later request accepts only that exact
/// certificate. A changed certificate is a hard failure, not a warning.
final class PinnedTrustDelegate: NSObject, URLSessionDelegate, URLSessionTaskDelegate, @unchecked Sendable {
    private let lock = NSLock()
    private let pin: String?
    private var _observed: String?
    private var _changed = false

    init(pin: String?) {
        self.pin = pin
    }

    var observedFingerprint: String? {
        lock.lock()
        defer { lock.unlock() }
        return _observed
    }

    var certificateChanged: Bool {
        lock.lock()
        defer { lock.unlock() }
        return _changed
    }

    func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        guard
            challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
            let trust = challenge.protectionSpace.serverTrust
        else {
            completionHandler(.performDefaultHandling, nil)
            return
        }

        let fingerprint = Self.fingerprint(of: trust)
        lock.lock()
        _observed = fingerprint
        let matches = pin != nil && pin == fingerprint
        if pin != nil && !matches { _changed = true }
        lock.unlock()

        if matches {
            completionHandler(.useCredential, URLCredential(trust: trust))
        } else {
            completionHandler(.cancelAuthenticationChallenge, nil)
        }
    }

    /// The API never redirects (HTTP is upgraded before the app is reached, and
    /// the app only speaks https). A redirect would leave the pinned host.
    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        completionHandler(nil)
    }

    static func fingerprint(of trust: SecTrust) -> String? {
        guard
            let chain = SecTrustCopyCertificateChain(trust) as? [SecCertificate],
            let leaf = chain.first
        else { return nil }
        let der = SecCertificateCopyData(leaf) as Data
        return SHA256.hash(data: der).map { String(format: "%02X", $0) }.joined(separator: ":")
    }
}
