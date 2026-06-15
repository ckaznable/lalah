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

/// (rate, channels, bits, is_float) probe candidates for exclusive mode, best
/// first. The first one the device accepts wins.
const CANDIDATES: &[(u32, u16, u16, bool)] = &[
    (48_000, 2, 16, false),
    (44_100, 2, 16, false),
    (48_000, 1, 16, false),
    (48_000, 2, 32, false), // 32-bit PCM
    (48_000, 2, 32, true),  // 32-bit IEEE float
];

/// Build a WAVEFORMATEXTENSIBLE describing the candidate format.
fn make_wfx(rate: u32, channels: u16, bits: u16, is_float: bool) -> WAVEFORMATEXTENSIBLE {
    let block_align = channels * (bits / 8);
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: bits,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: bits,
        },
        dwChannelMask: 0,
        SubFormat: if is_float {
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            KSDATAFORMAT_SUBTYPE_PCM
        },
    }
}

fn audio_format(rate: u32, channels: u16, bits: u16, is_float: bool) -> AudioFormat {
    let format = if is_float {
        SampleFormat::F32Le
    } else if bits == 16 {
        SampleFormat::S16Le
    } else {
        SampleFormat::S32Le
    };
    AudioFormat {
        sample_rate: rate,
        channels: channels as u32,
        format,
        frame_bytes: (channels * (bits / 8)) as u32,
    }
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

        // STEP 6: probe for a supported exclusive format (accept only S_OK).
        let mut chosen: Option<(WAVEFORMATEXTENSIBLE, u32, u16, u16, bool)> = None;
        for &(rate, channels, bits, is_float) in CANDIDATES {
            let wfx = make_wfx(rate, channels, bits, is_float);
            let hr = audio_client.IsFormatSupported(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                &raw const wfx.Format,
                None,
            );
            if hr == S_OK {
                chosen = Some((wfx, rate, channels, bits, is_float));
                break;
            }
        }
        let (wfx, rate, channels, bits, is_float) = match chosen {
            Some(c) => c,
            None => return Err(AUDCLNT_E_UNSUPPORTED_FORMAT.into()),
        };
        let block_align = (channels * (bits / 8)) as usize;

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
        ring.set_format(audio_format(rate, channels, bits, is_float));
        println!(
            "lalah-vm: capturing exclusive {} Hz, {} ch, {} bit{} (block {} B).",
            rate,
            channels,
            bits,
            if is_float { " float" } else { "" },
            block_align
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
