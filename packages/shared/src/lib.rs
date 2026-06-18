//! shared::shm — FROZEN cross-process contract for the IVSHMEM audio bridge.
//!
//! Both `lalah-vm` (Windows producer) and `lalah-host` (Linux consumer) compile
//! against THIS file verbatim. The layout is byte-identical on
//! `x86_64-pc-windows-msvc` and `x86_64-unknown-linux-gnu`. Do NOT reorder fields
//! or change padding without bumping [`SHM_VERSION`].
//!
//! # Soundness model
//!
//! The region is mapped (IVSHMEM on the guest, a memory-backend-file mmap on the
//! host) and mutated by ANOTHER process — possibly another OS. Therefore:
//!
//!  * We NEVER form a `&[u8]` / `&mut [u8]` over the payload, nor a long-lived
//!    `&mut` over the header. The payload is touched only through raw pointers
//!    with volatile element copies; the header only through its atomic fields.
//!  * `capacity` is the single authoritative ring size (a power of two), so the
//!    mask arithmetic is identical on both sides; it is validated on attach. No
//!    `data.len()`-vs-`capacity` divergence is possible.
//!  * `head` and `tail`/`seq` each own their own 64-byte cache line; the config
//!    fields are read-mostly on line 0. This avoids producer/consumer false
//!    sharing.
//!  * A producer seqlock (`seq` even/odd) plus a consumer post-copy recheck of
//!    `tail`/`seq` eliminate torn reads and silent overrun. All offsets are
//!    frame-aligned so a PCM sample is never split (no clicks).
//!  * `magic`/`version`/`header_size`/`capacity` are validated on attach, so a
//!    layout or size skew fails loudly instead of producing garbage audio.
//!  * init/attach split: the host initializes the header with the format UNSET;
//!    the producer publishes the negotiated format on WASAPI start; the consumer
//!    waits for a valid format before configuring PipeWire.

#![allow(clippy::missing_safety_doc)]

use core::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

/// Magic value ("LALAHSM1") published last by the host initializer.
pub const SHM_MAGIC: u64 = 0x4C41_4C41_4853_4D31;
/// Layout version. Bump on ANY change to [`ShmHeader`] or the ring semantics.
pub const SHM_VERSION: u32 = 1;

/// PCM sample format. `repr(u32)` so it round-trips through the atomic field.
///
/// All formats are little-endian (both sides are x86_64); no byte swapping ever
/// happens — bytes flow straight from WASAPI `GetBuffer` through the ring into
/// the PipeWire buffer.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleFormat {
    S16Le = 0,
    S32Le = 1,
    F32Le = 2,
}

impl SampleFormat {
    #[inline]
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(SampleFormat::S16Le),
            1 => Some(SampleFormat::S32Le),
            2 => Some(SampleFormat::F32Le),
            _ => None,
        }
    }

    #[inline]
    pub fn bytes_per_sample(self) -> u32 {
        match self {
            SampleFormat::S16Le => 2,
            SampleFormat::S32Le => 4,
            SampleFormat::F32Le => 4,
        }
    }
}

/// Snapshot of the negotiated audio format, read out of the header.
#[derive(Clone, Copy, Debug)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u32,
    pub format: SampleFormat,
    /// `channels * bytes_per_sample` (== WASAPI `nBlockAlign`). The alignment
    /// unit for every ring read/write.
    pub frame_bytes: u32,
}

/// The FROZEN header. `repr(C, align(64))`, total size 192 = three 64-byte lines.
///
/// * Line 0 (offset 0): read-mostly config, written once by the host.
/// * Line 1 (offset 64): producer-owned (`tail` + `seq`).
/// * Line 2 (offset 128): consumer-owned (`head`).
///
/// The payload follows immediately at offset `size_of::<ShmHeader>()` = 192.
#[repr(C, align(64))]
pub struct ShmHeader {
    // ---- line 0: read-mostly config, written once by the host init ----
    /// [`SHM_MAGIC`]; published last (Release), validated first (Acquire).
    pub magic: AtomicU64, // off 0
    /// [`SHM_VERSION`].
    pub version: AtomicU32, // off 8
    /// `size_of::<ShmHeader>()` == 192; ABI guard against padding/toolchain skew.
    pub header_size: AtomicU32, // off 12
    /// Authoritative ring size in bytes; MUST be a power of two.
    pub capacity: AtomicU64, // off 16
    /// e.g. 48000. `0` means format-unset (the host-init sentinel).
    pub sample_rate: AtomicU32, // off 24
    /// e.g. 2. Stored as u32 for alignment (WASAPI `nChannels` is u16, widened).
    pub channels: AtomicU32, // off 28
    /// [`SampleFormat`] as u32.
    pub format: AtomicU32, // off 32
    /// `channels * bytes_per_sample`. `0` is the "format not yet published" gate.
    pub frame_bytes: AtomicU32, // off 36
    _pad0: [u8; 24], // off 40 -> 64

    // ---- line 1: producer-owned ----
    /// Monotonic count of bytes ever written; published with Release.
    pub tail: AtomicU64, // off 64
    /// Seqlock counter: even = stable, odd = writing.
    pub seq: AtomicU64, // off 72
    _pad1: [u8; 48], // off 80 -> 128

