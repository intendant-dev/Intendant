import Foundation

private func expect(_ condition: @autoclosure () -> Bool, _ message: String) {
    guard condition() else { fatalError(message) }
}

for version in [
    "0.2.0",
    "v0.2.0",
    "0.2.0-alpha.1",
    "0.2.0-alpha.10",
    "0.2.0-rc.1",
] {
    expect(!UpdateChecker.isDevBuild(version), "release misclassified as dev: \(version)")
}

for version in [
    "",
    "garbage",
    "1.2",
    "1.2.3.4",
    "0.0.0-dev",
    "0.0.0-1a2b3c4d",
    "1.2.0-4-g1a2b3c4d",
    "1.2.0-4-g1a2b3c4d-dirty",
    "0.2.0-alpha.6-dirty",
    "0.2.0-beta.1",
] {
    expect(UpdateChecker.isDevBuild(version), "dev build misclassified as release: \(version)")
}

expect(
    UpdateChecker.isNewer(remote: "v0.2.0-alpha.6", than: "0.2.0-alpha.5"),
    "a later alpha must win"
)
expect(
    !UpdateChecker.isNewer(remote: "v0.2.0-alpha.5", than: "0.2.0-alpha.6"),
    "an earlier alpha must not win"
)
expect(
    !UpdateChecker.isNewer(remote: "v0.2.0-alpha.6", than: "0.2.0-alpha.6"),
    "the same alpha must not win"
)
expect(
    UpdateChecker.isNewer(remote: "v0.2.0-rc.1", than: "0.2.0-alpha.99"),
    "rc must follow alpha"
)
expect(
    UpdateChecker.isNewer(remote: "v0.2.0", than: "0.2.0-rc.9"),
    "stable must follow rc"
)
expect(
    UpdateChecker.isNewer(remote: "v0.2.0-rc.2", than: "0.2.0-rc.1"),
    "a later rc must win"
)
expect(
    !UpdateChecker.isNewer(remote: "v0.2.0-alpha.1", than: "0.2.0"),
    "alpha must not replace stable"
)
expect(
    UpdateChecker.isNewer(remote: "v0.3.0-alpha.1", than: "0.2.9"),
    "a later core must win regardless of stage"
)
expect(
    UpdateChecker.isNewer(remote: "v0.3.0", than: "0.2.0-4-g1a2b3c4d"),
    "a later core must still update a git-describe build"
)
expect(
    !UpdateChecker.isNewer(remote: "v0.2.0-alpha.7", than: "0.2.0-4-g1a2b3c4d"),
    "ambiguous same-core dev builds must not prompt"
)
expect(
    !UpdateChecker.isNewer(remote: "not-a-version", than: "0.2.0-alpha.1"),
    "garbage must not prompt"
)

print("UpdateChecker version tests passed")
