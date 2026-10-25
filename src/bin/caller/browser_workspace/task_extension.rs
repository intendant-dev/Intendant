//! Extensions selected by a task, authorized by the existing immutable startup policy.
//! A request names bytes; it never approves them or selects a personal profile.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskExtension {
    pub archive_path: String,
    pub archive_sha256: String,
    pub archive_byte_length: u64,
    pub manifest_version: u32,
    pub version: String,
}
impl TaskExtension {
    pub(super) fn validate(&self) -> Result<(), String> {
        if !extension_policy::absolute_path_valid(Path::new(&self.archive_path))
            || !extension_policy::sha256_valid(&self.archive_sha256)
            || !(1..=BROWSER_EXTENSION_ARCHIVE_MAX_BYTES).contains(&self.archive_byte_length)
            || self.manifest_version != 3
            || !extension_policy::version_valid(&self.version)
        {
            return Err(
                "extension requires a complete canonical pinned MV3 archive identity".into(),
            );
        }
        extension_policy::current().approved_worker(
            &self.archive_sha256, self.archive_byte_length, self.manifest_version, &self.version,
        ).map(|_| ()).map_err(|_| "extension is not approved by the daemon startup policy; task arguments cannot grant approval".into())
    }
    pub(super) fn matches(&self, extension: Option<&BrowserWorkspaceExtension>) -> bool {
        extension.is_some_and(|extension| {
            extension.archive_sha256 == self.archive_sha256
                && extension.archive_byte_length == self.archive_byte_length
                && extension.manifest_version == self.manifest_version
                && extension.version == self.version
        })
    }
}

/// The original website remains the only non-extension page. Other UI/worker
/// targets may belong only to the exact extension loaded into this fresh profile.
/// An idle MV3 worker may disappear: it is not the lifetime of an installation.
#[cfg(any(target_os = "macos", test))]
pub(super) fn validate_targets(
    targets: &serde_json::Value,
    original: &str,
    extension: Option<&BrowserWorkspaceExtension>,
) -> Result<(), String> {
    let targets = targets
        .as_array()
        .filter(|rows| rows.len() <= 64)
        .ok_or("bounded browser target inventory unavailable")?;
    let extension_id = extension.and_then(|e| e.runtime_id.as_deref());
    if extension.is_some() && !extension_id.is_some_and(runtime_id_valid) {
        return Err("loaded extension identity unavailable".into());
    }
    let mut originals = 0;
    let mut seen = std::collections::BTreeSet::new();
    for target in targets {
        let id = target["targetId"]
            .as_str()
            .ok_or("browser target identity unavailable")?;
        if id.is_empty() || id.len() > 128 || !seen.insert(id) {
            return Err("browser target identity is missing or duplicated".into());
        }
        let kind = target["type"]
            .as_str()
            .ok_or("browser target type unavailable")?;
        let url = target["url"]
            .as_str()
            .ok_or("browser target URL unavailable")?;
        if id == original {
            if kind != "page" || url.starts_with("chrome-extension:") {
                return Err("original browser page changed kind or origin".into());
            }
            originals += 1;
        } else if url.starts_with("chrome-extension:") {
            if !extension_id.is_some_and(|id| extension_url_matches(url, id)) {
                return Err("foreign extension target refused".into());
            }
        } else if kind == "page" {
            return Err(
                "unexpected non-extension page; original task page was not substituted".into(),
            );
        }
    }
    if originals != 1 {
        return Err("original task page is missing or ambiguous".into());
    }
    Ok(())
}
#[cfg(any(target_os = "macos", test))]
pub(super) fn runtime_id_valid(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| (b'a'..=b'p').contains(&byte))
}
#[cfg(any(target_os = "macos", test))]
pub(super) fn extension_url_matches(raw: &str, id: &str) -> bool {
    runtime_id_valid(id)
        && raw.starts_with(&format!("chrome-extension://{id}/"))
        && url::Url::parse(raw).is_ok_and(|url| {
            url.scheme() == "chrome-extension"
                && url.host_str() == Some(id)
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn task_cannot_approve_an_extension_or_select_a_personal_profile() {
        let request = json!({"archive_path":"/tmp/fixture.zip","archive_sha256":"a".repeat(64),
            "archive_byte_length":100,"manifest_version":3,"version":"1.0"});
        let extension: TaskExtension = serde_json::from_value(request.clone()).unwrap();
        assert!(extension.validate().is_err());
        for field in ["policy", "service_worker", "profile_dir", "owner_surface"] {
            let mut forged = request.clone();
            forged[field] = json!(true);
            assert!(serde_json::from_value::<TaskExtension>(forged).is_err());
        }
    }
    #[test]
    fn extension_origin_cannot_alias_another_runtime_or_url_scheme() {
        let id = "a".repeat(32);
        assert!(extension_url_matches(
            &format!("chrome-extension://{id}/popup.html#view"),
            &id
        ));
        for url in [
            format!("https://{id}/popup.html"),
            format!("chrome-extension://{id}.evil/popup.html"),
            format!("chrome-extension://{id}@example.test/popup.html"),
            format!("chrome-extension://{id}:123/popup.html"),
        ] {
            assert!(!extension_url_matches(&url, &id));
        }
    }
    #[test]
    fn ordinary_browser_still_requires_its_only_original_page() {
        let original = json!({"targetId":"page-one","type":"page","url":"https://example.test"});
        assert!(validate_targets(&json!([original.clone()]), "page-one", None).is_ok());
        assert!(validate_targets(&json!([original.clone(), original]), "page-one", None).is_err());
        assert!(validate_targets(&json!([]), "page-one", None).is_err());
    }
    #[test]
    fn task_targets_never_adopt_another_extension_page_or_recycled_id() {
        let extension = BrowserWorkspaceExtension {
            archive_sha256: "a".repeat(64),
            archive_byte_length: 10,
            manifest_version: 3,
            version: "1.0".into(),
            service_worker: "worker.js".into(),
            load_path: "/test/extension".into(),
            runtime_id: Some("a".repeat(32)),
        };
        let page = json!({"targetId":"original","type":"page","url":"https://example.test/"});
        let own = json!({"targetId":"popup","type":"page","url":format!("chrome-extension://{}/popup.html","a".repeat(32))});
        assert!(validate_targets(
            &json!([page.clone(), own.clone()]),
            "original",
            Some(&extension)
        )
        .is_ok());
        assert!(validate_targets(
            &json!([page.clone(), own.clone(), own.clone()]),
            "original",
            Some(&extension)
        )
        .is_err());
        let mut foreign = own.clone();
        foreign["url"] = json!(format!("chrome-extension://{}/popup.html", "b".repeat(32)));
        assert!(validate_targets(
            &json!([page.clone(), foreign]),
            "original",
            Some(&extension)
        )
        .is_err());
        let mut web = own;
        web["url"] = json!("https://other.test/");
        assert!(
            validate_targets(&json!([page.clone(), web]), "original", Some(&extension)).is_err()
        );
        // A sleeping worker is expected; it must not revoke an installation.
        assert!(validate_targets(&json!([page]), "original", Some(&extension)).is_ok());
    }
}
