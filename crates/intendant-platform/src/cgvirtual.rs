//! Experimental macOS virtual monitor lifecycle, deliberately unwired from CU.
//!
//! A monitor shares WindowServer focus, cursor and clipboard with the login
//! session. It is **not a security sandbox** and this API grants no authority.
//! Native calls require the main thread; the owner and handles are !Send/!Sync.
//! Unit tests use only injected metadata and fake objects, including on macOS.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Conservative first-slice limits: two 1x monitors, each at most 4096² pixels.
pub const MAX_DISPLAYS: usize = 2;
pub const MIN_DIMENSION: u32 = 64;
pub const MAX_DIMENSION: u32 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

impl Dimensions {
    fn validate(self) -> Result<(), Error> {
        if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&self.width)
            || !(MIN_DIMENSION..=MAX_DIMENSION).contains(&self.height)
        {
            return Err(Error::InvalidDimensions);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    UnsupportedPlatform,
    MainThreadRequired,
    UnavailableAbi(String),
    InvalidDimensions,
    OwnerAlreadyOpen,
    Capacity,
    GenerationExhausted,
    StaleHandle,
    InvalidNativeId,
    NativeFailure(&'static str),
    TeardownUnconfirmed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => f.write_str("CGVirtualDisplay requires macOS"),
            Self::MainThreadRequired => f.write_str("CGVirtualDisplay requires the main thread"),
            Self::UnavailableAbi(detail) => write!(f, "CGVirtualDisplay ABI unavailable: {detail}"),
            Self::InvalidDimensions => write!(
                f,
                "dimensions must each be {MIN_DIMENSION}..={MAX_DIMENSION}"
            ),
            Self::OwnerAlreadyOpen => {
                f.write_str("a CGVirtualDisplay owner is already open in this process")
            }
            Self::Capacity => write!(f, "CGVirtualDisplay limit is {MAX_DISPLAYS} per process"),
            Self::GenerationExhausted => {
                f.write_str("CGVirtualDisplay generation counter exhausted")
            }
            Self::StaleHandle => f.write_str("stale or foreign CGVirtualDisplay handle"),
            Self::InvalidNativeId => {
                f.write_str("native object returned a zero or duplicate display ID")
            }
            Self::NativeFailure(stage) => write!(f, "CGVirtualDisplay failed: {stage}"),
            Self::TeardownUnconfirmed => f.write_str(
                "CGVirtualDisplay teardown unconfirmed; further creation disabled in this process",
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Opaque, owner-bound generation. Cannot be constructed from a CGDisplayID.
/// Handles do not keep native objects alive after owner destruction.
#[derive(Clone, Debug)]
pub struct Handle {
    owner: Rc<()>,
    generation: u64,
}

/// The ID comes only from the retained native object's `displayID` getter.
/// It is neither an Intendant DisplayTarget ID nor an authorization token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    pub native_id: u32,
    pub dimensions: Dimensions,
}

static OWNER_OPEN: AtomicBool = AtomicBool::new(false);

struct OwnerPermit;

impl Drop for OwnerPermit {
    fn drop(&mut self) {
        OWNER_OPEN.store(false, Ordering::Release);
    }
}

/// One main-thread owner per process. Drop tears down every remaining object.
/// `destroy` reports teardown errors; Drop can only latch them and disable
/// further creation. No physical display is selected or used as a fallback.
pub struct VirtualDisplays {
    // Field order matters: release objects before admitting another owner.
    registry: Registry<crate::platform::NativeCGVirtualDisplay>,
    _permit: OwnerPermit,
}

impl VirtualDisplays {
    /// Probe classes and full method ABIs without creating a native monitor.
    /// This is an ABI check, not a promise of WindowServer/TCC/capture readiness.
    pub fn open() -> Result<Self, Error> {
        crate::platform::NativeCGVirtualDisplay::probe()?;
        OWNER_OPEN
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::OwnerAlreadyOpen)?;
        Ok(Self {
            registry: Registry::new(),
            _permit: OwnerPermit,
        })
    }

    /// Explicit native side effect: create a 1x, 60 Hz monitor in the current
    /// WindowServer session. No capture, input, TCC prompts or layout changes
    /// are requested; WindowServer itself can rearrange windows on hotplug.
    pub fn create(&mut self, dimensions: Dimensions) -> Result<Handle, Error> {
        self.registry.create(dimensions, || {
            crate::platform::NativeCGVirtualDisplay::create(dimensions)
        })
    }

    pub fn info(&self, handle: &Handle) -> Result<DisplayInfo, Error> {
        self.registry.info(handle)
    }

    /// Invalidate this exact generation, release its objects, and poll removal
    /// for at most two seconds. Private OS calls themselves are not interruptible.
    /// An error never permits retrying destruction by numeric display ID.
    pub fn destroy(&mut self, handle: &Handle) -> Result<(), Error> {
        self.registry.destroy(handle)
    }
}

pub(crate) trait NativeObject {
    fn native_id(&self) -> u32;
    fn shutdown(&mut self) -> Result<(), Error>;
}

struct Entry<T> {
    generation: u64,
    info: DisplayInfo,
    object: T,
}

struct Registry<T> {
    owner: Rc<()>,
    next_generation: u64,
    entries: Vec<Entry<T>>,
    poisoned: bool,
}

impl<T: NativeObject> Registry<T> {
    fn new() -> Self {
        Self {
            owner: Rc::new(()),
            next_generation: 1,
            entries: Vec::new(),
            poisoned: false,
        }
    }

    fn create(
        &mut self,
        dimensions: Dimensions,
        create: impl FnOnce() -> Result<T, Error>,
    ) -> Result<Handle, Error> {
        dimensions.validate()?;
        if self.poisoned {
            return Err(Error::TeardownUnconfirmed);
        }
        if self.entries.len() >= MAX_DISPLAYS {
            return Err(Error::Capacity);
        }
        let generation = self.next_generation;
        self.next_generation = generation
            .checked_add(1)
            .ok_or(Error::GenerationExhausted)?;
        let mut object = create()?;
        let native_id = object.native_id();
        if native_id == 0 || self.entries.iter().any(|e| e.info.native_id == native_id) {
            if object.shutdown().is_err() {
                self.poisoned = true;
                return Err(Error::TeardownUnconfirmed);
            }
            return Err(Error::InvalidNativeId);
        }
        self.entries.push(Entry {
            generation,
            info: DisplayInfo {
                native_id,
                dimensions,
            },
            object,
        });
        Ok(Handle {
            owner: self.owner.clone(),
            generation,
        })
    }

    fn index(&self, handle: &Handle) -> Result<usize, Error> {
        if !Rc::ptr_eq(&self.owner, &handle.owner) {
            return Err(Error::StaleHandle);
        }
        self.entries
            .iter()
            .position(|e| e.generation == handle.generation)
            .ok_or(Error::StaleHandle)
    }

    fn info(&self, handle: &Handle) -> Result<DisplayInfo, Error> {
        Ok(self.entries[self.index(handle)?].info)
    }

    fn destroy(&mut self, handle: &Handle) -> Result<(), Error> {
        let index = self.index(handle)?;
        let mut entry = self.entries.remove(index);
        let result = entry.object.shutdown();
        self.poisoned |= result.is_err();
        result
    }
}

// Method encodings are copied one type at a time by the bridge, eliminating
// stack offsets. Exact matching deliberately rejects unfamiliar encodings,
// including integer-width changes and different struct layouts.
#[cfg(any(target_os = "macos", test))]
pub(crate) struct MethodAbi {
    pub class: &'static str,
    pub selector: &'static str,
    pub class_method: bool,
    pub types: &'static str,
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn probe_abi(
    mut lookup: impl FnMut(&MethodAbi) -> Result<String, Error>,
) -> Result<(), Error> {
    let mut check = |class, selector, class_method, types| {
        let method = MethodAbi {
            class,
            selector,
            class_method,
            types,
        };
        let actual = lookup(&method)?;
        if actual != method.types {
            return Err(Error::UnavailableAbi(format!(
                "{class} {selector}: expected {types}, got {actual}"
            )));
        }
        Ok(())
    };
    for class in [
        "CGVirtualDisplayDescriptor",
        "CGVirtualDisplaySettings",
        "CGVirtualDisplayMode",
        "CGVirtualDisplay",
    ] {
        check(class, "alloc", true, "@|@|:")?;
        // NSObject declares release as oneway void in the installed Apple SDK.
        check(class, "release", false, "Vv|@|:")?;
    }
    for class in ["CGVirtualDisplayDescriptor", "CGVirtualDisplaySettings"] {
        check(class, "init", false, "@|@|:")?;
    }
    for selector in [
        "setMaxPixelsWide:",
        "setMaxPixelsHigh:",
        "setVendorID:",
        "setProductID:",
        "setSerialNum:",
        "setSerialNumber:",
    ] {
        check("CGVirtualDisplayDescriptor", selector, false, "v|@|:|I")?;
    }
    for selector in ["setName:", "setQueue:"] {
        check("CGVirtualDisplayDescriptor", selector, false, "v|@|:|@")?;
    }
    check(
        "CGVirtualDisplayDescriptor",
        "setSizeInMillimeters:",
        false,
        "v|@|:|{CGSize=dd}",
    )?;
    for selector in [
        "setRedPrimary:",
        "setGreenPrimary:",
        "setBluePrimary:",
        "setWhitePoint:",
    ] {
        check(
            "CGVirtualDisplayDescriptor",
            selector,
            false,
            "v|@|:|{CGPoint=dd}",
        )?;
    }
    check("CGVirtualDisplaySettings", "setHiDPI:", false, "v|@|:|I")?;
    check("CGVirtualDisplaySettings", "setModes:", false, "v|@|:|@")?;
    check(
        "CGVirtualDisplayMode",
        "initWithWidth:height:refreshRate:",
        false,
        "@|@|:|I|I|d",
    )?;
    check("CGVirtualDisplay", "initWithDescriptor:", false, "@|@|:|@")?;
    check("CGVirtualDisplay", "displayID", false, "I|@|:")?;
    // Apple's BOOL is bool on arm64, signed char on x86_64.
    let apply = if cfg!(target_arch = "aarch64") {
        "B|@|:|@"
    } else {
        "c|@|:|@"
    };
    check("CGVirtualDisplay", "applySettings:", false, apply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const SIZE: Dimensions = Dimensions {
        width: 1024,
        height: 768,
    };

    struct Fake {
        id: u32,
        released: bool,
        fail: bool,
        log: Rc<RefCell<Vec<u32>>>,
    }

    impl NativeObject for Fake {
        fn native_id(&self) -> u32 {
            self.id
        }
        fn shutdown(&mut self) -> Result<(), Error> {
            if !self.released {
                self.log.borrow_mut().push(self.id);
                self.released = true;
            }
            if self.fail {
                Err(Error::TeardownUnconfirmed)
            } else {
                Ok(())
            }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = self.shutdown();
        }
    }

    fn fake(id: u32, log: &Rc<RefCell<Vec<u32>>>) -> Fake {
        Fake {
            id,
            released: false,
            fail: false,
            log: log.clone(),
        }
    }

    #[test]
    fn limits_refuse_before_native_effects() {
        let log = Rc::default();
        let mut registry = Registry::<Fake>::new();
        for size in [
            Dimensions { width: 0, ..SIZE },
            Dimensions { height: 63, ..SIZE },
            Dimensions {
                width: 4097,
                ..SIZE
            },
            Dimensions {
                height: u32::MAX,
                ..SIZE
            },
        ] {
            assert_eq!(
                registry
                    .create(size, || panic!("native effect"))
                    .unwrap_err(),
                Error::InvalidDimensions
            );
        }
        for edge in [MIN_DIMENSION, MAX_DIMENSION] {
            assert!(Dimensions {
                width: edge,
                height: edge
            }
            .validate()
            .is_ok());
        }
        for id in 1..=MAX_DISPLAYS as u32 {
            registry.create(SIZE, || Ok(fake(id, &log))).unwrap();
        }
        assert_eq!(
            registry
                .create(SIZE, || panic!("native effect"))
                .unwrap_err(),
            Error::Capacity
        );
    }

    #[test]
    fn generations_survive_native_id_reuse_and_owner_replacement() {
        let log = Rc::default();
        let mut registry = Registry::new();
        let old = registry.create(SIZE, || Ok(fake(42, &log))).unwrap();
        registry.destroy(&old).unwrap();
        let new = registry.create(SIZE, || Ok(fake(42, &log))).unwrap();
        assert_eq!(registry.destroy(&old), Err(Error::StaleHandle));
        assert_eq!(registry.info(&new).unwrap().native_id, 42);
        let mut other = Registry::new();
        other.create(SIZE, || Ok(fake(42, &log))).unwrap();
        assert_eq!(other.destroy(&new), Err(Error::StaleHandle));
        drop(registry);
        assert_eq!(*log.borrow(), vec![42, 42]);
        assert_eq!(other.info(&new), Err(Error::StaleHandle));
    }

    #[test]
    fn failure_rolls_back_and_drop_releases_every_object_once() {
        let log = Rc::default();
        let mut registry = Registry::new();
        assert!(registry
            .create(SIZE, || Err::<Fake, _>(Error::NativeFailure("fixture")))
            .is_err());
        let handle = registry.create(SIZE, || Ok(fake(7, &log))).unwrap();
        for id in [0, 7] {
            assert_eq!(
                registry.create(SIZE, || Ok(fake(id, &log))).unwrap_err(),
                Error::InvalidNativeId
            );
        }
        registry.create(SIZE, || Ok(fake(8, &log))).unwrap();
        registry.destroy(&handle).unwrap();
        assert_eq!(registry.destroy(&handle), Err(Error::StaleHandle));
        drop(registry);
        assert_eq!(*log.borrow(), vec![0, 7, 7, 8]);
    }

    #[test]
    fn uncertain_teardown_invalidates_handle_and_disables_creation() {
        let log = Rc::default();
        let mut registry = Registry::new();
        let handle = registry
            .create(SIZE, || {
                let mut object = fake(9, &log);
                object.fail = true;
                Ok(object)
            })
            .unwrap();
        assert_eq!(registry.destroy(&handle), Err(Error::TeardownUnconfirmed));
        assert_eq!(registry.info(&handle), Err(Error::StaleHandle));
        assert_eq!(
            registry
                .create(SIZE, || panic!("native effect"))
                .unwrap_err(),
            Error::TeardownUnconfirmed
        );
        assert_eq!(*log.borrow(), vec![9]);
    }

    #[test]
    fn generation_exhaustion_never_wraps() {
        let mut registry = Registry::<Fake>::new();
        registry.next_generation = u64::MAX;
        assert_eq!(
            registry
                .create(SIZE, || panic!("native effect"))
                .unwrap_err(),
            Error::GenerationExhausted
        );
    }

    #[test]
    fn release_abi_preserves_nsobject_oneway_return_qualifier() {
        // NSObject.h declares -(oneway void)release. A plain void expectation
        // rejects a supported runtime before creation; do not erase qualifiers.
        let mut releases = 0;
        probe_abi(|m| {
            if m.selector == "release" {
                assert_eq!(m.types, "Vv|@|:");
                releases += 1;
                Ok("Vv|@|:".to_owned())
            } else {
                Ok(m.types.to_owned())
            }
        })
        .unwrap();
        assert_eq!(releases, 4);
        assert!(probe_abi(|m| Ok(if m.selector == "release" {
            "v|@|:".to_owned()
        } else {
            m.types.to_owned()
        }))
        .is_err());
    }

    #[test]
    fn abi_requires_every_class_selector_and_exact_signature() {
        let mut methods = Vec::new();
        probe_abi(|m| {
            methods.push((m.class, m.selector, m.class_method));
            Ok(m.types.to_owned())
        })
        .unwrap();
        // Fail each entry in turn: no skipped requirement and no runtime probe.
        for missing in methods {
            for replacement in [
                None,
                Some("@|@|:|Q"),
                Some("v|@|:|{CGSize=ff}"),
                Some("v|@|:|@?"),
            ] {
                let error = probe_abi(|m| {
                    if (m.class, m.selector, m.class_method) == missing {
                        replacement
                            .map(str::to_owned)
                            .ok_or_else(|| Error::UnavailableAbi("missing class/selector".into()))
                    } else {
                        Ok(m.types.to_owned())
                    }
                })
                .unwrap_err();
                assert!(matches!(error, Error::UnavailableAbi(_)));
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_platform_is_explicit() {
        assert!(matches!(
            VirtualDisplays::open(),
            Err(Error::UnsupportedPlatform)
        ));
    }
}