    // ---- line 2: consumer-owned ----
    /// Monotonic count of bytes ever consumed; published with Release.
    pub head: AtomicU64, // off 128
    _pad2: [u8; 56], // off 136 -> 192
}

const _: () = assert!(core::mem::size_of::<ShmHeader>() == 192);
const _: () = assert!(core::mem::align_of::<ShmHeader>() == 64);

/// Cross-process ring handle. Holds RAW POINTERS only — never a `&[u8]` /
/// `&mut [u8]` spanning shared memory. One attach point per process; used
/// single-threaded per process (the handle may be *moved* to the audio thread).
pub struct ShmAudioBuffer {
    hdr: *const ShmHeader, // header touched only via its atomic fields
    payload: *mut u8,      // payload touched only via volatile copies
    cap: usize,            // == capacity (validated); power of two
}

// The pointers live in shared memory that outlives the handle. The handle may be
// moved between threads on one side, but is only ever used by one thread at a
// time, so it is `Send` but not `Sync`.
unsafe impl Send for ShmAudioBuffer {}

impl ShmAudioBuffer {
    /// HOST INIT. Call EXACTLY ONCE, before the producer attaches.
    ///
    /// Writes the config and zeroes the counters, leaving the FORMAT UNSET
    /// (`sample_rate`/`channels`/`format`/`frame_bytes` = 0); the producer
    /// publishes the real format later via [`set_format`](Self::set_format).
    ///
    /// `cap` MUST be a power of two and `<= total_size - size_of::<ShmHeader>()`.
    ///
    /// # Safety
    /// `ptr` must point at a writable mapping of at least `total_size` bytes that
    /// stays valid and stable for the returned handle's lifetime, and no other
    /// process may be attached yet.
    pub unsafe fn init(ptr: *mut u8, total_size: usize, cap: usize) -> Self {
        let hs = core::mem::size_of::<ShmHeader>();
        assert!(total_size > hs, "shm region too small for header");
        assert!(cap.is_power_of_two(), "capacity must be a power of two");
        assert!(cap <= total_size - hs, "capacity exceeds available payload");

        let h = unsafe { &*(ptr as *const ShmHeader) };
        // Config + counters first (Relaxed); magic publishes last with Release.
        h.header_size.store(hs as u32, Ordering::Relaxed);
        h.capacity.store(cap as u64, Ordering::Relaxed);
        h.sample_rate.store(0, Ordering::Relaxed); // UNSET
        h.channels.store(0, Ordering::Relaxed); // UNSET
        h.format.store(0, Ordering::Relaxed);
        h.frame_bytes.store(0, Ordering::Relaxed); // 0 => format-unset sentinel
        h.head.store(0, Ordering::Relaxed);
        h.tail.store(0, Ordering::Relaxed);
        h.seq.store(0, Ordering::Relaxed);
        h.version.store(SHM_VERSION, Ordering::Relaxed);
        h.magic.store(SHM_MAGIC, Ordering::Release); // publish LAST

        Self {
            hdr: ptr as *const ShmHeader,
            payload: unsafe { ptr.add(hs) },
            cap,
        }
    }

