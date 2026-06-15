# lalah — IVSHMEM audio bridge (Win11 VM → Linux host)

Bridges audio from a Windows 11 guest to a Linux host over an **IVSHMEM** shared
memory region, with minimal latency.

- **`lalah-vm`** (Windows guest, producer): grabs a *given* capture/input endpoint
  with **WASAPI in exclusive, event-driven mode** and writes raw PCM frames into a
  lock-free ring in the shared region.
- **`lalah-host`** (Linux host, consumer): `mmap`s the IVSHMEM
  memory-backend-file, reads the ring, and plays the bytes out through
  **PipeWire**, with bounded latency (drops the oldest audio if it falls behind).
- **`shared`**: the *frozen* cross-process contract — the `ShmHeader` layout and
  the `ShmAudioBuffer` ring API. Both sides compile against it verbatim.

```
 ┌─────────────── Windows 11 guest ───────────────┐      ┌──────────── Linux host ────────────┐
 │  capture endpoint ──WASAPI excl.──► lalah-vm    │      │   lalah-host ──► PipeWire ──► speakers│
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
  on WASAPI start; the host waits for it before configuring PipeWire.
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
./lalah-host --shm /dev/shm/lalah --latency-ms 20

# Windows guest (capture/input endpoint id; no enumeration)
lalah-vm.exe --device "{0.0.1.00000000}.{your-endpoint-guid}"
```

`lalah-vm` attach-retries until the host has initialized the region, publishes the
negotiated format, then streams. `lalah-host` waits for that format, then plays.

### Windows prerequisite

The capture endpoint must allow exclusive control: in `mmsys.cpl` → Recording →
the device → *Advanced* → enable **"Allow applications to take exclusive control
of this device"**. Otherwise `Initialize` fails with
`AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED`.

## License

MIT
