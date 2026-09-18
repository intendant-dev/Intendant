//! Explicit opt-in only. No normal test invokes this, nor does it start a
//! daemon/config/auth stack. All selectors come from this test's own helper.

use super::*;

pub(super) fn run() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("native monitor smoke requires macOS".into());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        let worker = tokio::spawn(run_with_factory(rx, process::Process::spawn));
        let result = exercise(&tx, directory.path()).await;
        // Every error also closes the pipe and joins exact-child cleanup.
        // No deadline abandons the native owner during shutdown.
        drop(tx);
        worker.await.map_err(|e| e.to_string())??;
        result
    })
}

async fn request(tx: &mpsc::Sender<Request>, action: Action) -> Result<Receipt, String> {
    let (reply, receive) = oneshot::channel();
    tx.send(Request {
        action,
        authority: Authority {
            owner_surface: true,
            autonomy: std::sync::Arc::new(tokio::sync::RwLock::new(Default::default())),
        },
        reply,
        element_dispatch: None,
    })
    .await
    .map_err(|_| "smoke broker closed")?;
    receive.await.map_err(|_| "smoke result closed")?
}

async fn exercise(tx: &mpsc::Sender<Request>, directory: &std::path::Path) -> Result<(), String> {
    let created = request(
        tx,
        Action::Create {
            width: 640,
            height: 480,
        },
    )
    .await?;
    let Value::Created(monitor) = &created.value else {
        return Err("missing created monitor".into());
    };
    let monitor = monitor.clone();
    if !created.commit() {
        return Err("create receipt expired".into());
    }
    let image = request(
        tx,
        Action::Capture {
            selector: monitor.selector.clone(),
            path: directory.join("owned.png"),
        },
    )
    .await?;
    let Value::Captured(screenshot) = &image.value else {
        return Err("missing captured frame".into());
    };
    if screenshot.width != 640
        || screenshot.height != 480
        || !screenshot.png.starts_with(b"\x89PNG\r\n\x1a\n")
    {
        return Err("unexpected exact-monitor image geometry/format".into());
    }
    if !image.commit() {
        return Err("image receipt expired".into());
    }
    let destroy = || Action::Destroy {
        display_id: monitor.display_id,
        selector: monitor.selector.clone(),
    };
    if !request(tx, destroy()).await?.commit() {
        return Err("destroy receipt expired".into());
    }
    if request(tx, destroy()).await.is_ok() {
        return Err("stale generation destruction accepted".into());
    }
    if request(
        tx,
        Action::Capture {
            selector: monitor.selector,
            path: directory.join("stale.png"),
        },
    )
    .await
    .is_ok()
    {
        return Err("stale generation capture accepted".into());
    }
    // A second test-owned monitor remains live to exercise EOF/owner cleanup.
    if !request(
        tx,
        Action::Create {
            width: 640,
            height: 480,
        },
    )
    .await?
    .commit()
    {
        return Err("create receipt expired".into());
    }
    eprintln!("owned-monitor native lifecycle + exact capture passed; joining EOF cleanup");
    Ok(())
}
