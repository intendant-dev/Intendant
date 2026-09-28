//! Private main-thread macOS browser-workspace launcher.
//! Authority and browser policy stay in the controller; this shim only owns the
//! exact Chrome-for-Testing application lifecycle and nonactivation invariant.

use std::{ffi::CString, path::Path};

pub fn supervise(bundle: &Path, arguments: &[String]) -> Result<(), String> {
    extern "C" {
        fn intendant_macos_browser_supervise(
            bundle_path: *const std::ffi::c_char,
            argc: i32,
            argv: *const *const std::ffi::c_char,
        ) -> i32;
    }
    let bundle = bundle
        .to_str()
        .ok_or("managed browser bundle path is not UTF-8")?;
    let bundle = CString::new(bundle).map_err(|_| "managed browser bundle path contains NUL")?;
    let encoded = arguments
        .iter()
        .map(|argument| {
            CString::new(argument.as_str()).map_err(|_| "browser launch argument contains NUL")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let pointers = encoded
        .iter()
        .map(|argument| argument.as_ptr())
        .collect::<Vec<_>>();
    let argc = i32::try_from(pointers.len()).map_err(|_| "too many browser launch arguments")?;
    // SAFETY: all C strings and the pointer array remain alive for the blocking
    // call. The native shim catches Objective-C exceptions and owns no Rust data.
    let code =
        unsafe { intendant_macos_browser_supervise(bundle.as_ptr(), argc, pointers.as_ptr()) };
    if code == 0 {
        Ok(())
    } else {
        Err(format!(
            "private macOS browser supervisor exited with code {code}"
        ))
    }
}

pub fn intercept_supervisor() -> Option<Result<(), String>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args
        .first()
        .is_none_or(|arg| arg != "--private-macos-browser-workspace-v1")
    {
        return None;
    }
    if args.len() < 3 {
        return Some(Err(
            "private macOS browser supervisor requires bundle path and browser arguments".into(),
        ));
    }
    let bundle = std::path::PathBuf::from(&args[1]);
    let arguments = args[2..]
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| "private macOS browser argument is not UTF-8".to_string())
        })
        .collect::<Result<Vec<_>, _>>();
    Some(arguments.and_then(|arguments| supervise(&bundle, &arguments)))
}
