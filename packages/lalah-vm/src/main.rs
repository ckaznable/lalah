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
mod dshow;
#[cfg(windows)]
mod ivshmem;
#[cfg(windows)]
mod video;
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
    use crate::{dshow, ivshmem, video, wasapi};
    use shared::ShmAudioBuffer;
    use std::time::Duration;

    /// Capture endpoint id to use when `--device` is not passed. Fill this in with
    /// your VM's recording endpoint id (the long `{0.0.1.00000000}.{guid}` form).
    const DEFAULT_DEVICE_ID: &str = "";

    struct Args {
        device_id: String,
        /// Use DirectShow (not WASAPI) to capture audio — for capture cards whose
        /// audio only flows through their DShow filter (GC573 etc.).
        ds_audio: bool,
        /// Optional friendly-name substring to pick the DShow audio device.
        audio_device: Option<String>,
        /// Keep a video capture stream open so the card emits audio (GC573 etc.).
        video_keepalive: bool,
        /// Optional friendly-name substring to pick the video device.
        video_device: Option<String>,
        /// Video renderer: "null" (default, no window) or "window" (opens a window).
        video_renderer: String,
    }

    fn usage() -> ! {
        eprintln!(
            "Usage: lalah-vm [--device <endpoint-id>] [--video-keepalive] [--video-device <name>] [--video-renderer <null|window>]\n\
             \n\
             --device <endpoint-id>         WASAPI capture (input) endpoint id to grab\n\
             --ds-audio                     capture audio via DirectShow instead of WASAPI (GC573)\n\
             --audio-device <name>          friendly-name substring for the DShow audio device\n\
             \x20                              (implies --ds-audio; default: first audio device)\n\
             --video-keepalive              open a video capture stream and discard frames so the\n\
             \x20                              card starts its audio (needed for AVerMedia GC573 etc.)\n\
             --video-device <name>          friendly-name substring to pick the video device\n\
             \x20                              (implies --video-keepalive; default: first video device)\n\
             --video-renderer <null|window> video renderer to use (default: null)\n\
             \n\
             If --device is omitted, the baked-in DEFAULT_DEVICE_ID is used."
        );
        std::process::exit(2);
    }

    fn parse_args() -> Args {
        let mut device_id = DEFAULT_DEVICE_ID.to_string();
        let mut ds_audio = false;
        let mut audio_device = None;
        let mut video_keepalive = false;
        let mut video_device = None;
        let mut video_renderer = "null".to_string();
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--device" => device_id = it.next().unwrap_or_else(|| usage()),
                "--ds-audio" => ds_audio = true,
                "--audio-device" => {
                    audio_device = Some(it.next().unwrap_or_else(|| usage()));
                    ds_audio = true;
                }
                "--video-keepalive" => video_keepalive = true,
                "--video-device" => {
                    video_device = Some(it.next().unwrap_or_else(|| usage()));
                    video_keepalive = true;
                }
                "--video-renderer" => {
                    let val = it.next().unwrap_or_else(|| usage());
                    if val != "null" && val != "window" {
                        eprintln!("error: --video-renderer must be 'null' or 'window'");
                        usage();
                    }
                    video_renderer = val;
                }
                "-h" | "--help" => usage(),
                other => {
                    eprintln!("unknown argument: {other}");
                    usage();
                }
            }
        }
        // WASAPI needs an endpoint id; the DShow path selects by name instead.
        if !ds_audio && device_id.is_empty() {
            eprintln!("error: no capture device id (pass --device, --ds-audio, or set DEFAULT_DEVICE_ID)");
            usage();
        }
        Args {
            device_id,
            ds_audio,
            audio_device,
            video_keepalive,
            video_device,
            video_renderer,
        }
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

        // Capture cards (e.g. AVerMedia GC573) only emit audio while video is
        // streaming. Keep the video pin alive (OBS-style) BEFORE probing audio so
        // the audio endpoint reports its formats and produces frames. Held for the
        // whole capture; dropped (stops the worker) when run() returns.
        // For DirectShow audio capture, we run video and audio in the same Filter Graph.
        // For WASAPI capture, we run the keepalive in a separate thread.
        let _video = if !args.ds_audio && args.video_keepalive {
            let use_null_renderer = args.video_renderer == "null";
            match video::VideoKeepAlive::start(args.video_device.clone(), use_null_renderer) {
                Ok(v) => {
                    // Let the card spin up before the WASAPI format probe.
                    std::thread::sleep(Duration::from_millis(700));
                    Some(v)
                }
                Err(e) => {
                    eprintln!("lalah-vm: video keep-alive failed: {e}");
                    eprintln!(
                        "lalah-vm: continuing without it (audio may not start on this device)."
                    );
                    None
                }
            }
        } else {
            None
        };

        if args.ds_audio {
            // DirectShow audio capture: publishes the format then forwards PCM
            // into the ring. Takes the ring by value and blocks until terminated.
            // Both audio capture and video keep-alive run in the same Filter Graph here.
            let video_renderer_type = if args.video_keepalive {
                Some(args.video_renderer.as_str())
            } else {
                None
            };
            dshow::capture_dshow_audio(
                args.audio_device.as_deref(),
                args.video_device.as_deref(),
                video_renderer_type,
                ring,
            )?;
        } else {
            // WASAPI exclusive capture: publishes the format then pushes PCM into
            // the ring until the stream stalls or errors.
            std::thread::sleep(Duration::from_millis(3000));
            wasapi::capture_exclusive(&args.device_id, &mut ring)?;
        }
        Ok(())
    }
}
