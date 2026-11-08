import Foundation

/// The identity of the server a bearer token may be sent to: scheme, host and
/// port, normalized so that URLs which reach the same server compare equal.
///
/// Only `http`/`https` with a host is a server origin; anything else returns
/// `nil`, which the token rules read as "do not send the token here".
public struct ServerOrigin: Equatable, Sendable {
    public let scheme: String
    public let host: String
    public let port: Int

    public init(scheme: String, host: String, port: Int) {
        self.scheme = scheme
        self.host = host
        self.port = port
    }

    /// The origin of a URL, or `nil` for anything but `http`/`https` with a
    /// host. The scheme and host are lowercased, the default port is made
    /// explicit, and IPv6 brackets are dropped so the stored host is canonical.
    public static func of(_ url: URL) -> ServerOrigin? {
        guard let scheme = url.scheme?.lowercased(),
              scheme == "http" || scheme == "https"
        else {
            return nil
        }
        guard let rawHost = url.host(percentEncoded: false), !rawHost.isEmpty else {
            return nil
        }
        var host = rawHost.lowercased()
        if host.hasPrefix("["), host.hasSuffix("]") {
            host.removeFirst()
            host.removeLast()
        }
        if host.isEmpty {
            return nil
        }
        let port = url.port ?? (scheme == "https" ? 443 : 80)
        return ServerOrigin(scheme: scheme, host: host, port: port)
    }

    /// The origin parsed back from its [`canonical`](canonical) string.
    public static func of(canonical: String) -> ServerOrigin? {
        guard let url = URL(string: canonical) else {
            return nil
        }
        return of(url)
    }

    /// A stable `scheme://host:port` string, with an IPv6 host bracketed so it
    /// round-trips through `URL` and [`of(canonical:)`](of(canonical:)).
    public var canonical: String {
        let shownHost = host.contains(":") ? "[\(host)]" : host
        return "\(scheme)://\(shownHost):\(port)"
    }

    /// Whether this origin is the local machine: `localhost`, a `*.localhost`
    /// name, canonical dotted-decimal `127.0.0.0/8`, or `::1`. A leading-zero
    /// octet (`0127.0.0.1`) or a trailing label (`127.0.0.1.evil.example`) is
    /// deliberately not loopback.
    public var isLoopback: Bool {
        if host == "localhost" || host.hasSuffix(".localhost") {
            return true
        }
        if host == "::1" {
            return true
        }
        let parts = host.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 4 else {
            return false
        }
        var octets: [Int] = []
        for part in parts {
            // Reject an empty label and a leading-zero label such as "0127";
            // a bare "0" is allowed.
            if part.isEmpty || (part.count > 1 && part.hasPrefix("0")) {
                return false
            }
            guard let value = Int(part), (0...255).contains(value) else {
                return false
            }
            octets.append(value)
        }
        return octets[0] == 127
    }
}
