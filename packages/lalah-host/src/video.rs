//! Experimental video passthrough (host side): drain the shared-memory video
//! ring and forward whole raw frames into a named pipe (FIFO) for a downstream
//! player (mpv).
//!
//! Policy, per the design:
//!  * The FIFO is opened `O_WRONLY | O_NONBLOCK`; its buffer is enlarged to
//!    `pipe_size` (>= one frame) so a whole frame fits.
//!  * If the reader has not connected yet (`ENXIO`) we keep retrying the open.
//!  * A frame is written ONLY if it fits whole in the pipe's free space; if the
//!    reader is behind (pipe full) the frame is DROPPED — we never block.
//!  * Reads are staggered `read_delay_us` after a new frame is observed, so the
//!    host's big copy does not race the VM's write of the same instant.
//!
//! Tear-safety itself comes from the N-slot ring in `shared` (the writer and
//! reader sit on different slots); the stagger is an extra margin and is tunable.

use shared::ShmVideoBuffer;
use std::ffi::CString;
use std::time::Duration;

/// Entry point for the video thread. Blocks forever (until the process exits).
pub fn run_video(mut vid: ShmVideoBuffer, fifo_path: String, pipe_size: usize, read_delay_us: u64) {
    // Wait for the VM to publish the geometry.
    let vf = loop {
        if let Some(vf) = vid.refresh_geometry() {
            break vf;
        }
        println!("lalah-host(video): waiting for the VM to publish video geometry…");
        std::thread::sleep(Duration::from_millis(300));
    };

    let frame_size = vf.frame_size as usize;
    let cc = fourcc_string(vf.fourcc);
    println!(
        "lalah-host(video): {}x{} {} ({} B/frame). Forwarding to FIFO {}",
        vf.width, vf.height, cc, frame_size, fifo_path
    );
    println!("lalah-host(video): play it with mpv, e.g.:");
    println!("    {}", mpv_command(&vf, &fifo_path));

    if let Err(e) = make_fifo(&fifo_path) {
        eprintln!("lalah-host(video): mkfifo {fifo_path}: {e}; video disabled.");
        return;
    }

    let mut frame = vec![0u8; frame_size];
    let mut fd: i32 = -1;
    let mut warned_small_pipe = false;
    let mut last_count = vid.frame_count();
    let mut dropped: u64 = 0;
    let poll = Duration::from_micros(500);

    loop {
        // (Re)open the FIFO until a reader is present (O_NONBLOCK write-open fails
        // with ENXIO while no reader has it open).
        if fd < 0 {
            match open_fifo_nonblock(&fifo_path) {
                Some(f) => {
                    fd = f;
                    let actual = set_pipe_size(fd, pipe_size);
                    println!(
                        "lalah-host(video): FIFO reader connected; pipe buffer {} B (requested {} B).",
                        actual, pipe_size
                    );
                    if actual < frame_size && !warned_small_pipe {
                        let need = frame_size.next_power_of_two().max(pipe_size);
                        eprintln!(
                            "lalah-host(video): WARNING pipe buffer {actual} B < frame {frame_size} B — \
                             every frame will be dropped. Raise the cap, e.g.:\n    \
                             sudo sysctl -w fs.pipe-max-size={need}\n    \
                             (or run lalah-host with CAP_SYS_RESOURCE / as root)."
                        );
                        warned_small_pipe = true;
                    }
                }
                _ => {
                    // No reader yet — don't accumulate stale frames meanwhile.
                    last_count = vid.frame_count();
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }

        let count = vid.frame_count();
        if count == last_count {
            std::thread::sleep(poll);
            continue;
        }

        // A new frame is available: stagger, then grab the newest.
        if read_delay_us > 0 {
            std::thread::sleep(Duration::from_micros(read_delay_us));
        }
        match vid.read_frame(&mut frame) {
            Some(_newest) => {
                last_count = vid.frame_count();
                match write_frame(fd, &frame) {
                    WriteOutcome::Wrote => {}
                    WriteOutcome::Dropped => {
                        dropped += 1;
                        if dropped == 1 || dropped.is_multiple_of(120) {
                            println!(
                                "lalah-host(video): reader behind / pipe full — dropped {dropped} frame(s) so far."
                            );
                        }
                    }
                    WriteOutcome::ReaderGone => {
                        unsafe { libc::close(fd) };
                        fd = -1;
                        println!("lalah-host(video): FIFO reader disconnected; awaiting a new reader.");
                    }
                }
            }
            None => {
                // Torn (writer lapped us during the copy) — drop and move on.
                last_count = count;
            }
        }
    }
}

enum WriteOutcome {
    Wrote,
    Dropped,
    ReaderGone,
}

/// Write the whole frame iff it fits in the pipe's current free space; otherwise
/// drop it. Pre-checking free space keeps frame boundaries intact (we are the
/// only writer, so the reader only ever frees more space between check and write).
fn write_frame(fd: i32, frame: &[u8]) -> WriteOutcome {
    let pipe_sz = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
    if pipe_sz > 0 {
        let mut queued: libc::c_int = 0;
        if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut queued) } == 0 {
            let free = pipe_sz as i64 - queued as i64;
            if free < frame.len() as i64 {
                return WriteOutcome::Dropped;
            }
        }
    }
    let n = unsafe { libc::write(fd, frame.as_ptr() as *const libc::c_void, frame.len()) };
    if n == frame.len() as isize {
        return WriteOutcome::Wrote;
    }
    if n < 0 {
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EAGAIN) => WriteOutcome::Dropped,
            Some(libc::EPIPE) => WriteOutcome::ReaderGone,
            _ => WriteOutcome::Dropped,
        }
    } else {
        // Partial write — should not happen after the free-space check; treat as a
        // drop (a desync guard) rather than leave the stream half-framed.
        WriteOutcome::Dropped
    }
}

