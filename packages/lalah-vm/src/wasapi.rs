//! WASAPI exclusive, event-driven capture of a GIVEN endpoint id.
//!
//! Flow (see Microsoft's exclusive-mode event-driven capture pattern):
//!   CoInitializeEx -> resolve device by id -> Activate IAudioClient ->
//!   GetDevicePeriod (min) -> probe exclusive formats via IsFormatSupported ->
//!   Initialize(EXCLUSIVE|EVENTCALLBACK) with the buffer-size-alignment retry ->
//!   SetEventHandle -> GetService(IAudioCaptureClient) -> publish format ->
//!   Start -> { WaitForSingleObject; drain GetBuffer/ReleaseBuffer } loop.

use shared::{AudioFormat, SampleFormat, ShmAudioBuffer};
use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_UNSUPPORTED_FORMAT,
    AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// 100-nanosecond units per second (the REFERENCE_TIME unit).
const REFTIMES_PER_SEC: f64 = 10_000_000.0;
/// Wake-up timeout per capture cycle; treated as a stall if exceeded.
const WAIT_TIMEOUT_MS: u32 = 2000;

// Plain WAVEFORMATEX format tags. Many capture cards (e.g. AVerMedia GC573)
// accept ONLY the plain WAVEFORMATEX form in exclusive mode and reject
// WAVEFORMATEXTENSIBLE for 16-bit stereo — so we probe plain PCM first. The
// "Advanced" tab in mmsys.cpl shows exactly this plain format.
const WAVE_FORMAT_PCM_TAG: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 3;

/// Base formats to probe, best first. Each is tried as plain WAVEFORMATEX first,
/// then as WAVEFORMATEXTENSIBLE. The first the device accepts (S_OK) wins.
const BASE_FORMATS: &[(u32, u16, u16, bool)] = &[
    (48_000, 2, 16, false),
    (44_100, 2, 16, false),
    (48_000, 1, 16, false),
    (48_000, 2, 32, false), // 32-bit PCM
    (48_000, 2, 32, true),  // 32-bit IEEE float
];

#[derive(Clone, Copy)]
struct Candidate {
    rate: u32,
    channels: u16,
    bits: u16,
    is_float: bool,
    extensible: bool,
}

/// Build WAVEFORMATEXTENSIBLE storage configured either as a plain WAVEFORMATEX
/// (`cbSize = 0`, PCM/FLOAT tag — the trailing fields are ignored) or a true
/// WAVEFORMATEXTENSIBLE (`cbSize = 22`, SubFormat set). Using the larger struct
/// as storage keeps the pointer correctly aligned for `Initialize`.
fn make_wfx(c: Candidate) -> WAVEFORMATEXTENSIBLE {
    let block_align = c.channels * (c.bits / 8);
    let tag = if c.extensible {
        WAVE_FORMAT_EXTENSIBLE as u16
    } else if c.is_float {
        WAVE_FORMAT_IEEE_FLOAT_TAG
    } else {
        WAVE_FORMAT_PCM_TAG
    };
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: tag,
            nChannels: c.channels,
            nSamplesPerSec: c.rate,
            nAvgBytesPerSec: c.rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: c.bits,
            cbSize: if c.extensible { 22 } else { 0 },
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: c.bits,
        },
        dwChannelMask: if c.channels >= 2 { 0x3 } else { 0x4 },
        SubFormat: if c.is_float {
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            KSDATAFORMAT_SUBTYPE_PCM
        },
    }
}

/// Parse the chosen format blob into our wire [`AudioFormat`]. Returns `None` for
/// formats we don't carry (e.g. 24-bit). Reads packed fields BY VALUE only
/// (taking a reference to a packed field is UB).
fn parse_audio_format(w: &WAVEFORMATEXTENSIBLE) -> Option<AudioFormat> {
    let tag = w.Format.wFormatTag;
    let bits = w.Format.wBitsPerSample;
    let channels = w.Format.nChannels;
    let rate = w.Format.nSamplesPerSec;
    let block = w.Format.nBlockAlign;
    let is_float = if tag == WAVE_FORMAT_IEEE_FLOAT_TAG {
        true
    } else if tag == WAVE_FORMAT_PCM_TAG {
        false
    } else if tag == WAVE_FORMAT_EXTENSIBLE as u16 {
        let sub = w.SubFormat; // copy out of the packed struct before comparing
        sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        return None;
    };
    let format = if is_float {
        if bits != 32 {
            return None;
        }
        SampleFormat::F32Le
    } else if bits == 16 {
        SampleFormat::S16Le
    } else if bits == 32 {
        SampleFormat::S32Le
    } else {
        return None;
    };
    Some(AudioFormat {
        sample_rate: rate,
        channels: channels as u32,
        format,
        frame_bytes: block as u32,
    })
}

