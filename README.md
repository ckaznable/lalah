# lalah — IVSHMEM audio bridge (Win11 VM → Linux host)

Bridges audio from a Windows 11 guest to a Linux host over an **IVSHMEM** shared
memory region, with minimal latency.

- **`lalah-vm`** (Windows guest, producer): captures audio — either a *given*
  endpoint via **WASAPI exclusive, event-driven**, or a capture card's audio via
  **DirectShow** (`--ds-audio`) — and writes raw PCM frames into a lock-free ring
  in the shared region.
- **`lalah-host`** (Linux host, consumer): `mmap`s the IVSHMEM
  memory-backend-file, reads the ring, and plays the bytes out through
  **PipeWire**, with bounded latency (drops the oldest audio if it falls behind).
- **`shared`**: the *frozen* cross-process contract — the `ShmHeader` layout and
  the `ShmAudioBuffer` ring API. Both sides compile against it verbatim.

There is also an **experimental video passthrough** (off by default): the VM grabs
raw capture-card frames and writes them into a second sub-region of the same
IVSHMEM mapping; the host forwards whole frames into a named pipe (FIFO) for mpv.
See [Experimental: video passthrough](#experimental-video-passthrough).

```
 ┌─────────────── Windows 11 guest ───────────────┐      ┌──────────── Linux host ────────────┐
 │  capture endpoint ──WASAPI/DShow──► lalah-vm    │      │   lalah-host ──► PipeWire ──► speakers│
 │                                       │ push     │      │      ▲ read_quantum                  │
 │                              ivshmem.sys (BAR2)  │      │   mmap(/dev/shm/lalah)               │
 └───────────────────────────────────────┼────────┘      └──────┼──────────────────────────────┘
                                          └─────── IVSHMEM shared memory ───────┘
                                          offset 0 = ShmHeader, payload @ 192
```

## Design highlights (the `shared` contract)

The shared region is mutated by two separate processes / OSes, so the contract is
deliberately conservative:

- **Raw pointers + atomics only** — never a `&[u8]`/`&mut [u8]` over the payload
  (that would be aliasing UB with a foreign writer). Payload bytes move via
  *volatile* element copies.
- **`#[repr(C, align(64))]` header, 192 bytes = three cache lines**: read-mostly
  config (magic/version/capacity/format) on line 0, producer `tail`+`seq` on
  line 1, consumer `head` on line 2 — no false sharing.
- **Authoritative power-of-two `capacity`** → all index math is a mask, identical
  on both sides; validated on attach (with magic/version/header-size) so a layout
  skew fails loudly instead of producing garbage audio.
- **Producer seqlock + consumer post-copy recheck** eliminate torn reads / overrun.
- **Format auto-negotiation**: the host initializes the header with the format
  *unset*; the VM publishes the negotiated `(rate, channels, sample-format)` once
  on capture start; the host waits for it before configuring PipeWire.
- **All offsets are frame-aligned**, so a PCM sample is never split (no clicks).

Sample formats carried: `S16LE`, `S32LE`, `F32LE` (little-endian both sides;
bytes flow memcpy-straight, no conversion). 24-bit is intentionally unsupported.

## Building

### Host (Linux)

System packages: `libpipewire-0.3-dev`, `libspa-0.2-dev`, `clang`/`libclang-dev`,
`pkg-config` (the `pipewire`/`libspa-sys` crates run `bindgen` at build time). Set
`LIBCLANG_PATH` if `libclang` isn't auto-discovered.

```sh
cargo build --release -p lalah-host
```

### Guest (Windows, x86_64)

Build on Windows (MSVC toolchain), or cross-compile. The `windows` crate links
its import libs via `raw-dylib`, so no extra setup is needed:

```sh
cargo build --release -p lalah-vm                       # on Windows
cargo check  -p lalah-vm --target x86_64-pc-windows-gnu # type-check from Linux
```

> The IVSHMEM `IVSHMEM_MMAP` ioctl struct layout is **64-bit specific** — build
> x86_64 only.

## QEMU / IVSHMEM setup

Use **`ivshmem-plain`** with a **memory-backend-file** (no register prefix; the
host file offset 0 equals the guest BAR2 offset 0 equals the `ShmHeader`):

```
-object memory-backend-file,id=hostmem,mem-path=/dev/shm/lalah,size=2097152,share=on \
-device ivshmem-plain,memdev=hostmem
```

- `share=on` is **mandatory** (without it QEMU maps copy-on-write and the host
  never sees guest writes).
- `size=` must be a power of two; the ring capacity becomes the largest power of
  two `≤ size − 192`. ~2 MiB is ample for 48 kHz stereo.
- If you change `size=`, delete the stale `/dev/shm/lalah` so QEMU recreates it.
- The file is created as QEMU's user — run QEMU as your user, or `chmod`/`chown`
  so `lalah-host` can open it `O_RDWR`.

In the guest, install the **IVSHMEM driver** (`ivshmem.sys` from virtio-win /
Looking Glass) for the IVSHMEM PCI device.

## Running

Start the **host first** (it initializes the header), then the guest producer:

```sh
# Linux host
./lalah-host --shm /dev/shm/lalah --latency-ms 20 --quantum 256

# Windows guest (capture/input endpoint id; no enumeration)
lalah-vm.exe --device "{0.0.1.00000000}.{your-endpoint-guid}"

# Capture card that needs video running for audio (AVerMedia GC573 etc.)
lalah-vm.exe --device "{0.0.1.00000000}.{guid}" --video-device "Live Gamer"

# Capture card whose audio only flows via DirectShow (GC573): grab audio via DShow.
# Run once without --audio-device first to list the detected audio devices.
lalah-vm.exe --ds-audio --audio-device "AVerMedia"
```

`lalah-vm` attach-retries until the host has initialized the region, publishes the
negotiated format, then streams. `lalah-host` waits for that format, then plays.

`lalah-vm` flags:

- `--device <endpoint-id>` — the WASAPI capture (input) endpoint to grab.
- `--ds-audio` — capture audio via **DirectShow** instead of WASAPI (for capture
  cards whose audio only flows through their DShow filter). `--device` is then
  ignored.
- `--audio-device <name>` — friendly-name substring to pick the DShow audio device
  (implies `--ds-audio`; default: the first audio device).
- `--video-keepalive` — open a **DirectShow** video capture stream and discard its
  frames so the card starts streaming its audio pin (see below).
- `--video-device <name>` — friendly-name substring to pick the video device
  (implies `--video-keepalive`; default: the first video device).
- `--video-renderer <null|window>` — renderer for the keep-alive: `null` (default,
  no window) or `window` (shows a preview window; try it if a device refuses to
  stream to a null renderer).
- `--video-passthrough` — **EXPERIMENTAL**: grab raw video frames via a DirectShow
  SampleGrabber and forward them into the shared video region. Implies `--ds-audio`
  (it rides the same Filter Graph). Use `--video-device <name>` to pick the card.
  Requires the host to run with `--video-fifo`.

`lalah-host` flags:

- `--shm <path>` — memory-backend-file to mmap (default `/dev/shm/lalah`).
- `--latency-ms <u32>` — jitter-buffer budget; the ring drops the oldest audio once
  the backlog exceeds it (default 20).
- `--quantum <frames>` — PipeWire quantum hint (`NODE_LATENCY = quantum/rate`);
  lower = less latency, more wakeups (default 256; the server may clamp).
- `--video-fifo <path>` — **EXPERIMENTAL**: enable video passthrough and forward
  raw frames to this named pipe (created if absent). Pins audio to the first 16 MiB.
- `--video-read-delay-us <us>` — stagger before reading a freshly published frame
  (default 500; see below). Lower to 0 to disable the stagger.
- `--video-pipe-size <bytes>` — desired FIFO buffer size (default 4 MiB; must hold
  at least one whole frame — see the `fs.pipe-max-size` note below).

### Tuning latency

End-to-end latency ≈ capture buffer + ring jitter buffer (`--latency-ms`) + PipeWire
quantum (`--quantum`) + sink buffer. The two knobs you control are `--latency-ms`
and `--quantum`; lower each until the audio starts to break up, then back off.
`lalah-host` prints a once-per-second line whenever it slips —
`Underflows (Producer/VM slow)` / `Overflows (Consumer/Host slow)` — so watch that
while tuning, and cross-check the actual quantum with `pw-top`.

## Experimental: video passthrough

Off by default. When enabled, the **same** IVSHMEM mapping is split: audio is
pinned to the first **16 MiB** (its ring becomes the largest power of two that
fits there), and everything after 16 MiB is a self-describing video sub-region —
its own magic/version header (the audio ABI is unchanged) followed by an N-slot
frame ring.

```
 VM: card ─DShow SampleGrabber─► push_frame ─┐                ┌─► read_frame ─► FIFO ─► mpv
                                             ▼                │   (O_NONBLOCK, drop if full)
   IVSHMEM:  [0 .. 16 MiB) audio ring  │  [16 MiB .. end) ShmVideoHeader + frame slots
```

Enable it on **both** sides:

```sh
# Host: forward frames to a FIFO that mpv will read.
./lalah-host --video-fifo /tmp/lalah-video

# Guest (GC573 etc.): grab audio + video over DirectShow.
lalah-vm.exe --video-passthrough --audio-device "AVerMedia" --video-device "Live Gamer"
```

On start the host prints the published geometry and a ready-to-run **mpv** command,
e.g.:

```sh
mpv --demuxer=rawvideo --demuxer-rawvideo-w=1920 --demuxer-rawvideo-h=1080 \
    --demuxer-rawvideo-mp-format=yuyv422 --untimed --profile=low-latency /tmp/lalah-video
```

Start mpv (or start it first — the host retries the FIFO open until a reader
connects, in either order). Frames are passed through **raw and uncompressed**;
the host derives the mpv pixel format from the card's FourCC (`YUY2`→`yuyv422`,
`NV12`→`nv12`, `P010`→`p010`, …) and prints a `--demuxer-rawvideo-format=<FourCC>`
fallback for anything it doesn't recognise.