/// Create the FIFO if it does not already exist.
fn make_fifo(path: &str) -> std::io::Result<()> {
    let c = CString::new(path).map_err(|_| std::io::Error::other("fifo path has interior NUL"))?;
    if unsafe { libc::mkfifo(c.as_ptr(), 0o644) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EEXIST) {
        Ok(()) // already a FIFO (or some node) at that path — reuse it
    } else {
        Err(err)
    }
}

/// Open the FIFO for non-blocking writes. Returns `None` while no reader is
/// connected (`ENXIO`) — the caller retries.
fn open_fifo_nonblock(path: &str) -> Option<i32> {
    let c = CString::new(path).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
    (fd >= 0).then_some(fd)
}

/// Enlarge the pipe buffer to `size` (rounded up to a power of two by the kernel,
/// capped at `fs.pipe-max-size`). Returns the actual size in bytes.
fn set_pipe_size(fd: i32, size: usize) -> usize {
    unsafe {
        let r = libc::fcntl(fd, libc::F_SETPIPE_SZ, size as libc::c_int);
        if r >= 0 {
            return r as usize;
        }
        // EPERM (size > fs.pipe-max-size for an unprivileged process) etc. — fall
        // back to whatever the pipe currently is.
        libc::fcntl(fd, libc::F_GETPIPE_SZ).max(0) as usize
    }
}

/// Render a FourCC as its 4 ASCII chars (or `0x…` if not printable).
fn fourcc_string(fourcc: u32) -> String {
    if fourcc == 0 {
        return "RGB/BI_RGB".to_string();
    }
    let b = fourcc.to_le_bytes();
    if b.iter().all(|&c| c.is_ascii_graphic() || c == b' ') {
        String::from_utf8_lossy(&b).trim_end().to_string()
    } else {
        format!("0x{fourcc:08X}")
    }
}

/// Map a DirectShow FourCC to an mpv `--demuxer-rawvideo-mp-format` name, if known.
fn mpv_mp_format(fourcc: u32) -> Option<&'static str> {
    match &fourcc.to_le_bytes() {
        b"YUY2" | b"YUYV" => Some("yuyv422"),
        b"UYVY" => Some("uyvy422"),
        b"YVYU" => Some("yvyu422"),
        b"NV12" => Some("nv12"),
        b"NV21" => Some("nv21"),
        b"YV12" => Some("yuv420p"), // planar; channel order differs but format/size match
        b"I420" | b"IYUV" => Some("yuv420p"),
        b"P010" => Some("p010"),
        _ => None,
    }
}

/// Build a ready-to-run mpv command for the published geometry.
fn mpv_command(vf: &shared::VideoFormat, fifo: &str) -> String {
    let base = format!(
        "mpv --demuxer=rawvideo --demuxer-rawvideo-w={} --demuxer-rawvideo-h={}",
        vf.width, vf.height
    );
    let fmt = match mpv_mp_format(vf.fourcc) {
        Some(name) => format!("--demuxer-rawvideo-mp-format={name}"),
        _ => format!(
            "--demuxer-rawvideo-format={} # adjust if mpv rejects it",
            fourcc_string(vf.fourcc)
        ),
    };
    format!("{base} {fmt} --demuxer-rawvideo-fps=60 --profile=low-latency {fifo}")
}