    /// ATTACH (either side). Validates `magic`/`version`/`header_size`/`capacity`.
    /// Does NOT require the format to be set yet — poll [`format`](Self::format).
    ///
    /// # Safety
    /// Same mapping requirements as [`init`](Self::init); the region must have
    /// already been initialized by the host.
    pub unsafe fn attach(ptr: *mut u8, total_size: usize) -> Result<Self, &'static str> {
        let hs = core::mem::size_of::<ShmHeader>();
        if total_size <= hs {
            return Err("region smaller than header");
        }
        let h = unsafe { &*(ptr as *const ShmHeader) };
        if h.magic.load(Ordering::Acquire) != SHM_MAGIC {
            return Err("bad magic (region not initialized yet)");
        }
        if h.version.load(Ordering::Relaxed) != SHM_VERSION {
            return Err("version mismatch");
        }
        if h.header_size.load(Ordering::Relaxed) as usize != hs {
            return Err("ABI/header_size mismatch");
        }
        let cap = h.capacity.load(Ordering::Relaxed) as usize;
        if cap == 0 || !cap.is_power_of_two() || cap > total_size - hs {
            return Err("bad capacity");
        }
        Ok(Self {
            hdr: ptr as *const ShmHeader,
            payload: unsafe { ptr.add(hs) },
            cap,
        })
    }

    #[inline]
    fn h(&self) -> &ShmHeader {
        // Safe: `hdr` points at a live, aligned ShmHeader for the handle's
        // lifetime, and we only ever touch its atomic fields.
        unsafe { &*self.hdr }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Returns the current (head, tail) pointer values of the ring buffer.
    #[inline]
    pub fn pointers(&self) -> (u64, u64) {
        let h = self.h();
        (h.head.load(Ordering::Relaxed), h.tail.load(Ordering::Acquire))
    }

    /// PRODUCER: publish the negotiated format ONCE, on WASAPI start.
    ///
    /// `frame_bytes` is published LAST (Release) — it is the consumer's
    /// readiness gate.
    pub fn set_format(&self, fmt: AudioFormat) {
        let h = self.h();
        h.sample_rate.store(fmt.sample_rate, Ordering::Relaxed);
        h.channels.store(fmt.channels, Ordering::Relaxed);
        h.format.store(fmt.format as u32, Ordering::Relaxed);
        h.frame_bytes.store(fmt.frame_bytes, Ordering::Release); // gate, last
    }

    /// CONSUMER: read the format if the producer has published it
    /// (`frame_bytes != 0`), else `None`.
    pub fn format(&self) -> Option<AudioFormat> {
        let h = self.h();
        let frame_bytes = h.frame_bytes.load(Ordering::Acquire); // gate, first
        if frame_bytes == 0 {
            return None;
        }
        let format = SampleFormat::from_u32(h.format.load(Ordering::Relaxed))?;
        Some(AudioFormat {
            sample_rate: h.sample_rate.load(Ordering::Relaxed),
            channels: h.channels.load(Ordering::Relaxed),
            format,
            frame_bytes,
        })
    }

    #[inline]
    fn mask(&self, pos: u64) -> usize {
        (pos as usize) & (self.cap - 1)
    }

    /// PRODUCER (VM). Overwrite-newest semantics. If `src` is longer than the
    /// ring, only the newest `cap` bytes survive. The seqlock plus the Release
    /// store of `tail` guard the consumer against torn reads.
    pub fn push_overwrite(&mut self, src: &[u8]) {
        let h = self.h();
        let src = if src.len() > self.cap {
            &src[src.len() - self.cap..] // keep the newest cap bytes
        } else {
            src
        };
        let n = src.len();
        if n == 0 {
            return;
        }
        let tail = h.tail.load(Ordering::Relaxed);

        // Enter the critical section: seq -> odd, ordered before payload writes.
        let s = h.seq.load(Ordering::Relaxed);
        h.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);

        let start = self.mask(tail);
        unsafe {
            if start + n <= self.cap {
                volatile_copy_to(self.payload, start, src);
            } else {
                let first = self.cap - start;
                volatile_copy_to(self.payload, start, &src[..first]);
                volatile_copy_to(self.payload, 0, &src[first..]);
            }
        }

        // Publish tail (Release), then close the seqlock (-> even).
        h.tail.store(tail.wrapping_add(n as u64), Ordering::Release);
        fence(Ordering::Release);
        h.seq.store(s.wrapping_add(2), Ordering::Relaxed);
    }

    /// CONSUMER (host). Fixed-quantum, frame-aligned, realtime-safe (no alloc,
    /// no lock, no syscall) "play newest" read with bounded latency.
    ///
    /// Writes whole frames into `out`; any tail of `out` beyond live data is
    /// zero-filled (silence, never stale bytes). Returns the number of LIVE
    /// bytes written (a multiple of `frame_bytes`).
    ///
    /// `max_latency_bytes`: if the backlog exceeds this, `head` jumps forward to
    /// the newest window (frame-aligned) so end-to-end latency stays bounded at
    /// the cost of dropping the oldest audio.
    pub fn read_quantum(&mut self, out: &mut [u8], max_latency_bytes: u64) -> usize {
        let h = self.h();
        let frame = h.frame_bytes.load(Ordering::Relaxed).max(1) as u64;
        let quantum = (out.len() as u64 / frame) * frame; // frame-aligned request

        loop {
            let s0 = h.seq.load(Ordering::Acquire);
            if s0 & 1 == 1 {
                core::hint::spin_loop();
                continue; // writer mid-write
            }
            let tail = h.tail.load(Ordering::Acquire);
            let mut head = h.head.load(Ordering::Relaxed);
            if head > tail {
                head = tail; // desync guard (saturate)
            }
            let avail = tail - head;

            // Bounded latency: keep only the newest `budget`, frame-aligned.
            let budget = (max_latency_bytes / frame) * frame;
            if avail > budget {
                head = tail - budget;
            }

            let want = quantum.min(tail - head);
            let want = (want / frame) * frame; // frame-aligned
            let wu = want as usize;

            let start = self.mask(head);
            unsafe {
                if start + wu <= self.cap {
                    volatile_copy_from(self.payload, start, &mut out[..wu]);
                } else {
                    let first = self.cap - start;
                    volatile_copy_from(self.payload, start, &mut out[..first]);
                    volatile_copy_from(self.payload, 0, &mut out[first..wu]);
                }
            }

            // Overrun / torn recheck: the writer must not have lapped us, and
            // the seqlock must be unchanged & even.
            let tail2 = h.tail.load(Ordering::Acquire);
            let s1 = h.seq.load(Ordering::Acquire);
            if s1 != s0 || tail2.wrapping_sub(head) > self.cap as u64 {
                continue; // retry the whole read
            }

            h.head.store(head + want, Ordering::Release);
            for b in &mut out[wu..] {
                *b = 0; // silence the remainder
            }
            return wu;
        }
    }
}

/// Volatile element copy into the payload. Legal with respect to a foreign writer
/// and uncacheable by the compiler. NOTE: `copy_nonoverlapping` is NOT volatile,
/// so it must not be used here. Per-byte is the correct-but-slow baseline; it can
/// be replaced with chunked volatile word copies for throughput without changing
/// the semantics.
#[inline]
unsafe fn volatile_copy_to(base: *mut u8, off: usize, src: &[u8]) {
    let dst = unsafe { base.add(off) };
    for (i, &b) in src.iter().enumerate() {
        unsafe { core::ptr::write_volatile(dst.add(i), b) };
    }
}

