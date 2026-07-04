//! VP8 encoder backed by libvpx via the `env-libvpx-sys` (`vpx_sys`) FFI.
//!
//! Wraps `vpx_codec_*` directly because the upstream `vpx-encode` crate
//! exposes neither `kf_max_dist` (so its keyframes are essentially scene-
//! change driven) nor `VPX_EFLAG_FORCE_KF` (so callers cannot trigger an
//! immediate keyframe when a new peer joins).  Both are required for the
//! display pipeline: the first to bound how long a fresh peer waits during
//! light desktop activity, the second to short-circuit the wait entirely
//! when the bridge knows a peer just attached.

// The real backend is libvpx via `vpx_sys` (env-libvpx-sys), which has no
// Windows build in Tier-0 (the crate is gated off `cfg(windows)` in
// Cargo.toml). Everything below is therefore compiled only on non-Windows;
// Windows gets the `Vp8Encoder` stub at the bottom of this file whose
// `new()` returns `Err`. The `mod vp8;` decl and `pub use vp8::Vp8Encoder`
// in `mod.rs` stay unconditional so call sites don't need their own cfgs.
#[cfg(not(target_os = "windows"))]
use super::{EncodedPacket, Encoder, PayloadSpec};
#[cfg(not(target_os = "windows"))]
use std::ffi::c_int;
#[cfg(not(target_os = "windows"))]
use std::mem::MaybeUninit;
#[cfg(not(target_os = "windows"))]
use std::ptr;
#[cfg(not(target_os = "windows"))]
use vpx_sys::*;

/// VP8 encoder configured for low-latency screen capture.
#[cfg(not(target_os = "windows"))]
pub struct Vp8Encoder {
    ctx: vpx_codec_ctx_t,
    width: usize,
    height: usize,
    pts_ms: i64,
    /// Cached canonical payload spec — VP8 has no fmtp variants, so this
    /// is the same for every packet and every VP8 encoder instance.
    /// Stored here so `payload_spec()` can return a reference without
    /// constructing on each call.
    payload_spec: PayloadSpec,
}

// `ctx` holds raw pointers from libvpx that aren't `Send`.  The encoder
// runs on a dedicated `std::thread` and is never shared, so transferring
// ownership across threads at construction time is safe.
#[cfg(not(target_os = "windows"))]
unsafe impl Send for Vp8Encoder {}

#[cfg(not(target_os = "windows"))]
impl Vp8Encoder {
    /// Build a VP8 encoder targeting `bitrate_kbps` at `width`×`height`.
    ///
    /// Configuration choices:
    /// * `kf_max_dist = 30` — caps the gap between keyframes at ~1s of
    ///   wall-clock encode at 30fps, so a peer that joins between forced
    ///   keyframes never waits longer than that for a decodable reference.
    /// * `g_threads = 4` — libvpx parallelises tile encode across threads;
    ///   4 is enough to keep up with 1080p30 on a typical multi-core box
    ///   without saturating the host.
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, String> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err("width and height must be even".to_string());
        }

        let iface = unsafe { vpx_codec_vp8_cx() };
        if iface.is_null() {
            return Err("vpx_codec_vp8_cx returned null".to_string());
        }

        let mut cfg: vpx_codec_enc_cfg_t = unsafe { MaybeUninit::zeroed().assume_init() };
        let err = unsafe { vpx_codec_enc_config_default(iface, &mut cfg, 0) };
        if err != VPX_CODEC_OK {
            return Err(format!("vpx_codec_enc_config_default: {err:?}"));
        }

        cfg.g_w = width;
        cfg.g_h = height;
        cfg.g_timebase.num = 1;
        cfg.g_timebase.den = 1000;
        cfg.rc_target_bitrate = bitrate_kbps;
        cfg.g_threads = 4;
        cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;
        cfg.kf_min_dist = 0;
        cfg.kf_max_dist = 30;

        let mut ctx: vpx_codec_ctx_t = unsafe { MaybeUninit::zeroed().assume_init() };
        let err = unsafe {
            vpx_codec_enc_init_ver(&mut ctx, iface, &cfg, 0, VPX_ENCODER_ABI_VERSION as i32)
        };
        if err != VPX_CODEC_OK {
            return Err(format!("vpx_codec_enc_init_ver: {err:?}"));
        }

        // Real-time CPU usage tradeoff: higher = faster encode, lower
        // quality.  6 (the libvpx maximum for VP8) matches what the
        // upstream `vpx-encode` crate uses for VP9.
        let err = unsafe {
            vpx_codec_control_(
                &mut ctx,
                vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int,
                6 as c_int,
            )
        };
        if err != VPX_CODEC_OK {
            eprintln!("[display/encode/vp8] WARN: VP8 realtime CPU tuning failed: {err:?}");
        }

        Ok(Self {
            ctx,
            width: width as usize,
            height: height as usize,
            pts_ms: 0,
            payload_spec: PayloadSpec::vp8(),
        })
    }
}