**Sizing the region.** You need `size = 16 MiB + (≥ 3 × frame_size)`. A 1080p
`YUY2` frame is ~4 MiB, so allow ~32 MiB minimum; **64 MiB** is comfortable:

```
-object memory-backend-file,id=hostmem,mem-path=/dev/shm/lalah,size=67108864,share=on \
-device ivshmem-plain,memdev=hostmem
```

If fewer than 3 frame slots fit past 16 MiB, the VM logs the rejection and runs
audio-only.

**Pipe buffer (important).** A whole frame must fit in the FIFO, so the host tries
to set the pipe buffer to `--video-pipe-size` (default 4 MiB). The unprivileged
cap is `fs.pipe-max-size` (1 MiB by default), so 4 MiB fails with `EPERM` and the
host warns and falls back — at which point frames larger than the pipe are *all*
dropped. Raise the cap once:

```sh
sudo sysctl -w fs.pipe-max-size=4194304   # or larger; or run lalah-host with CAP_SYS_RESOURCE
```

**No tearing, no blocking.** The producer writes slots round-robin and publishes a
monotonic frame count; the consumer reads the newest slot and re-checks the count,
dropping a frame only if the writer could have lapped it during the copy (with
≥ 3 slots the two never share a slot in practice). The producer never waits on the
consumer. `--video-read-delay-us` (default 500 µs) additionally staggers the host's
read after a new frame is observed — measured on the host's own clock, so no
guest/host clock comparison is needed — to keep the host's large copy off the same
instant as the VM's write. It is a margin on top of the structural tear-safety, so
you can lower it (even to 0) freely.