/// Volatile element copy out of the payload. See [`volatile_copy_to`].
#[inline]
unsafe fn volatile_copy_from(base: *const u8, off: usize, dst: &mut [u8]) {
    let s = unsafe { base.add(off) };
    for (i, b) in dst.iter_mut().enumerate() {
        *b = unsafe { core::ptr::read_volatile(s.add(i)) };
    }
}

// ======================= VIDEO (experimental passthrough) ====================
//
// Optional second sub-region for forwarding raw video frames VM -> host. It is
// SELF-LOCATING and SELF-DESCRIBING: when video is enabled, audio is pinned to
// the first [`AUDIO_REGION_BYTES`] of the mapping and the video region is
// everything after it, beginning with its own [`ShmVideoHeader`] (its own magic /
// version, independent of the audio header — enabling video does NOT change the
// audio ABI). When video is disabled, none of this is touched and the audio
// layout is byte-identical to a video-unaware build.
//
// Tearing is handled structurally, not by timing: the payload is an N-slot frame
// ring. The producer writes slots round-robin and publishes a monotonic frame
// count (Release); the consumer reads the newest slot and re-checks the count
// (Acquire) — if the producer advanced by >= N-1 frames during the copy, the
// slot may have been lapped, so the frame is reported torn and the consumer drops
// it. With N >= 3 the writer and reader are never on the same slot in practice.
// The producer NEVER reads consumer state (no backpressure) — same contract as
// the audio ring.

/// When video is enabled, audio occupies the first 16 MiB of the mapping and the
/// video region is everything after it. Both sides agree on this split by this
/// constant alone, so the video region needs no pointer stored in the audio
/// header.
pub const AUDIO_REGION_BYTES: usize = 16 * 1024 * 1024;

/// Magic for the video sub-region header ("LALAHVD1"), independent of
/// [`SHM_MAGIC`]. Published last by the host initializer.
pub const VIDEO_MAGIC: u64 = 0x4C41_4C41_5648_4431;
/// Video sub-region layout version. Bump on any change to [`ShmVideoHeader`].
pub const VIDEO_VERSION: u32 = 1;
/// Minimum frame slots for the tear-safe ring: the writer's slot, a one-slot
/// separation margin, and the reader's slot.
pub const VIDEO_MIN_SLOTS: u32 = 3;

/// Snapshot of the published video geometry. `fourcc` is the DirectShow
/// `biCompression` (e.g. `YUY2`); `frame_size` is `biSizeImage` (the slot size).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    /// Bytes per row (0 if the producer could not determine it).
    pub stride: u32,
    /// `biCompression` FourCC, or 0 for an RGB/uncompressed `BI_RGB` frame.
    pub fourcc: u32,
    /// Bytes per frame == the ring slot's used length (`biSizeImage`).
    pub frame_size: u32,
}

/// FROZEN video header. `repr(C, align(64))`, total 128 = two 64-byte lines.
///
/// * Line 0: config (host init) + geometry (producer publishes once).
/// * Line 1: producer-owned `frame_seq` (monotonic count of completed frames).
///
/// Frame slots follow at offset `size_of::<ShmVideoHeader>()` (already 64-aligned),
/// each `align_up_64(frame_size)` bytes wide.
#[repr(C, align(64))]
pub struct ShmVideoHeader {
    // ---- line 0: config + geometry ----
    /// [`VIDEO_MAGIC`]; published last (Release), validated first (Acquire).
    magic: AtomicU64, // off 0
    /// [`VIDEO_VERSION`].
    version: AtomicU32, // off 8
    /// `size_of::<ShmVideoHeader>()` == 128; ABI guard.
    header_size: AtomicU32, // off 12
    width: AtomicU32,   // off 16
    height: AtomicU32,  // off 20
    stride: AtomicU32,  // off 24
    fourcc: AtomicU32,  // off 28
    /// Bytes per frame; `0` is the "geometry not yet published" gate.
    frame_size: AtomicU32, // off 32
    /// Number of frame slots (computed by the producer at publish time).
    slot_count: AtomicU32, // off 36
    _vpad0: [u8; 24], // off 40 -> 64
    // ---- line 1: producer-owned ----
    /// Monotonic count of completed frames; slot of frame `f` is `f % slot_count`.
    frame_seq: AtomicU64, // off 64
    _vpad1: [u8; 56], // off 72 -> 128
}

const _: () = assert!(core::mem::size_of::<ShmVideoHeader>() == 128);
const _: () = assert!(core::mem::align_of::<ShmVideoHeader>() == 64);

/// Round `n` up to a multiple of 64 (so every slot starts 64-byte aligned and the
/// word-wise volatile copies below stay aligned).
#[inline]
const fn align_up_64(n: usize) -> usize {
    (n + 63) & !63
}

/// Given the whole mapping, return the video sub-region `(base, len)` — the bytes
/// after [`AUDIO_REGION_BYTES`] — or `None` if the mapping does not extend past
/// the audio region.
///
/// # Safety
/// `base` must point at a mapping of at least `total` bytes.
pub unsafe fn video_region(base: *mut u8, total: usize) -> Option<(*mut u8, usize)> {
    if total <= AUDIO_REGION_BYTES + core::mem::size_of::<ShmVideoHeader>() {
        return None;
    }
    Some((unsafe { base.add(AUDIO_REGION_BYTES) }, total - AUDIO_REGION_BYTES))
}

