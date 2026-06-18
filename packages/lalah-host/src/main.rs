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
    use crate::playback;
    use memmap2::MmapOptions;
    use shared::{ShmAudioBuffer, ShmHeader};
    use std::fs::OpenOptions;
    use std::time::Duration;

    const DEFAULT_SHM: &str = "/dev/shm/lalah";
    const DEFAULT_LATENCY_MS: u32 = 20;
    const DEFAULT_QUANTUM: u32 = 256;
    const PAGE: usize = 4096;

    struct Args {
        shm_path: String,
        latency_ms: u32,
        /// PipeWire quantum in frames (NODE_LATENCY = quantum/rate).
        quantum: u32,
    }

    fn usage() -> ! {
        eprintln!(
            "Usage: lalah-host [--shm <path>] [--latency-ms <u32>] [--quantum <frames>]\n\
             \n\
             --shm <path>         memory-backend-file path (default {DEFAULT_SHM})\n\
             --latency-ms <u32>   max buffered latency before dropping old audio (default {DEFAULT_LATENCY_MS})\n\
             --quantum <frames>   PipeWire quantum hint in frames; lower = less latency,\n\
             \x20                    more wakeups (default {DEFAULT_QUANTUM}; server may clamp)"
        );
        std::process::exit(2);
    }

    fn parse_args() -> Args {
        let mut shm_path = DEFAULT_SHM.to_string();
        let mut latency_ms = DEFAULT_LATENCY_MS;
        let mut quantum = DEFAULT_QUANTUM;
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

        let cap = prev_power_of_two(total - header_size);
        println!(
            "lalah-host: mapped {} ({} B), ring capacity {} B; initializing header.",
            args.shm_path, total, cap
        );

        // Host is the initializer: publish magic/version/capacity, format UNSET.
        let ring = unsafe { ShmAudioBuffer::init(base, total, cap) };

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