/// Capture from `device_id` and push PCM into `ring`. Blocks until the stream
/// stalls (no buffer within the timeout) or errors.
pub fn capture_exclusive(device_id: &str, ring: &mut ShmAudioBuffer) -> Result<()> {
    unsafe {
        // STEP 1: COM init (MTA). S_FALSE (already initialized) counts as success.
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        // STEP 2-3: resolve the endpoint by its id (no enumeration/data-flow).
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let id_w: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
        // The id refers to a capture (input) endpoint; GetDevice resolves it by
        // id directly, so no data-flow/enumeration is needed.
        let device = enumerator.GetDevice(PCWSTR(id_w.as_ptr()))?;

        // STEP 4: activate an IAudioClient on the endpoint.
        let mut audio_client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;

        // STEP 5: minimum device period for lowest latency.
        let mut min_period: i64 = 0;
        audio_client.GetDevicePeriod(None, Some(&mut min_period))?;

        // STEP 6: probe for a supported exclusive format. Each base format is
        // tried as plain WAVEFORMATEX first, then WAVEFORMATEXTENSIBLE; only S_OK
        // counts. Every probe is logged so a failing device shows its verdicts.
        let mut chosen: Option<WAVEFORMATEXTENSIBLE> = None;
        'probe: for &(rate, channels, bits, is_float) in BASE_FORMATS {
            for extensible in [false, true] {
                let c = Candidate {
                    rate,
                    channels,
                    bits,
                    is_float,
                    extensible,
                };
                let wfx = make_wfx(c);
                let hr = audio_client.IsFormatSupported(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    &raw const wfx.Format,
                    None,
                );
                println!(
                    "lalah-vm: probe {} Hz {} ch {} bit{} [{}] -> 0x{:08X}",
                    rate,
                    channels,
                    bits,
                    if is_float { " float" } else { "" },
                    if extensible { "EXTENSIBLE" } else { "PCM" },
                    hr.0 as u32,
                );
                if hr == S_OK {
                    chosen = Some(wfx);
                    break 'probe;
                }
            }
        }
        let wfx = match chosen {
            Some(w) => w,
            None => return Err(AUDCLNT_E_UNSUPPORTED_FORMAT.into()),
        };
        let fmt = parse_audio_format(&wfx)
            .ok_or_else(|| windows::core::Error::from(AUDCLNT_E_UNSUPPORTED_FORMAT))?;
        let rate = fmt.sample_rate;
        let block_align = fmt.frame_bytes as usize;

        // STEP 7-9: initialize, with the buffer-size alignment retry.
        let mut hns = min_period;
        let init = |client: &IAudioClient, dur: i64| {
            client.Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                dur,
                dur,
                &raw const wfx.Format,
                None,
            )
        };
        if let Err(e) = init(&audio_client, hns) {
            if e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
                // The client is now unusable; recompute the aligned period from
                // the buffer size it reports and re-activate a fresh client.
                let n_frames = audio_client.GetBufferSize()?;
                hns = (REFTIMES_PER_SEC / rate as f64 * n_frames as f64 + 0.5) as i64;
                audio_client = device.Activate(CLSCTX_ALL, None)?;
                init(&audio_client, hns)?;
            } else {
                return Err(e);
            }
        }

        // STEP 10: event handle for event-driven capture.
        let h_event = CreateEventW(None, false, false, PCWSTR::null())?;
        audio_client.SetEventHandle(h_event)?;

        // STEP 11: buffer size + capture service.
        let _buffer_frames = audio_client.GetBufferSize()?;
        let capture: IAudioCaptureClient = audio_client.GetService()?;

        // Publish the negotiated format BEFORE Start so the host can configure
        // PipeWire and begin consuming.
        ring.set_format(fmt);
        println!(
            "lalah-vm: capturing exclusive {} Hz, {} ch, {:?} (frame {} B).",
            fmt.sample_rate, fmt.channels, fmt.format, fmt.frame_bytes
        );

        // STEP 12: go.
        audio_client.Start()?;

        // STEP 13: capture loop.
        let mut silence: Vec<u8> = Vec::new();
        let result = capture_loop(&capture, h_event, block_align, ring, &mut silence);

        // STEP 14: teardown.
        let _ = audio_client.Stop();
        let _ = CloseHandle(h_event);
        result
    }
}

/// Inner loop, split out so teardown always runs.
unsafe fn capture_loop(
    capture: &IAudioCaptureClient,
    h_event: HANDLE,
    block_align: usize,
    ring: &mut ShmAudioBuffer,
    silence: &mut Vec<u8>,
) -> Result<()> {
    use windows::Win32::Foundation::WAIT_OBJECT_0;

    loop {
        // Wait for the next packet. A bounded wait (vs INFINITE) lets us treat a
        // dead/stalled device as a clean stop instead of hanging forever.
        let wait = unsafe { WaitForSingleObject(h_event, WAIT_TIMEOUT_MS) };
        if wait != WAIT_OBJECT_0 {
            return Ok(()); // timeout / abandoned -> stop cleanly
        }

        // Drain every queued packet before going back to wait.
        loop {
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
            if frames == 0 {
                break;
            }
            let bytes = frames as usize * block_align;
            if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                // The data pointer is undefined for silent packets — emit zeros so
                // the timeline stays continuous.
                if silence.len() < bytes {
                    silence.resize(bytes, 0);
                }
                ring.push_overwrite(&silence[..bytes]);
            } else {
                let slice = unsafe { std::slice::from_raw_parts(data, bytes) };
                ring.push_overwrite(slice);
            }
            unsafe { capture.ReleaseBuffer(frames)? };
        }
    }
}