/// Cross-process handle to the video sub-region. Like [`ShmAudioBuffer`], holds
/// RAW POINTERS only and is `Send` but not `Sync` (one thread per side).
pub struct ShmVideoBuffer {
    hdr: *const ShmVideoHeader,
    payload: *mut u8,
    region_len: usize,
    // Cached geometry (0 until published / refreshed). Both sides derive
    // `slot_stride` from `frame_size` identically, so the modulo math matches.
    frame_size: usize,
    slot_stride: usize,
    slot_count: usize,
}

unsafe impl Send for ShmVideoBuffer {}

impl ShmVideoBuffer {
    /// HOST INIT. Publish magic/version with the geometry UNSET; the producer
    /// publishes the real geometry later via [`set_geometry`](Self::set_geometry).
    ///
    /// # Safety
    /// `base` must point at a writable mapping of at least `region_len` bytes that
    /// stays valid for the handle's lifetime, and no producer may be attached yet.
    pub unsafe fn init(base: *mut u8, region_len: usize) -> Self {
        let hs = core::mem::size_of::<ShmVideoHeader>();
        assert!(region_len > hs, "video region too small for header");
        let h = unsafe { &*(base as *const ShmVideoHeader) };
        h.header_size.store(hs as u32, Ordering::Relaxed);
        h.width.store(0, Ordering::Relaxed);
        h.height.store(0, Ordering::Relaxed);
        h.stride.store(0, Ordering::Relaxed);
        h.fourcc.store(0, Ordering::Relaxed);
        h.slot_count.store(0, Ordering::Relaxed);
        h.frame_seq.store(0, Ordering::Relaxed);
        h.frame_size.store(0, Ordering::Relaxed); // gate: geometry unset
        h.version.store(VIDEO_VERSION, Ordering::Relaxed);
        h.magic.store(VIDEO_MAGIC, Ordering::Release); // publish LAST
        Self {
            hdr: base as *const ShmVideoHeader,
            payload: unsafe { base.add(hs) },
            region_len,
            frame_size: 0,
            slot_stride: 0,
            slot_count: 0,
        }
    }

