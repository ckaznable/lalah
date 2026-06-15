//! Cross-mapping integration test: proves that TWO INDEPENDENT mmaps of the same
//! backing file (the stand-in for the host process and the guest process sharing
//! the IVSHMEM region) observe each other's header atomics and payload writes
//! through the frozen `shared` contract.
//!
//! Linux-only (uses `memmap2`, which is a linux-gated dependency).
#![cfg(target_os = "linux")]

use memmap2::MmapOptions;
use shared::{AudioFormat, SampleFormat, ShmAudioBuffer, ShmHeader};
use std::fs::OpenOptions;

#[test]
fn two_independent_mappings_share_state() {
    let path = std::env::temp_dir().join(format!("lalah-xmap-{}", std::process::id()));
    let header = std::mem::size_of::<ShmHeader>();
    let cap = 4096usize; // power of two
    let total = header + cap;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    file.set_len(total as u64).unwrap();

    // Map #1 — the "host": initializes the header (format UNSET).
    let map_host = MmapOptions::new().map_raw(&file).unwrap();
    let mut host = unsafe { ShmAudioBuffer::init(map_host.as_mut_ptr(), total, cap) };

    // Map #2 — the "guest": a SEPARATE mapping of the same file. attach() must
    // observe the magic the host published.
    let map_guest = MmapOptions::new().map_raw(&file).unwrap();
    let mut guest = unsafe { ShmAudioBuffer::attach(map_guest.as_mut_ptr(), total) }
        .expect("guest must attach to the host-initialized region");

    // Before the guest publishes a format, the host sees none.
    assert!(host.format().is_none());

    // Guest (producer) publishes the negotiated format; host (consumer) sees it
    // across the independent mapping.
    let fmt = AudioFormat {
        sample_rate: 48_000,
        channels: 2,
        format: SampleFormat::S16Le,
        frame_bytes: 4,
    };
    guest.set_format(fmt);
    let seen = host.format().expect("host sees the guest's format");
    assert_eq!(seen.sample_rate, 48_000);
    assert_eq!(seen.frame_bytes, 4);
    assert_eq!(seen.format, SampleFormat::S16Le);

    // Guest pushes PCM; host reads the exact bytes back through the other map.
    let payload: Vec<u8> = (0..64).map(|i| (i * 3) as u8).collect();
    guest.push_overwrite(&payload);

    let mut out = vec![0u8; 64];
    let n = host.read_quantum(&mut out, 1 << 20);
    assert_eq!(n, 64, "host must read all 64 bytes the guest wrote");
    assert_eq!(out, payload, "payload must survive the cross-mapping round-trip");

    let _ = std::fs::remove_file(&path);
}
