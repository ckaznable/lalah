//! lalah-host — the Linux consumer.
//!
//! Maps the IVSHMEM region (exposed on the host as a QEMU memory-backend-file),
//! initializes the shared ring, waits for the VM to publish the negotiated audio
//! format, then streams the audio to PipeWire with bounded latency.
//!
//! Startup order: run THIS first (it initializes the header), then start the VM
//! and `lalah-vm` (which attach-retries until the magic is valid).

#[cfg(target_os = "linux")]
mod playback;
#[cfg(target_os = "linux")]
mod video;

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("lalah-host is Linux-only (it outputs via PipeWire).");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use crate::{playback, video};
    use memmap2::MmapOptions;
    use shared::{ShmAudioBuffer, ShmHeader, ShmVideoBuffer};
    use std::fs::OpenOptions;
    use std::time::Duration;

    const DEFAULT_SHM: &str = "/dev/shm/lalah";
    const DEFAULT_LATENCY_MS: u32 = 20;
    const DEFAULT_QUANTUM: u32 = 256;
    const DEFAULT_VIDEO_READ_DELAY_US: u64 = 500;
    const DEFAULT_VIDEO_PIPE_SIZE: usize = 4 * 1024 * 1024;
    const PAGE: usize = 4096;

    struct Args {
        shm_path: String,
        latency_ms: u32,
        /// PipeWire quantum in frames (NODE_LATENCY = quantum/rate).
        quantum: u32,
        /// Experimental video passthrough: FIFO to forward raw frames to (enables
        /// video when set).
        video_fifo: Option<String>,
        /// Stagger before reading a freshly published frame (microseconds).
        video_read_delay_us: u64,
        /// Desired FIFO buffer size in bytes (must hold >= one frame).
        video_pipe_size: usize,
    }

    fn usage() -> ! {
        eprintln!(
            "Usage: lalah-host [--shm <path>] [--latency-ms <u32>] [--quantum <frames>]\n\
             \x20                 [--video-fifo <path>] [--video-read-delay-us <us>] [--video-pipe-size <bytes>]\n\
             \n\
             --shm <path>                 memory-backend-file path (default {DEFAULT_SHM})\n\
             --latency-ms <u32>           max buffered latency before dropping old audio (default {DEFAULT_LATENCY_MS})\n\
             --quantum <frames>           PipeWire quantum hint in frames; lower = less latency,\n\
             \x20                            more wakeups (default {DEFAULT_QUANTUM}; server may clamp)\n\
             --video-fifo <path>          EXPERIMENTAL: forward raw video frames to this named pipe\n\
             \x20                            (enables video; pins audio to the first 16 MiB)\n\
             --video-read-delay-us <us>   stagger before reading a new frame (default {DEFAULT_VIDEO_READ_DELAY_US})\n\
             --video-pipe-size <bytes>    desired FIFO buffer size (default {DEFAULT_VIDEO_PIPE_SIZE}; needs\n\
             \x20                            fs.pipe-max-size raised for >1 MiB)"
        );
        std::process::exit(2);
    }

    fn parse_args() -> Args {
        let mut shm_path = DEFAULT_SHM.to_string();
        let mut latency_ms = DEFAULT_LATENCY_MS;
        let mut quantum = DEFAULT_QUANTUM;
        let mut video_fifo = None;
        let mut video_read_delay_us = DEFAULT_VIDEO_READ_DELAY_US;
        let mut video_pipe_size = DEFAULT_VIDEO_PIPE_SIZE;
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--shm" => shm_path = it.next().unwrap_or_else(|| usage()),
                "--latency-ms" => {
                    latency_ms = it
                        .next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(|| usage())
                }
                "--quantum" => {
                    quantum = it
                        .next()
                        .and_then(|v| v.parse().ok())
                        .filter(|&q| q > 0)
                        .unwrap_or_else(|| usage())
                }
                "--video-fifo" => video_fifo = Some(it.next().unwrap_or_else(|| usage())),
                "--video-read-delay-us" => {
                    video_read_delay_us = it
                        .next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(|| usage())
                }
                "--video-pipe-size" => {
                    video_pipe_size = it
                        .next()
                        .and_then(|v| v.parse().ok())
                        .filter(|&s| s > 0)
                        .unwrap_or_else(|| usage())
                }
                "-h" | "--help" => usage(),
                other => {
                    eprintln!("unknown argument: {other}");
                    usage();
                }
            }
        }
        Args {
            shm_path,
            latency_ms,
            quantum,
            video_fifo,
            video_read_delay_us,
            video_pipe_size,
        }
    }

    /// Largest power of two `<= n` (the ring capacity that fits the payload).
    fn prev_power_of_two(n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        1usize << (usize::BITS - 1 - n.leading_zeros())
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args = parse_args();

        // The host must NOT create/truncate the file — QEMU owns its size. We
        // only open + mmap what is already there.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&args.shm_path)
            .map_err(|e| format!("open {}: {e} (is the VM/QEMU running with share=on?)", args.shm_path))?;

        // map_raw (NOT map_mut): a `&mut [u8]` over guest-mutated memory would be
        // aliasing UB; we only ever touch it through the contract's raw API.
        let mmap = MmapOptions::new().map_raw(&file)?;
        let base = mmap.as_mut_ptr();
        let total = mmap.len();

        let header_size = std::mem::size_of::<ShmHeader>();
        if total <= header_size {
            return Err(format!("shm region {total} B too small for header {header_size} B").into());
        }

        // Fault every page in once so the RT process callback never page-faults.
        let mut off = 0;
        while off < total {
            unsafe { std::ptr::read_volatile(base.add(off)) };
            off += PAGE;
        }

        // With video enabled the audio ring is pinned to the first 16 MiB and the
        // video sub-region takes everything after it; otherwise audio owns the
        // whole mapping (byte-identical to a video-unaware build).
        let cap = if args.video_fifo.is_some() {
            if total <= shared::AUDIO_REGION_BYTES {
                return Err(format!(
                    "--video-fifo set but shm region {total} B <= audio region {} B; grow QEMU size=",
                    shared::AUDIO_REGION_BYTES
                )
                .into());
            }
            prev_power_of_two(shared::AUDIO_REGION_BYTES - header_size)
        } else {
            prev_power_of_two(total - header_size)
        };
        println!(
            "lalah-host: mapped {} ({} B), ring capacity {} B; initializing header.",
            args.shm_path, total, cap
        );

        // Host is the initializer: publish magic/version/capacity, format UNSET.
        let ring = unsafe { ShmAudioBuffer::init(base, total, cap) };

        // Experimental video passthrough: initialize the video sub-region and spawn
        // a thread that forwards frames to the FIFO. It runs for the whole process;
        // `mmap` stays alive (the audio playback below blocks forever), keeping the
        // video pointers valid.
        if let Some(fifo) = args.video_fifo.clone() {
            match unsafe { shared::video_region(base, total) } {
                Some((vbase, vlen)) => {
                    let vid = unsafe { ShmVideoBuffer::init(vbase, vlen) };
                    let delay = args.video_read_delay_us;
                    let psize = args.video_pipe_size;
                    println!(
                        "lalah-host: video passthrough enabled (region {} B at +{} B, FIFO {}).",
                        vlen,
                        shared::AUDIO_REGION_BYTES,
                        fifo
                    );
                    std::thread::Builder::new()
                        .name("video-forward".into())
                        .spawn(move || video::run_video(vid, fifo, psize, delay))?;
                }
                None => {
                    return Err(format!(
                        "--video-fifo set but shm region {total} B has no room past the {} B audio region; grow QEMU size=",
                        shared::AUDIO_REGION_BYTES
                    )
                    .into());
                }
            }
        }

        // Wait for the producer (VM) to negotiate + publish the format.
        let fmt = loop {
            if let Some(fmt) = ring.format() {
                break fmt;
            }
            println!("lalah-host: waiting for the VM to publish an audio format…");
            std::thread::sleep(Duration::from_millis(500));
        };

        // latency budget in bytes = ms * rate * frame_bytes / 1000.
        let max_latency_bytes =
            args.latency_ms as u64 * fmt.sample_rate as u64 * fmt.frame_bytes as u64 / 1000;

        // `mmap` and `file` stay alive in this scope for the whole blocking run,
        // keeping the pointers inside `ring` valid.
        playback::run_playback(fmt, ring, max_latency_bytes, args.quantum)?;

        drop(mmap);
        drop(file);
        Ok(())
    }
}
