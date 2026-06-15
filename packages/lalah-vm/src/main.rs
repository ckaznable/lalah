//! lalah-vm — the Windows 11 producer.
//!
//! Opens a GIVEN capture (input) endpoint with WASAPI in EXCLUSIVE, event-driven
//! mode for minimal latency, maps the IVSHMEM region via the ivshmem.sys driver,
//! and pushes the raw PCM frames into the shared ring for the Linux host.
//!
//! The device id is provided up front (no enumeration). The host must already be
//! running (it initializes the shared header); this side attach-retries until the
//! magic is valid, then publishes the negotiated format and starts capturing.

#[cfg(windows)]
mod ivshmem;
#[cfg(windows)]
mod wasapi;

#[cfg(not(windows))]
fn main() {
    eprintln!("lalah-vm is Windows-only (it captures via WASAPI).");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows_main::run()
}

#[cfg(windows)]
mod windows_main {
    use crate::{ivshmem, wasapi};
    use shared::ShmAudioBuffer;
    use std::time::Duration;

    /// Capture endpoint id to use when `--device` is not passed. Fill this in with
    /// your VM's recording endpoint id (the long `{0.0.1.00000000}.{guid}` form).
    const DEFAULT_DEVICE_ID: &str = "";

    struct Args {
        device_id: String,
    }

    fn usage() -> ! {
        eprintln!(
            "Usage: lalah-vm [--device <endpoint-id>]\n\
             \n\
             --device <endpoint-id>   WASAPI capture (input) endpoint id to grab\n\
             \n\
             If omitted, the baked-in DEFAULT_DEVICE_ID is used."
        );
        std::process::exit(2);
    }

    fn parse_args() -> Args {
        let mut device_id = DEFAULT_DEVICE_ID.to_string();
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--device" => device_id = it.next().unwrap_or_else(|| usage()),
                "-h" | "--help" => usage(),
                other => {
                    eprintln!("unknown argument: {other}");
                    usage();
                }
            }
        }
        if device_id.is_empty() {
            eprintln!("error: no capture device id (pass --device or set DEFAULT_DEVICE_ID)");
            usage();
        }
        Args { device_id }
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args = parse_args();

        // Map the IVSHMEM BAR2 region (kept alive for the whole process).
        let region = ivshmem::map_ivshmem()?;
        println!(
            "lalah-vm: mapped IVSHMEM region {} B at {:p}.",
            region.len, region.base
        );

        // Attach to the ring the host initialized. Retry until the host is up.
        let mut ring = loop {
            match unsafe { ShmAudioBuffer::attach(region.base, region.len) } {
                Ok(r) => break r,
                Err(e) => {
                    println!("lalah-vm: waiting for host to initialize the ring ({e})…");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        };
        println!(
            "lalah-vm: attached ring (capacity {} B). Opening capture device…",
            ring.capacity()
        );

        // Run the WASAPI capture loop: it publishes the format then pushes PCM
        // into the ring until the stream stalls or errors.
        wasapi::capture_exclusive(&args.device_id, &mut ring)?;
        Ok(())
    }
}
