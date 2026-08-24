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

    private enum ReleaseStage: Int {
        case alpha = 0
        case rc = 1
        case stable = 2
    }

    private struct ReleaseVersion {
        let stage: ReleaseStage
        let stageNumber: Int
    }

    /// A release tag is dotted numeric, optionally followed by the alpha/rc
    /// channels this repository publishes. Everything else — including
    /// git-describe distance/hash suffixes and `-dirty` — is a development
    /// build and should not produce a launch-time update prompt.
    static func isDevBuild(_ version: String) -> Bool {
        parseReleaseVersion(version) == nil
    }

    /// Compares dotted numeric cores first, then the published prerelease
    /// sequence (`alpha.N` < `rc.N` < stable). Git-describe builds retain the
    /// old core-only behavior: a later release core can be reported by the
    /// explicit menu check, but an ambiguous suffix on the same core never
    /// produces an update prompt.
    static func isNewer(remote: String, than local: String) -> Bool {
        guard let remoteCore = numericCore(remote),
              let localCore = numericCore(local) else { return false }

        for index in 0..<max(remoteCore.count, localCore.count) {
            let remotePart = index < remoteCore.count ? remoteCore[index] : 0
            let localPart = index < localCore.count ? localCore[index] : 0
            if remotePart != localPart {
                return remotePart > localPart
            }
        }

        guard let remoteRelease = parseReleaseVersion(remote),
              let localRelease = parseReleaseVersion(local) else { return false }
        if remoteRelease.stage != localRelease.stage {
            return remoteRelease.stage.rawValue > localRelease.stage.rawValue
        }
        return remoteRelease.stage != .stable
            && remoteRelease.stageNumber > localRelease.stageNumber
    }

    private static func numericCore(_ version: String) -> [Int]? {
        var candidate = version
        if candidate.hasPrefix("v") || candidate.hasPrefix("V") {
            candidate.removeFirst()
        }
        guard !candidate.isEmpty else { return nil }
        let core = candidate.split(
            separator: "-",
            maxSplits: 1,
            omittingEmptySubsequences: false
        )[0]
        let rawParts = core.split(separator: ".", omittingEmptySubsequences: false)
            .map { Int($0) }
        guard rawParts.count == 3, !rawParts.contains(nil) else { return nil }
        return rawParts.compactMap { $0 }
    }

    private static func parseReleaseVersion(_ version: String) -> ReleaseVersion? {
        var candidate = version
        if candidate.hasPrefix("v") || candidate.hasPrefix("V") {
            candidate.removeFirst()
        }
        guard !candidate.isEmpty, numericCore(candidate) != nil else { return nil }

        let pieces = candidate.split(
            separator: "-",
            maxSplits: 1,
            omittingEmptySubsequences: false
        )
        guard pieces.count == 2 else {
            return ReleaseVersion(stage: .stable, stageNumber: 0)
        }

        let prerelease = pieces[1].split(separator: ".", omittingEmptySubsequences: false)
        guard prerelease.count == 2,
              let stageNumber = Int(prerelease[1]),
              stageNumber >= 0 else { return nil }
        let stage: ReleaseStage
        switch prerelease[0].lowercased() {
        case "alpha": stage = .alpha
        case "rc": stage = .rc
        default: return nil
        }
        return ReleaseVersion(stage: stage, stageNumber: stageNumber)
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
