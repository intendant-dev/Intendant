use crate::browser_workspace::{
    ensure_managed_chromium, managed_chromium_status, ManagedBrowserInstallOptions,
};

pub async fn run(args: Vec<String>) -> Result<(), String> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(());
    }

    match args[0].as_str() {
        "browser" | "browsers" => run_browsers(&args[1..]).await,
        other => Err(format!(
            "unknown setup command '{other}'. Run `intendant setup --help`."
        )),
    }
}

async fn run_browsers(args: &[String]) -> Result<(), String> {
    let mut check = false;
    let mut force = false;
    let mut json = false;
    let mut print_path = false;
    let mut channel = "stable".to_string();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" | "help" => {
                print_browsers_help();
                return Ok(());
            }
            "--check" => check = true,
            "--force" => force = true,
            "--json" => json = true,
            "--print-path" => print_path = true,
            "--channel" => {
                i += 1;
                channel = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| "--channel requires a value".to_string())?;
            }
            arg if arg.starts_with("--channel=") => {
                channel = arg.trim_start_matches("--channel=").to_string();
            }
            other => return Err(format!("unknown flag {other}")),
        }
        i += 1;
    }

    if check {
        let status = managed_chromium_status();
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&status).map_err(|e| e.to_string())?
            );
        } else if print_path {
            if let Some(path) = status.executable.as_deref() {
                println!("{path}");
            }
        } else if status.installed {
            println!("Managed browser installed");
            if let Some(path) = status.executable.as_deref() {
                println!("  executable: {path}");
            }
            if let Some(source) = status.source.as_deref() {
                println!("  source: {source}");
            }
        } else {
            println!("Managed browser missing");
            println!("  {}", status.message);
        }
        return if status.installed {
            Ok(())
        } else {
            Err(status.message)
        };
    }

    let result = ensure_managed_chromium(ManagedBrowserInstallOptions { channel, force }).await?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
        );
    } else if print_path {
        println!("{}", result.executable);
    } else if result.installed {
        println!(
            "Installed Chrome for Testing {} {} for {}",
            result.channel, result.version, result.platform
        );
        println!("  executable: {}", result.executable);
        println!("  install dir: {}", result.install_dir);
    } else {
        println!("Managed browser already installed");
        println!("  executable: {}", result.executable);
        println!("  source: {}", result.source);
    }

    Ok(())
}

fn print_help() {
    println!(
        "Usage:\n  intendant setup browsers [--check] [--force] [--channel stable|beta|dev|canary] [--json] [--print-path]\n\nCommands:\n  browsers    Install or verify Intendant's managed Chrome for Testing browser"
    );
}

fn print_browsers_help() {
    println!(
        "Usage:\n  intendant setup browsers [options]\n\nOptions:\n  --check       Verify a managed browser exists without downloading\n  --force       Re-download even if a managed browser is already present\n  --channel C   Chrome for Testing channel: stable, beta, dev, or canary (default: stable)\n  --json        Print machine-readable JSON\n  --print-path  Print only the managed browser executable path"
    );
}
