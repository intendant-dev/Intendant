//! Checked application-local focus observation when the system-wide AX router
//! cannot complete. This never replaces an unavailable element with a safe state.
use super::placement;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Process {
    pub pid: i32,
    pub birth: (u64, u32),
}

pub(crate) trait Native {
    type Element: PartialEq;
    /// The currently foreground process and its live start identity.
    fn process(&mut self) -> Result<Process, String>;
    /// AXFrontmost on the exact application object retained for `expected`.
    fn frontmost(&mut self) -> Result<bool, String>;
    fn focused(&mut self) -> Result<Self::Element, String>;
    fn element_pid(&mut self, element: &Self::Element) -> Result<i32, String>;
}

fn check<N: Native>(n: &mut N, expected: Process, deadline: Instant) -> Result<(), String> {
    placement::time_left(deadline)?;
    if n.process()? != expected || !n.frontmost()? {
        return Err(
            "foreground application identity/state changed during focus observation".into(),
        );
    }
    placement::time_left(deadline)
}

pub(crate) fn observe<N: Native>(
    n: &mut N,
    expected: Process,
    deadline: Instant,
) -> Result<N::Element, String> {
    if expected.pid <= 0 || expected.birth.0 == 0 || expected.birth.1 >= 1_000_000 {
        return Err("invalid foreground process identity".into());
    }
    check(n, expected, deadline)?;
    let first = n.focused()?;
    if n.element_pid(&first)? != expected.pid {
        return Err("focused element belongs to another process".into());
    }
    check(n, expected, deadline)?;
    let second = n.focused()?;
    if n.element_pid(&second)? != expected.pid || first != second {
        return Err("focused element changed during application observation".into());
    }
    check(n, expected, deadline)?;
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, time::Duration};

    #[derive(Clone)]
    enum Reply {
        Process(Process),
        Front(bool),
        Element(u64),
        Pid(i32),
        Failure,
    }
    struct Fake(VecDeque<Reply>, usize);
    impl Fake {
        fn next(&mut self) -> Result<Reply, String> {
            self.1 += 1;
            match self.0.pop_front().expect("unexpected extra focus query") {
                Reply::Failure => Err("metadata unavailable".into()),
                reply => Ok(reply),
            }
        }
    }
    impl Native for Fake {
        type Element = u64;
        fn process(&mut self) -> Result<Process, String> {
            let Reply::Process(value) = self.next()? else {
                panic!("process query order")
            };
            Ok(value)
        }
        fn frontmost(&mut self) -> Result<bool, String> {
            let Reply::Front(value) = self.next()? else {
                panic!("frontmost query order")
            };
            Ok(value)
        }
        fn focused(&mut self) -> Result<u64, String> {
            let Reply::Element(value) = self.next()? else {
                panic!("element query order")
            };
            Ok(value)
        }
        fn element_pid(&mut self, _: &u64) -> Result<i32, String> {
            let Reply::Pid(value) = self.next()? else {
                panic!("PID query order")
            };
            Ok(value)
        }
    }
    fn identity() -> Process {
        Process {
            pid: 41,
            birth: (100, 20),
        }
    }
    fn replies() -> Vec<Reply> {
        vec![
            Reply::Process(identity()),
            Reply::Front(true),
            Reply::Element(7),
            Reply::Pid(41),
            Reply::Process(identity()),
            Reply::Front(true),
            Reply::Element(7),
            Reply::Pid(41),
            Reply::Process(identity()),
            Reply::Front(true),
        ]
    }
    fn run(replies: Vec<Reply>) -> Result<u64, String> {
        observe(
            &mut Fake(replies.into(), 0),
            identity(),
            Instant::now() + Duration::from_secs(4),
        )
    }
    #[test]
    fn exact_focus_requires_two_reads_and_three_foreground_checks() {
        let mut native = Fake(replies().into(), 0);
        assert_eq!(
            observe(
                &mut native,
                identity(),
                Instant::now() + Duration::from_secs(4)
            )
            .unwrap(),
            7
        );
        assert_eq!(native.1, 10);
        assert!(native.0.is_empty());
    }
    #[test]
    fn any_failed_observation_refuses_instead_of_assuming_stability() {
        for index in 0..10 {
            let mut rows = replies();
            rows[index] = Reply::Failure;
            assert!(run(rows).is_err(), "read {index}");
        }
    }
    #[test]
    fn foreground_changes_and_pid_reuse_refuse_at_every_check() {
        for index in [0, 4, 8] {
            for changed in [
                Process {
                    pid: 42,
                    ..identity()
                },
                Process {
                    birth: (101, 20),
                    ..identity()
                },
                Process {
                    birth: (100, 21),
                    ..identity()
                },
            ] {
                let mut rows = replies();
                rows[index] = Reply::Process(changed);
                assert!(run(rows).is_err(), "read {index}");
            }
        }
    }
    #[test]
    fn stale_workspace_candidate_cannot_bypass_application_frontmost() {
        for index in [1, 5, 9] {
            let mut rows = replies();
            rows[index] = Reply::Front(false);
            assert!(run(rows).is_err(), "read {index}");
        }
    }
    #[test]
    fn foreign_and_replaced_elements_refuse() {
        for index in [3, 7] {
            let mut rows = replies();
            rows[index] = Reply::Pid(42);
            assert!(run(rows).is_err());
        }
        let mut rows = replies();
        rows[6] = Reply::Element(8);
        assert!(run(rows).is_err());
    }
    #[test]
    fn invalid_identity_and_expired_deadline_do_not_read_metadata() {
        let mut native = Fake(replies().into(), 0);
        for bad in [
            Process {
                pid: 0,
                ..identity()
            },
            Process {
                birth: (0, 0),
                ..identity()
            },
            Process {
                birth: (100, 1_000_000),
                ..identity()
            },
        ] {
            assert!(observe(&mut native, bad, Instant::now() + Duration::from_secs(4)).is_err());
        }
        assert!(observe(
            &mut native,
            identity(),
            Instant::now() - Duration::from_secs(1)
        )
        .is_err());
        assert_eq!(native.1, 0);
    }
}
