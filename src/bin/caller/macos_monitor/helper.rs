//! Runs only on the process main thread, before the normal controller startup.
//! EOF (including parent death) drops the retained native owner. No sockets,
//! runtime, configuration, credentials, dashboard or display-session registry.

use super::controls::Native;
#[cfg(not(target_os = "macos"))]
use super::placement::UnsupportedNative as PlacementNative;
use super::placement::{Bounds, Windows};
use super::protocol::*;
#[cfg(target_os = "macos")]
use crate::ax::PlacementNative;
use intendant_platform::cgvirtual::{Dimensions, Handle, VirtualDisplays, MAX_DISPLAYS};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};

trait Owner {
    type Handle;
    fn create(&mut self, size: Dimensions) -> Result<Self::Handle, String>;
    fn info(&self, handle: &Self::Handle) -> Result<(u32, Dimensions), String>;
    fn destroy(&mut self, handle: &Self::Handle) -> Result<(), String>;
    fn bounds(&self, _handle: &Self::Handle) -> Result<Bounds, String> {
        Err("owned monitor geometry unavailable".into())
    }
}

impl Owner for VirtualDisplays {
    type Handle = Handle;
    fn create(&mut self, size: Dimensions) -> Result<Handle, String> {
        self.create(size).map_err(|e| e.to_string())
    }
    fn info(&self, handle: &Handle) -> Result<(u32, Dimensions), String> {
        self.info(handle)
            .map(|i| (i.native_id, i.dimensions))
            .map_err(|e| e.to_string())
    }
    fn bounds(&self, handle: &Handle) -> Result<Bounds, String> {
        self.bounds(handle)
            .map(|(x, y, width, height)| Bounds {
                x,
                y,
                width,
                height,
            })
            .map_err(|e| e.to_string())
    }
    fn destroy(&mut self, handle: &Handle) -> Result<(), String> {
        self.destroy(handle).map_err(|e| e.to_string())
    }
}

pub(super) fn run() -> Result<(), String> {
    let owner = match VirtualDisplays::open() {
        Ok(owner) => owner,
        Err(error) => {
            let message = error.to_string();
            let _ = write_reply(
                &mut std::io::stdout().lock(),
                0,
                Outcome::Error {
                    message: bounded_error(&message),
                    fatal: true,
                },
            );
            return Err(message);
        }
    };
    serve(
        owner,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    )
}

fn bounded_error(message: &str) -> String {
    message.chars().take(512).collect()
}