#[cfg(not(target_os = "windows"))]
impl Encoder for Vp8Encoder {
    fn encode(
        &mut self,
        i420: &[u8],
        duration_ms: u64,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedPacket>, String> {
        let y_size = self.width * self.height;
        let uv_size = self.width.div_ceil(2) * self.height.div_ceil(2);
        let expected = y_size + 2 * uv_size;
        if i420.len() < expected {
            return Err(format!(
                "I420 buffer too small: {} < {}",
                i420.len(),
                expected,
            ));
        }

        let mut image: vpx_image_t = unsafe { MaybeUninit::zeroed().assume_init() };
        let wrap = unsafe {
            vpx_img_wrap(
                &mut image,
                vpx_img_fmt::VPX_IMG_FMT_I420,
                self.width as u32,
                self.height as u32,
                1,
                i420.as_ptr() as *mut _,
            )
        };
        if wrap.is_null() {
            return Err("vpx_img_wrap returned null".to_string());
        }

        let pts = self.pts_ms;
        self.pts_ms += duration_ms as i64;

        let flags: i64 = if force_keyframe {
            VPX_EFLAG_FORCE_KF as i64
        } else {
            0
        };

        let err = unsafe {
            vpx_codec_encode(
                &mut self.ctx,
                &image,
                pts,
                duration_ms,
                flags,
                VPX_DL_REALTIME as u64,
            )
        };
        if err != VPX_CODEC_OK {
            return Err(format!("vpx_codec_encode: {err:?}"));
        }

        let mut out = Vec::new();
        let mut iter: vpx_codec_iter_t = ptr::null();
        loop {
            let pkt = unsafe { vpx_codec_get_cx_data(&mut self.ctx, &mut iter) };
            if pkt.is_null() {
                break;
            }
            let pkt_ref = unsafe { &*pkt };
            if pkt_ref.kind != vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                continue;
            }
            let frame = unsafe { &pkt_ref.data.frame };
            let data =
                unsafe { std::slice::from_raw_parts(frame.buf as *const u8, frame.sz as usize) }
                    .to_vec();
            out.push(EncodedPacket {
                data,
                pts_ms: frame.pts as u64,
                duration_ms,
                is_keyframe: (frame.flags & VPX_FRAME_IS_KEY) != 0,
                payload_spec: self.payload_spec.clone(),
            });
        }
        Ok(out)
    }

    fn codec_mime(&self) -> &'static str {
        "video/VP8"
    }

    fn payload_spec(&self) -> &PayloadSpec {
        &self.payload_spec
    }
}

#[cfg(not(target_os = "windows"))]
impl Drop for Vp8Encoder {
    fn drop(&mut self) {
        let err = unsafe { vpx_codec_destroy(&mut self.ctx) };
        if err != VPX_CODEC_OK {
            eprintln!("[display/encode/vp8] vpx_codec_destroy: {err:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Windows stub
// ---------------------------------------------------------------------------

/// Windows placeholder for the libvpx-backed VP8 encoder.
///
/// libvpx (`env-libvpx-sys`) is deferred on Windows (Tier-0), so there is
/// no software VP8 backend yet. The struct is uninhabitable in practice:
/// [`Vp8Encoder::new`] always returns `Err`, so the `Encoder` impl below
/// is never actually invoked. It exists only so `Box<dyn Encoder>` call
/// sites and `pub use vp8::Vp8Encoder` keep type-checking on Windows.
#[cfg(target_os = "windows")]
pub struct Vp8Encoder {
    payload_spec: super::PayloadSpec,
}

#[cfg(target_os = "windows")]
impl Vp8Encoder {
    /// Always fails on Windows — no libvpx backend in Tier-0.
    pub fn new(_width: u32, _height: u32, _bitrate_kbps: u32) -> Result<Self, String> {
        Err("VP8 (libvpx) encoding is not yet implemented on Windows".to_string())
    }
}

#[cfg(target_os = "windows")]
impl super::Encoder for Vp8Encoder {
    fn encode(
        &mut self,
        _i420: &[u8],
        _duration_ms: u64,
        _force_keyframe: bool,
    ) -> Result<Vec<super::EncodedPacket>, String> {
        // Unreachable: no instance can be constructed (see `new`). Return
        // an error rather than panicking to honor the no-panic contract.
        Err("VP8 encoding is not available on Windows".to_string())
    }

    fn codec_mime(&self) -> &'static str {
        super::MIME_TYPE_VP8
    }

    fn payload_spec(&self) -> &super::PayloadSpec {
        &self.payload_spec
    }
}
