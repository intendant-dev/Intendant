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

    /// Origin of the hosted rendezvous whose public transparency log
    /// commits this repo's releases (`release_manifest` entries;
    /// docs/src/self-hosted-rendezvous.md, "Release transparency").
    static let transparencyLogOrigin = "https://intendant.dev"

    /// Advisory verdict on whether a release tag is committed to the
    /// public transparency log. Fail-open like the CT/bundle tripwires:
    /// a log outage must never block an update, so `unknown` carries
    /// the error for display instead of failing anything.
    enum ReleaseLogStatus {
        case logged(artifactCount: Int)
        case notLogged
        case unknown(String)
    }

    private static func releaseLogAPI(tag: String) -> URL? {
        var components = URLComponents(string: "\(transparencyLogOrigin)/api/log/release-manifest")
        components?.queryItems = [URLQueryItem(name: "tag", value: tag)]
        return components?.url
    }

    /// Ask the transparency log whether it commits a release manifest for
    /// `tag`. Presence-only by design: the installed .app is the extracted
    /// tree, not the downloaded zip, so its release hash is not computable
    /// at runtime — the meaningful in-app advisory is whether the release
    /// is publicly committed at all. `intendant hosted-verify --releases`
    /// is the full out-of-band check. Completion runs on the main queue.
    static func fetchReleaseLogStatus(tag: String, completion: @escaping (ReleaseLogStatus) -> Void) {
        guard let url = releaseLogAPI(tag: tag) else {
            DispatchQueue.main.async { completion(.unknown("could not build log URL")) }
            return
        }
        var request = URLRequest(
            url: url,
            cachePolicy: .reloadIgnoringLocalCacheData,
            timeoutInterval: 15
        )
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        URLSession.shared.dataTask(with: request) { data, response, error in
            var status = ReleaseLogStatus.unknown("unexpected response")
            if let error = error {
                status = .unknown(error.localizedDescription)
            } else if let http = response as? HTTPURLResponse, http.statusCode != 200 {
                status = .unknown("HTTP \(http.statusCode)")
            } else if let data = data,
                      let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
                      object["ok"] as? Bool == true {
                if object["found"] as? Bool == true {
                    var artifactCount = 0
                    if let leafData = (object["leaf_json"] as? String)?.data(using: .utf8),
                       let leaf = (try? JSONSerialization.jsonObject(with: leafData)) as? [String: Any],
                       let artifacts = leaf["artifacts"] as? [[String: Any]] {
                        artifactCount = artifacts.count
                    }
                    status = .logged(artifactCount: artifactCount)
                } else {
                    status = .notLogged
                }
            }
            DispatchQueue.main.async { completion(status) }
        }.resume()
    }

    /// The advisory line the update alerts append — honest in all three
    /// states, blocking in none (the unknown case surfaces the error
    /// instead of hiding it).
    static func advisoryLine(for status: ReleaseLogStatus, tag: String) -> String {
        switch status {
        case .logged(let count):
            return "Transparency log: release \(tag) is publicly committed"
                + " (\(count) artifact\(count == 1 ? "" : "s"))."
                + " Full out-of-band check: intendant hosted-verify --releases \(tag)"
        case .notLogged:
            return "Transparency log: release \(tag) is NOT committed to the public log at"
                + " \(transparencyLogOrigin) — treat the download with suspicion and verify its"
                + " sha256 against the release page before opening it."
        case .unknown(let error):
            return "Transparency log: couldn't check release \(tag) (\(error))."
                + " Updating is not blocked; verify later with:"
                + " intendant hosted-verify --releases \(tag)"
        }
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