    /// ATTACH (producer). Validates magic/version/header_size; the geometry may
    /// still be unset (the producer is the one who publishes it).
    ///
    /// # Safety
    /// Same mapping requirements as [`init`](Self::init); the region must already
    /// have been initialized by the host.
    pub unsafe fn attach(base: *mut u8, region_len: usize) -> Result<Self, &'static str> {
        let hs = core::mem::size_of::<ShmVideoHeader>();
        if region_len <= hs {
            return Err("video region smaller than header");
        }
        let h = unsafe { &*(base as *const ShmVideoHeader) };
        if h.magic.load(Ordering::Acquire) != VIDEO_MAGIC {
            return Err("bad video magic (region not initialized yet)");
        }
        if h.version.load(Ordering::Relaxed) != VIDEO_VERSION {
            return Err("video version mismatch");
        }
        if h.header_size.load(Ordering::Relaxed) as usize != hs {
            return Err("video ABI/header_size mismatch");
        }
        let mut me = Self {
            hdr: base as *const ShmVideoHeader,
            payload: unsafe { base.add(hs) },
            region_len,
            frame_size: 0,
            slot_stride: 0,
            slot_count: 0,
        };
        me.refresh_geometry();
        Ok(me)
    }

    #[inline]
    fn h(&self) -> &ShmVideoHeader {
        unsafe { &*self.hdr }
    }

    /// PRODUCER: publish the geometry ONCE, on capture start. Computes and stores
    /// the slot count for the actual region size; errors if fewer than
    /// [`VIDEO_MIN_SLOTS`] frames fit. `frame_size` is published LAST (Release) —
    /// the consumer's readiness gate.
    pub fn set_geometry(&mut self, vf: VideoFormat) -> Result<u32, &'static str> {
        if vf.frame_size == 0 {
            return Err("zero frame_size");
        }
        let hs = core::mem::size_of::<ShmVideoHeader>();
        let slot_stride = align_up_64(vf.frame_size as usize);
        let usable = self.region_len - hs;
        let slot_count = usable / slot_stride;
        if (slot_count as u32) < VIDEO_MIN_SLOTS {
            return Err("video region too small for >= 3 frame slots");
        }
        let slot_count = slot_count as u32;
        self.frame_size = vf.frame_size as usize;
        self.slot_stride = slot_stride;
        self.slot_count = slot_count as usize;

        let h = self.h();
        h.width.store(vf.width, Ordering::Relaxed);
        h.height.store(vf.height, Ordering::Relaxed);
        h.stride.store(vf.stride, Ordering::Relaxed);
        h.fourcc.store(vf.fourcc, Ordering::Relaxed);
        h.slot_count.store(slot_count, Ordering::Relaxed);
        h.frame_seq.store(0, Ordering::Relaxed); // fresh stream
        h.frame_size.store(vf.frame_size, Ordering::Release); // gate, LAST
        Ok(slot_count)
    }

    /// CONSUMER/PRODUCER: read the published geometry into the handle's cache and
    /// return it, or `None` if the producer has not published yet
    /// (`frame_size == 0`).
    pub fn refresh_geometry(&mut self) -> Option<VideoFormat> {
        // Read everything out first so the immutable header borrow ends before we
        // update the cached fields.
        let (fs, slot_count, width, height, stride, fourcc) = {
            let h = self.h();
            let fs = h.frame_size.load(Ordering::Acquire); // gate, first
            if fs == 0 {
                return None;
            }
            (
                fs,
                h.slot_count.load(Ordering::Relaxed) as usize,
                h.width.load(Ordering::Relaxed),
                h.height.load(Ordering::Relaxed),
                h.stride.load(Ordering::Relaxed),
                h.fourcc.load(Ordering::Relaxed),
            )
        };
        self.frame_size = fs as usize;
        self.slot_stride = align_up_64(fs as usize);
        self.slot_count = slot_count;
        Some(VideoFormat {
            width,
            height,
            stride,
            fourcc,
            frame_size: fs,
        })
    }

    /// Monotonic count of completed frames (Acquire). Used by the consumer to
    /// detect a freshly published frame before applying its read stagger.
    #[inline]
    pub fn frame_count(&self) -> u64 {
        self.h().frame_seq.load(Ordering::Acquire)
    }

    /// PRODUCER. Write one frame into the next ring slot and publish it. Copies at
    /// most `frame_size` bytes (zero-filling a short source); a longer source is
    /// truncated. NEVER blocks and never reads consumer state.
    pub fn push_frame(&mut self, src: &[u8]) {
        if self.frame_size == 0 || self.slot_count == 0 {
            return;
        }
        let h = self.h();
        let seq = h.frame_seq.load(Ordering::Relaxed);
        let slot = (seq % self.slot_count as u64) as usize;
        let off = slot * self.slot_stride;
        unsafe {
            volatile_write_frame(self.payload, off, src, self.frame_size);
        }
        // Publish: ensure the slot writes are visible before the count bump.
        fence(Ordering::Release);
        h.frame_seq.store(seq.wrapping_add(1), Ordering::Release);
    }

    /// CONSUMER. Copy the NEWEST published frame into `out` (which must be at least
    /// `frame_size` bytes). Returns `Some(frame_number)` on a tear-safe read, or
    /// `None` if no frame exists yet or the writer may have lapped the slot during
    /// the copy (the caller drops a torn frame).
    pub fn read_frame(&self, out: &mut [u8]) -> Option<u64> {
        if self.frame_size == 0 || self.slot_count == 0 {
            return None;
        }
        let h = self.h();
        let s0 = h.frame_seq.load(Ordering::Acquire);
        if s0 == 0 {
            return None; // no frame published yet
        }
        let newest = s0 - 1;
        let slot = (newest % self.slot_count as u64) as usize;
        let off = slot * self.slot_stride;
        let len = self.frame_size.min(out.len());
        unsafe {
            volatile_copy_from_words(self.payload, off, &mut out[..len]);
        }
        // Tear-safe iff the writer advanced < slot_count-1 frames during the copy,
        // i.e. it cannot have started rewriting our slot.
        let s1 = h.frame_seq.load(Ordering::Acquire);
        if s1.wrapping_sub(s0) <= (self.slot_count as u64).saturating_sub(2) {
            Some(newest)
        } else {
            None // possibly torn -> drop
        }
    }

    /// Cached frame size in bytes (0 until geometry is known).
    #[inline]
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }
}

/// Write exactly `frame_size` bytes into a frame slot with word-wise volatile
/// stores (8 bytes/op + byte tail), sourcing bytes from `src` and zero-filling
/// any remainder past `src.len()`. Faster than the per-byte audio copy, which
/// matters for multi-MB frames. The whole pass is anchored at the 64-aligned slot
/// base `base + off` (slots are `align_up_64`), so every `*mut u64` store is
/// aligned regardless of how short `src` is.
#[inline]
unsafe fn volatile_write_frame(base: *mut u8, off: usize, src: &[u8], frame_size: usize) {
    let dst = unsafe { base.add(off) };
    let n = src.len().min(frame_size);
    let words = frame_size / 8;
    let mut i = 0;
    while i < words {
        let start = i * 8;
        let mut b = [0u8; 8];
        if start < n {
            let avail = (n - start).min(8);
            b[..avail].copy_from_slice(&src[start..start + avail]);
        }
        unsafe { core::ptr::write_volatile(dst.add(start) as *mut u64, u64::from_ne_bytes(b)) };
        i += 1;
    }
    let mut j = words * 8;
    while j < frame_size {
        let byte = if j < n { src[j] } else { 0 };
        unsafe { core::ptr::write_volatile(dst.add(j), byte) };
        j += 1;
    }
}

