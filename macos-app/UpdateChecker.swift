import Foundation

/// Manual-update checker against the GitHub releases of this repo.
///
/// Deliberately minimal and honest: it compares the bundled
/// `CFBundleShortVersionString` with the latest published release tag, and
/// the strongest action it ever takes is opening the release page in the
/// default browser. No auto-download, no auto-install, no background timers —
/// one silent check at launch (release builds only) plus the explicit
/// "Check for Updates…" menu item.
enum UpdateChecker {
    static let repoSlug = "intendant-dev/Intendant"

    static var releasesPageURL: URL {
        URL(string: "https://github.com/\(repoSlug)/releases/latest")!
    }

    private static var latestReleaseAPI: URL {
        URL(string: "https://api.github.com/repos/\(repoSlug)/releases/latest")!
    }

    struct Release {
        let tag: String
        let pageURL: URL
    }

    /// Version stamped into Info.plist by scripts/bundle-macos.sh (the tag on
    /// release builds, a `git describe` derivative on dev builds).
    static func bundledVersion() -> String {
        (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String)
            ?? "0.0.0-dev"
    }

    /// Release builds are stamped with plain dotted numerics from the tag;
    /// dev builds carry a suffix ("0.0.0-1a2b3c4d", "1.2.0-4-g1a2b3c4d-dirty").
    /// Dev builds skip the launch-time check — a local bundle nagging about
    /// the latest release is noise, not signal.
    static func isDevBuild(_ version: String) -> Bool {
        version.isEmpty || version.contains("-")
    }

    /// Compares dotted numeric prefixes ("v1.2.3-whatever" → [1,2,3]);
    /// suffixes are ignored, and unparseable versions are never "newer"
    /// (an update prompt must not fire on garbage input).
    static func isNewer(remote: String, than local: String) -> Bool {
        func numericPrefix(_ version: String) -> [Int]? {
            var core = version
            if core.hasPrefix("v") || core.hasPrefix("V") {
                core.removeFirst()
            }
            core = core.split(separator: "-").first.map(String.init) ?? core
            let rawParts = core.split(separator: ".").map { Int($0) }
            guard !rawParts.isEmpty, !rawParts.contains(nil) else { return nil }
            return rawParts.compactMap { $0 }
        }
        guard let remoteParts = numericPrefix(remote),
              let localParts = numericPrefix(local) else { return false }
        for i in 0..<max(remoteParts.count, localParts.count) {
            let r = i < remoteParts.count ? remoteParts[i] : 0
            let l = i < localParts.count ? localParts[i] : 0
            if r != l { return r > l }
        }
        return false
    }

    /// Fetch the latest published release. Completion runs on the main queue;
    /// `nil` means "couldn't determine" (offline, rate-limited, no releases
    /// published yet, unexpected payload) — callers decide whether that is
    /// silence (launch check) or an alert (explicit menu action).
    static func fetchLatestRelease(completion: @escaping (Release?) -> Void) {
        var request = URLRequest(
            url: latestReleaseAPI,
            cachePolicy: .reloadIgnoringLocalCacheData,
            timeoutInterval: 15
        )
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        URLSession.shared.dataTask(with: request) { data, response, _ in
            var release: Release?
            if let http = response as? HTTPURLResponse, http.statusCode == 200,
               let data = data,
               let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
               let tag = object["tag_name"] as? String, !tag.isEmpty {
                let page = (object["html_url"] as? String).flatMap { URL(string: $0) }
                    ?? releasesPageURL
                release = Release(tag: tag, pageURL: page)
            }
            DispatchQueue.main.async { completion(release) }
        }.resume()
    }
}