**Behind on the consumer?** If mpv can't keep up, the pipe fills and the host drops
whole frames (never partial), logging `dropped N frame(s)` periodically — video
degrades to a lower frame rate without desyncing or stalling audio.

### Windows prerequisite

The capture endpoint must allow exclusive control: in `mmsys.cpl` → Recording →
the device → *Advanced* → enable **"Allow applications to take exclusive control
of this device"**. Otherwise `Initialize` fails with
`AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED`.

### Capture cards (AVerMedia GC573, Elgato, …)

Two device-specific behaviours are handled:

1. **Exclusive format probing.** Many cards accept only the plain `WAVEFORMATEX`
   form in exclusive mode and reject `WAVEFORMATEXTENSIBLE` for 16-bit stereo
   (this is the format the *Advanced* tab shows, e.g. 48000/16/2). `lalah-vm`
   probes plain `WAVEFORMATEX` first, then extensible, and logs every probe with
   its `HRESULT` so you can see exactly what the device accepts.
2. **Video-gated audio.** The card only emits audio while video is captured. Pass
   `--video-keepalive` (or `--video-device <name>`): `lalah-vm` opens the video
   source with **DirectShow** and discards every frame (the OBS approach), purely
   to keep the audio pin alive. This starts **before** the audio probe so the
   endpoint is live by the time we open it. Use `--video-renderer window` if a
   device won't start streaming to a null renderer.
3. **DirectShow audio fallback.** If the card's audio never appears via WASAPI
   (the endpoint stays silent even with video running), grab it through DirectShow
   instead: `--ds-audio` builds a `source → SampleGrabber → NullRenderer` graph
   and forwards the grabbed PCM into the ring — the same way OBS pulls capture-card
   audio. Opening the DShow graph also starts the card streaming, so
   `--video-keepalive` is usually unnecessary alongside it. Run `--ds-audio` once
   with no `--audio-device` to list the detected audio devices, then pass a name
   substring.

## License

MIT