fn serve<O: Owner>(
    owner: O,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<(), String> {
    serve_with_windows(owner, Windows::new(PlacementNative), reader, writer)
}

fn serve_with_windows<O: Owner, N: Native>(
    mut owner: O,
    mut windows: Windows<N>,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<(), String> {
    let mut handles = BTreeMap::new();
    let mut next_handle = 1_u32;
    let mut seq = 0_u64;
    write_reply(writer, seq, Outcome::Ready { version: VERSION })?;
    // The closure ensures every exit, including malformed input/output failure,
    // explicitly destroys all still-owned handles before dropping the owner.
    let result: Result<(), String> = (|| {
        while let Some(line) = read_line(reader)? {
            let request: Request = serde_json::from_slice(&line).map_err(|e| e.to_string())?;
            seq = seq.checked_add(1).ok_or("monitor sequence exhausted")?;
            if request.seq != seq {
                return Err("monitor sequence mismatch".into());
            }
            let outcome = match request.op {
                Operation::Create { width, height } => {
                    super::validate_dimensions(width, height)?;
                    if handles.len() >= MAX_DISPLAYS {
                        Outcome::Error {
                            message: "monitor capacity reached".into(),
                            fatal: false,
                        }
                    } else {
                        let id = next_handle;
                        next_handle = next_handle
                            .checked_add(1)
                            .ok_or("monitor generation exhausted")?;
                        let native = owner.create(Dimensions { width, height })?;
                        // Keep the handle even if info fails, so cleanup owns it.
                        handles.insert(id, native);
                        let (native_id, size) = owner.info(&handles[&id])?;
                        Outcome::Monitor {
                            handle: id,
                            native_id,
                            width: size.width,
                            height: size.height,
                        }
                    }
                }
                Operation::Resolve { handle } => match handles.get(&handle) {
                    Some(native) => {
                        let (native_id, size) = owner.info(native)?;
                        Outcome::Monitor {
                            handle,
                            native_id,
                            width: size.width,
                            height: size.height,
                        }
                    }
                    None => Outcome::Error {
                        message: "stale monitor generation".into(),
                        fatal: false,
                    },
                },
                Operation::ListWindows { pid } => window_outcome(
                    windows
                        .candidates(pid)
                        .map(|candidates| Outcome::Windows { candidates }),
                ),
                Operation::BindWindow {
                    handle,
                    identity,
                    candidate,
                } => window_outcome(
                    windows
                        .bind(handle, identity, &candidate, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|binding| Outcome::BoundWindow { binding }),
                ),
                Operation::PlaceWindow { binding, bounds } => window_outcome(
                    windows
                        .place(binding, bounds, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|result| Outcome::PlacedWindow { result }),
                ),
                Operation::ReadWindowElements { binding } => window_outcome(
                    windows
                        .read_elements(binding, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|controls| Outcome::WindowElements { controls }),
                ),
                Operation::ReadKeyboardTarget { binding } => window_outcome(
                    windows
                        .read_keyboard_target(binding, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|target| Outcome::KeyboardTarget { target }),
                ),
                Operation::ActWindowElement {
                    binding,
                    element,
                    action,
                } => window_outcome(
                    windows
                        .act_element(binding, &element, &action, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|result| Outcome::ActedWindowElement { result }),
                ),
                Operation::PreparePointer { binding, point } => window_outcome(
                    windows
                        .prepare_click(binding, point, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|prepared| Outcome::PreparedPointer { prepared }),
                ),
                Operation::ClickPointer { binding, token } => window_outcome(
                    windows
                        .click(binding, &token, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|result| Outcome::ClickedPointer { result }),
                ),
                Operation::PrepareScroll {
                    binding,
                    point,
                    delta_y,
                } => window_outcome(
                    windows
                        .prepare_scroll(binding, point, delta_y, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|prepared| Outcome::PreparedScroll { prepared }),
                ),
                Operation::ScrollPointer { binding, token } => window_outcome(
                    windows
                        .scroll(binding, &token, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|result| Outcome::ScrolledPointer { result }),
                ),
                Operation::PrepareArrow { binding } => window_outcome(
                    windows
                        .prepare_arrow(binding, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|prepared| Outcome::PreparedArrow { prepared }),
                ),
                Operation::PressArrow { binding, token } => window_outcome(
                    windows
                        .press_arrow(binding, &token, |id| {
                            owner.bounds(handles.get(&id).ok_or("stale monitor generation")?)
                        })
                        .map(|result| Outcome::PressedArrow { result }),
                ),
                Operation::UnbindWindow { binding } => window_outcome(
                    windows
                        .unbind(binding)
                        .map(|()| Outcome::UnboundWindow { binding }),
                ),
                Operation::Destroy { handle } => {
                    windows.destroy_monitor(handle);
                    match handles.remove(&handle) {
                        Some(native) => {
                            owner.destroy(&native)?;
                            Outcome::Destroyed { handle }
                        }
                        None => Outcome::Error {
                            message: "stale monitor generation".into(),
                            fatal: false,
                        },
                    }
                }
            };
            write_reply(writer, seq, outcome)?;
        }
        Ok(())
    })();
    if let Err(error) = &result {
        let _ = write_reply(
            writer,
            seq,
            Outcome::Error {
                message: bounded_error(error),
                fatal: true,
            },
        );
    }
    // Release AX references before monitor destruction on EVERY exit. Never
    // close, move, restore or otherwise mutate application windows in cleanup.
    drop(windows);
    let mut cleanup = Ok(());
    for handle in handles.values() {
        if let Err(e) = owner.destroy(handle) {
            cleanup = Err(e);
        }
    }
    // Drop still performs the primitive's final RAII cleanup on this thread.
    drop(owner);
    result.and(cleanup)
}

fn window_outcome(result: Result<Outcome, String>) -> Outcome {
    result.unwrap_or_else(|message| Outcome::Error {
        message: bounded_error(&message),
        fatal: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Fake(Rc<RefCell<Vec<&'static str>>>);
    impl Owner for Fake {
        type Handle = u32;
        fn create(&mut self, _: Dimensions) -> Result<u32, String> {
            self.0.borrow_mut().push("create");
            Ok(7)
        }
        fn info(&self, _: &u32) -> Result<(u32, Dimensions), String> {
            Ok((
                42,
                Dimensions {
                    width: 640,
                    height: 480,
                },
            ))
        }
        fn bounds(&self, _: &u32) -> Result<Bounds, String> {
            Ok(super::super::placement::tests::monitor())
        }
        fn destroy(&mut self, _: &u32) -> Result<(), String> {
            self.0.borrow_mut().push("destroy");
            Ok(())
        }
    }
    #[test]
    fn eof_and_bad_protocol_release_only_owned_objects() {
        for suffix in [
            "",
            "garbage\n",
            "{\"seq\":1,\"op\":{\"op\":\"destroy\",\"handle\":1}}\n",
        ] {
            let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
            let mut input = encode(&Request {
                seq: 1,
                op: Operation::Create {
                    width: 640,
                    height: 480,
                },
            })
            .unwrap();
            input.extend_from_slice(suffix.as_bytes());
            let result = serve(
                Fake(Rc::clone(&log)),
                &mut input.as_slice(),
                &mut Vec::new(),
            );
            assert_eq!(result.is_ok(), suffix.is_empty());
            assert_eq!(*log.borrow(), vec!["create", "destroy"]);
        }
    }
    #[test]
    fn capacity_stale_generation_and_order_are_bounded() {
        let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let mut input = Vec::new();
        for (i, op) in [
            Operation::Create {
                width: 640,
                height: 480,
            },
            Operation::Create {
                width: 640,
                height: 480,
            },
            Operation::Create {
                width: 640,
                height: 480,
            },
            Operation::Destroy { handle: 1 },
            Operation::Destroy { handle: 1 },
            Operation::Create {
                width: 640,
                height: 480,
            },
        ]
        .into_iter()
        .enumerate()
        {
            input.extend(
                encode(&Request {
                    seq: i as u64 + 1,
                    op,
                })
                .unwrap(),
            );
        }
        let mut output = Vec::new();
        serve(Fake(Rc::clone(&log)), &mut input.as_slice(), &mut output).unwrap();
        let replies: Vec<Reply> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert!(matches!(
            replies[3].result,
            Outcome::Error { fatal: false, .. }
        ));
        assert!(matches!(
            replies[5].result,
            Outcome::Error { fatal: false, .. }
        ));
        assert!(matches!(
            replies[6].result,
            Outcome::Monitor { handle: 3, .. }
        ));
        assert_eq!(log.borrow().iter().filter(|v| **v == "create").count(), 3);
        assert_eq!(log.borrow().iter().filter(|v| **v == "destroy").count(), 3);
    }
    #[test]
    fn window_protocol_refusals_and_all_exit_paths_release_references_and_monitors() {
        use super::super::placement::tests::{identity, Fake as FakeWindows};
        for suffix in ["", "broken\n"] {
            let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
            let native = FakeWindows::default();
            let mut windows = Windows::new(native.clone());
            let candidate = windows.candidates(123).unwrap().remove(0).candidate;
            let mut input = Vec::new();
            for (n, op) in [
                Operation::Create {
                    width: 640,
                    height: 480,
                },
                Operation::BindWindow {
                    handle: 1,
                    identity: identity(),
                    candidate: candidate.clone(),
                },
                Operation::PlaceWindow {
                    binding: 1,
                    bounds: Bounds {
                        x: -1.0,
                        y: 0.0,
                        width: 100.0,
                        height: 100.0,
                    },
                },
                Operation::ListWindows { pid: 123 },
            ]
            .into_iter()
            .enumerate()
            {
                input.extend(
                    encode(&Request {
                        seq: n as u64 + 1,
                        op,
                    })
                    .unwrap(),
                );
            }
            input.extend_from_slice(suffix.as_bytes());
            let mut output = Vec::new();
            let result = serve_with_windows(
                Fake(log.clone()),
                windows,
                &mut input.as_slice(),
                &mut output,
            );
            assert_eq!(result.is_ok(), suffix.is_empty());
            assert_eq!(native.0.borrow().retained, 0);
            assert_eq!(native.0.borrow().writes, 0);
            assert_eq!(*log.borrow(), vec!["create", "destroy"]);
            let lines: Vec<Reply> = String::from_utf8(output)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            assert!(matches!(
                lines[2].result,
                Outcome::BoundWindow { binding: 1 }
            ));
            assert!(matches!(
                lines[3].result,
                Outcome::Error { fatal: false, .. }
            ));
            assert!(matches!(lines[4].result, Outcome::Windows { .. }));
        }
    }
    #[test]
    fn monitor_destroy_serializes_binding_release_and_stale_placement_refusal() {
        use super::super::placement::tests::{identity, Fake as FakeWindows};
        let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let native = FakeWindows::default();
        let mut windows = Windows::new(native.clone());
        let candidate = windows.candidates(123).unwrap().remove(0).candidate;
        let mut input = Vec::new();
        for (n, op) in [
            Operation::Create {
                width: 640,
                height: 480,
            },
            Operation::BindWindow {
                handle: 1,
                identity: identity(),
                candidate: candidate.clone(),
            },
            Operation::Destroy { handle: 1 },
            Operation::PlaceWindow {
                binding: 1,
                bounds: Bounds {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0,
                },
            },
        ]
        .into_iter()
        .enumerate()
        {
            input.extend(
                encode(&Request {
                    seq: n as u64 + 1,
                    op,
                })
                .unwrap(),
            );
        }
        serve_with_windows(
            Fake(log.clone()),
            windows,
            &mut input.as_slice(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(native.0.borrow().retained, 0);
        assert_eq!(native.0.borrow().writes, 0);
        assert_eq!(*log.borrow(), vec!["create", "destroy"]);
    }
    #[test]
    fn lost_output_releases_binding_before_owned_cleanup() {
        use super::super::placement::tests::{identity, Fake as FakeWindows};
        struct BrokenWriter(usize);
        impl Write for BrokenWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += 1;
                if self.0 >= 3 {
                    return Err(std::io::ErrorKind::BrokenPipe.into());
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let native = FakeWindows::default();
        let mut windows = Windows::new(native.clone());
        let candidate = windows.candidates(123).unwrap().remove(0).candidate;
        let mut input = Vec::new();
        for (n, op) in [
            Operation::Create {
                width: 640,
                height: 480,
            },
            Operation::BindWindow {
                handle: 1,
                identity: identity(),
                candidate: candidate.clone(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            input.extend(
                encode(&Request {
                    seq: n as u64 + 1,
                    op,
                })
                .unwrap(),
            );
        }
        assert!(serve_with_windows(
            Fake(log.clone()),
            windows,
            &mut input.as_slice(),
            &mut BrokenWriter(0)
        )
        .is_err());
        assert_eq!(native.0.borrow().retained, 0);
        assert_eq!(*log.borrow(), vec!["create", "destroy"]);
    }
}