/// Word-wise volatile copy out of the payload. See [`volatile_write_frame`].
#[inline]
unsafe fn volatile_copy_from_words(base: *const u8, off: usize, dst: &mut [u8]) {
    let s = unsafe { base.add(off) };
    let words = dst.len() / 8;
    let mut i = 0;
    while i < words {
        let w = unsafe { core::ptr::read_volatile(s.add(i * 8) as *const u64) };
        dst[i * 8..i * 8 + 8].copy_from_slice(&w.to_ne_bytes());
        i += 1;
    }
    let mut j = words * 8;
    while j < dst.len() {
        dst[j] = unsafe { core::ptr::read_volatile(s.add(j)) };
        j += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 1024;
    const TOTAL: usize = 192 + CAP;

    // 64-byte-aligned backing region so the ShmHeader alignment is satisfied.
    #[repr(C, align(64))]
    struct Region([u8; TOTAL]);

    fn fresh() -> (Box<Region>, ShmAudioBuffer) {
        let mut region = Box::new(Region([0u8; TOTAL]));
        let ptr = region.0.as_mut_ptr();
        let buf = unsafe { ShmAudioBuffer::init(ptr, TOTAL, CAP) };
        (region, buf)
    }

    fn fmt() -> AudioFormat {
        AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            format: SampleFormat::S16Le,
            frame_bytes: 4,
        }
    }

    #[test]
    fn header_layout_is_frozen() {
        assert_eq!(core::mem::size_of::<ShmHeader>(), 192);
        assert_eq!(core::mem::align_of::<ShmHeader>(), 64);
    }

    #[test]
    fn format_gate_unset_until_published() {
        let (_r, buf) = fresh();
        assert!(buf.format().is_none(), "format must be unset after init");
        buf.set_format(fmt());
        let f = buf.format().expect("format published");
        assert_eq!(f.sample_rate, 48_000);
        assert_eq!(f.channels, 2);
        assert_eq!(f.frame_bytes, 4);
        assert_eq!(f.format, SampleFormat::S16Le);
    }

    #[test]
    fn attach_validates_magic_and_capacity() {
        let (mut r, _buf) = fresh();
        let ptr = r.0.as_mut_ptr();
        assert!(unsafe { ShmAudioBuffer::attach(ptr, TOTAL) }.is_ok());

        // Corrupt the magic -> attach must refuse.
        let mut bad = Box::new(Region([0u8; TOTAL]));
        assert!(unsafe { ShmAudioBuffer::attach(bad.0.as_mut_ptr(), TOTAL) }.is_err());
    }

    #[test]
    fn push_then_read_roundtrips() {
        let (_r, mut buf) = fresh();
        buf.set_format(fmt());
        let data: Vec<u8> = (0..16).collect();
        buf.push_overwrite(&data);

        let mut out = [0u8; 16];
        let n = buf.read_quantum(&mut out, 1 << 20);
        assert_eq!(n, 16);
        assert_eq!(&out, data.as_slice());

        // Nothing new -> read returns 0 and zero-fills.
        let mut out2 = [0xAAu8; 16];
        let n2 = buf.read_quantum(&mut out2, 1 << 20);
        assert_eq!(n2, 0);
        assert!(out2.iter().all(|&b| b == 0), "remainder must be silence");
    }

    #[test]
    fn read_is_frame_aligned() {
        let (_r, mut buf) = fresh();
        buf.set_format(fmt()); // frame_bytes = 4
        buf.push_overwrite(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]); // 10 bytes = 2.5 frames

        // out holds 3 bytes (< 1 frame): nothing whole can be delivered.
        let mut tiny = [0u8; 3];
        assert_eq!(buf.read_quantum(&mut tiny, 1 << 20), 0);

        // out holds 7 bytes -> only 4 (one frame) delivered.
        let mut seven = [0u8; 7];
        let n = buf.read_quantum(&mut seven, 1 << 20);
        assert_eq!(n, 4);
        assert_eq!(&seven[..4], &[1, 2, 3, 4]);
    }

    #[test]
    fn wraps_around_ring_boundary() {
        let (_r, mut buf) = fresh();
        buf.set_format(fmt());
        // Advance tail near the end so the next write straddles the boundary.
        let filler = vec![0u8; CAP - 8];
        buf.push_overwrite(&filler);
        let mut drain = vec![0u8; CAP - 8];
        assert_eq!(buf.read_quantum(&mut drain, 1 << 24), CAP - 8);

        // This 16-byte write wraps: 8 bytes at the tail, 8 at the front.
        let payload: Vec<u8> = (100..116).collect();
        buf.push_overwrite(&payload);
        let mut out = [0u8; 16];
        let n = buf.read_quantum(&mut out, 1 << 24);
        assert_eq!(n, 16);
        assert_eq!(&out, payload.as_slice());
    }

    #[test]
    fn bounded_latency_drops_oldest() {
        let (_r, mut buf) = fresh();
        buf.set_format(fmt()); // frame_bytes = 4
        // Write 256 bytes (64 frames) but only allow ~16 bytes of backlog.
        let data: Vec<u8> = (0..=255).collect();
        buf.push_overwrite(&data);

        let mut out = [0u8; 64];
        let budget = 16u64; // 4 frames
        let n = buf.read_quantum(&mut out, budget);
        // Head jumps to tail-16, so we get the newest 16 bytes only.
        assert_eq!(n, 16);
        assert_eq!(&out[..16], &data[240..256]);
    }

    #[test]
    fn oversized_push_keeps_newest() {
        let (_r, mut buf) = fresh();
        buf.set_format(fmt());
        // 2*CAP bytes; only the newest CAP can survive in the ring.
        let data: Vec<u8> = (0..(2 * CAP)).map(|i| (i & 0xff) as u8).collect();
        buf.push_overwrite(&data);
        let mut out = vec![0u8; CAP];
        let n = buf.read_quantum(&mut out, (4 * CAP) as u64);
        assert_eq!(n, CAP);
        assert_eq!(&out[..], &data[CAP..]);
    }
}

