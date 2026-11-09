import Foundation
import Testing
@testable import AIMemoryMenuCore

struct ServerOriginTests {
    private func origin(_ string: String) -> ServerOrigin? {
        URL(string: string).flatMap(ServerOrigin.of)
    }

    @Test func normalizesSchemeHostAndPort() throws {
        let a = origin("HTTP://Memory.Example.COM/status")
        #expect(a?.scheme == "http")
        #expect(a?.host == "memory.example.com")
        #expect(a?.port == 80)

        #expect(origin("https://memory.example.com")?.port == 443)
        #expect(origin("http://memory.example.com:8080")?.port == 8080)
        // The path, query and fragment do not change the origin.
        #expect(origin("http://host:49374/a?b#c") == origin("http://host:49374/x"))
    }

    @Test func rejectsAnythingButHttpWithAHost() throws {
        #expect(origin("ftp://host") == nil)
        #expect(origin("file:///etc/passwd") == nil)
        #expect(origin("mailto:boss@example.com") == nil)
        #expect(origin("http://") == nil)
    }

    @Test func canonicalRoundTrips() throws {
        for string in [
            "http://127.0.0.1:49374",
            "https://memory.example.com",
            "http://[::1]:49374",
            "http://host:8080",
        ] {
            let o = try #require(origin(string))
            #expect(ServerOrigin.of(canonical: o.canonical) == o)
        }
    }

    @Test func loopbackTable() throws {
        for yes in [
            "http://localhost:49374",
            "http://app.localhost:49374",
            "http://127.0.0.1:49374",
            "http://127.3.2.1:49374",
            "http://[::1]:49374",
        ] {
            #expect(origin(yes)?.isLoopback == true, "\(yes) is loopback")
        }
        for no in [
            "http://memory.example.com",
            "http://10.0.0.5:49374",
            "http://0127.0.0.1:49374", // leading-zero octet
            "http://127.0.0.1.evil.example:49374", // trailing label
            "http://notlocalhost:49374",
            "http://localhost.evil.example:49374",
        ] {
            #expect(origin(no)?.isLoopback == false, "\(no) is not loopback")
        }
    }
}
