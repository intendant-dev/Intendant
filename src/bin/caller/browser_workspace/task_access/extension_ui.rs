//! Pure input/origin bounds shared by offscreen extension operations. No IO.
use super::*;
use serde_json::{json, Value};

pub(super) fn resource_url(id: &str, path: &str) -> Result<String, String> {
    if !super::super::task_extension::runtime_id_valid(id)
        || path.len() > 2048
        || path.chars().any(char::is_control)
        || path.contains('\\')
    {
        return Err("invalid extension resource identity".into());
    }
    let name = path.split(['?', '#']).next().unwrap_or_default();
    if !extension_policy::relative_path_valid(name) || name.contains('%') {
        return Err(
            "extension resource must be a literal relative path, not a URL or traversal".into(),
        );
    }
    let url = format!("chrome-extension://{id}/{path}");
    if !super::super::task_extension::extension_url_matches(&url, id) {
        return Err("extension resource escaped its installed origin".into());
    }
    Ok(url)
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn validate_target(
    info: &Value,
    id: &str,
    extension_id: Option<&str>,
) -> Result<(), String> {
    if info["targetId"] != id || info["type"] != "page" {
        return Err("page target changed identity or type".into());
    }
    let url = info["url"].as_str().ok_or("page URL unavailable")?;
    if let Some(extension_id) = extension_id {
        if !super::super::task_extension::extension_url_matches(url, extension_id) {
            return Err("extension view left its assigned extension origin".into());
        }
    } else if url.starts_with("chrome-extension:") {
        return Err("website view changed to an extension".into());
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn viewport(metrics: &Value) -> Result<(f64, f64), String> {
    let v = &metrics["cssLayoutViewport"];
    let width = v["clientWidth"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 1.0 && *n <= 4096.0)
        .ok_or("page width outside capture bounds")?;
    let height = v["clientHeight"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 1.0 && *n <= 4096.0)
        .ok_or("page height outside capture bounds")?;
    Ok((width, height))
}

pub(super) fn pointer_commands(
    x: f64,
    y: f64,
    delta_y: Option<i32>,
    size: (f64, f64),
) -> Result<Vec<Value>, String> {
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 || x >= size.0 || y >= size.1 {
        return Err("page point lies outside the captured viewport".into());
    }
    if let Some(delta) = delta_y {
        crate::macos_monitor::scroll::validate_delta(delta)?;
        Ok(vec![
            json!({"type":"mouseWheel","x":x,"y":y,"deltaX":0,"deltaY":delta,
            "modifiers":0,"pointerType":"mouse"}),
        ])
    } else {
        Ok(vec![
            json!({"type":"mousePressed","x":x,"y":y,"button":"left","buttons":1,
            "clickCount":1,"modifiers":0,"pointerType":"mouse"}),
            json!({"type":"mouseReleased","x":x,"y":y,"button":"left","buttons":0,
            "clickCount":1,"modifiers":0,"pointerType":"mouse"}),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extension_resource_is_an_exact_owned_origin_not_arbitrary_navigation() {
        let id = "a".repeat(32);
        assert_eq!(
            resource_url(&id, "popup.html?tab=3#view").unwrap(),
            format!("chrome-extension://{id}/popup.html?tab=3#view")
        );
        for path in [
            "https://example.test/",
            "//example.test/x",
            "../x",
            "%2e%2e/x",
            "x/../../y",
            "javascript:alert(1)",
            "a\\b",
            "",
        ] {
            assert!(resource_url(&id, path).is_err(), "{path}");
        }
    }
    #[test]
    fn page_target_selection_does_not_adopt_foreign_extensions_or_worker_targets() {
        let id = "a".repeat(32);
        let info = json!({"targetId":"retained","type":"page","url":format!("chrome-extension://{id}/popup.html")});
        assert!(validate_target(&info, "retained", Some(&id)).is_ok());
        assert!(validate_target(&info, "other", Some(&id)).is_err());
        assert!(validate_target(&info, "retained", Some(&"b".repeat(32))).is_err());
        assert!(validate_target(&info, "retained", None).is_err());
        let mut worker = info;
        worker["type"] = json!("service_worker");
        assert!(validate_target(&worker, "retained", Some(&id)).is_err());
    }
    #[test]
    fn pointer_pairs_and_wheels_are_page_bounded_and_never_global() {
        let click = pointer_commands(10.0, 20.0, None, (720.0, 443.0)).unwrap();
        assert_eq!(click.len(), 2);
        assert_eq!(click[0]["type"], "mousePressed");
        assert_eq!(click[1]["type"], "mouseReleased");
        assert_eq!(click[1]["buttons"], 0);
        for (x, y) in [(f64::NAN, 0.0), (-1.0, 0.0), (720.0, 2.0), (2.0, 443.0)] {
            assert!(pointer_commands(x, y, None, (720.0, 443.0)).is_err());
        }
        let wheel = pointer_commands(1.0, 1.0, Some(180), (720.0, 443.0)).unwrap();
        assert_eq!(wheel.len(), 1);
        assert_eq!(wheel[0]["deltaY"], 180);
        assert!(pointer_commands(1.0, 1.0, Some(601), (720.0, 443.0)).is_err());
    }
}