#[cfg(test)]
mod video_tests {
    use super::*;

    const FRAME: usize = 64; // slot_stride == 64 (already 64-aligned)
    const SLOTS: usize = 4;
    const VLEN: usize = core::mem::size_of::<ShmVideoHeader>() + FRAME * SLOTS;

    #[repr(C, align(64))]
    struct VRegion([u8; VLEN]);

    fn fresh() -> (Box<VRegion>, ShmVideoBuffer) {
        let mut region = Box::new(VRegion([0u8; VLEN]));
        let ptr = region.0.as_mut_ptr();
        let buf = unsafe { ShmVideoBuffer::init(ptr, VLEN) };
        (region, buf)
    }

    fn vfmt() -> VideoFormat {
        VideoFormat {
            width: 8,
            height: 8,
            stride: 8,
            fourcc: u32::from_le_bytes(*b"YUY2"),
            frame_size: FRAME as u32,
        }
    }

    #[test]
    fn video_header_layout_is_frozen() {
        assert_eq!(core::mem::size_of::<ShmVideoHeader>(), 128);
        assert_eq!(core::mem::align_of::<ShmVideoHeader>(), 64);
    }

    #[test]
    fn geometry_gate_unset_until_published() {
        let (_r, mut buf) = fresh();
        assert!(buf.refresh_geometry().is_none(), "geometry must start unset");
        let n = buf.set_geometry(vfmt()).expect("publish geometry");
        assert_eq!(n, SLOTS as u32);
        let vf = buf.refresh_geometry().expect("geometry published");
        assert_eq!(vf, vfmt());
    }

    #[test]
    fn attach_validates_video_magic() {
        let (mut r, _buf) = fresh();
        let ptr = r.0.as_mut_ptr();
        assert!(unsafe { ShmVideoBuffer::attach(ptr, VLEN) }.is_ok());

        let mut bad = Box::new(VRegion([0u8; VLEN]));
        assert!(unsafe { ShmVideoBuffer::attach(bad.0.as_mut_ptr(), VLEN) }.is_err());
    }

    #[test]
    fn region_too_small_for_three_slots_is_rejected() {
        // Only room for 2 slots -> set_geometry must refuse.
        const SMALL: usize = core::mem::size_of::<ShmVideoHeader>() + FRAME * 2;
        #[repr(C, align(64))]
        struct Small([u8; SMALL]);
        let mut region = Box::new(Small([0u8; SMALL]));
        let mut buf = unsafe { ShmVideoBuffer::init(region.0.as_mut_ptr(), SMALL) };
        assert!(buf.set_geometry(vfmt()).is_err());
    }

    #[test]
    fn no_frame_before_push() {
        let (_r, mut buf) = fresh();
        buf.set_geometry(vfmt()).unwrap();
        let mut out = [0u8; FRAME];
        assert!(buf.read_frame(&mut out).is_none());
    }

    #[test]
    fn push_read_roundtrip_and_newest_wins() {
        let (_r, mut buf) = fresh();
        buf.set_geometry(vfmt()).unwrap();

        let f1: Vec<u8> = (0..FRAME).map(|i| i as u8).collect();
        buf.push_frame(&f1);
        let mut out = [0u8; FRAME];
        assert_eq!(buf.read_frame(&mut out), Some(0));
        assert_eq!(&out[..], &f1[..]);

        // Newest frame wins; older ones are skipped.
        for k in 1..=5u8 {
            let f: Vec<u8> = (0..FRAME).map(|i| i as u8 ^ (k * 17)).collect();
            buf.push_frame(&f);
        }
        let n = buf.read_frame(&mut out).expect("frame");
        assert_eq!(n, 5);
        let expect: Vec<u8> = (0..FRAME).map(|i| i as u8 ^ (5 * 17)).collect();
        assert_eq!(&out[..], &expect[..]);
    }

    #[test]
    fn short_source_is_zero_filled() {
        let (_r, mut buf) = fresh();
        buf.set_geometry(vfmt()).unwrap();
        buf.push_frame(&[0xAB; 10]); // shorter than FRAME
        let mut out = [0xFFu8; FRAME];
        assert_eq!(buf.read_frame(&mut out), Some(0));
        assert_eq!(&out[..10], &[0xAB; 10]);
        assert!(out[10..].iter().all(|&b| b == 0), "tail must be zero-filled");
    }

    #[test]
    fn slot_wraps_around_ring() {
        let (_r, mut buf) = fresh();
        buf.set_geometry(vfmt()).unwrap(); // SLOTS slots
        // Push more than SLOTS frames so the ring wraps; the newest must still
        // round-trip cleanly.
        for k in 0..(SLOTS as u8 * 3 + 1) {
            let f: Vec<u8> = (0..FRAME).map(|i| (i as u8).wrapping_add(k)).collect();
            buf.push_frame(&f);
        }
        let last = SLOTS as u8 * 3; // last k pushed
        let mut out = [0u8; FRAME];
        let n = buf.read_frame(&mut out).expect("frame");
        assert_eq!(n, (SLOTS as u64 * 3 + 1) - 1);
        let expect: Vec<u8> = (0..FRAME).map(|i| (i as u8).wrapping_add(last)).collect();
        assert_eq!(&out[..], &expect[..]);
    }
}
