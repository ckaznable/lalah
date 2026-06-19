//! DirectShow audio capture (the OBS approach for capture cards).
//!
//! Capture cards like the AVerMedia GC573 expose their audio through a DirectShow
//! audio-capture filter, not (reliably) through a WASAPI endpoint. This module
//! builds a graph `source -> SampleGrabber -> NullRenderer`, where the Sample
//! Grabber's `BufferCB` callback hands us raw PCM that we push straight into the
//! shared ring — same downstream path as the WASAPI producer.
//!
//! The Sample Grabber / Null Renderer (qedit) interfaces are not in windows-rs,
//! so `ISampleGrabber` / `ISampleGrabberCB` are defined here by IID with
//! `#[interface]`, and the callback is implemented with `#[implement]`.
//!
//! NOTE: the DShow constants/structs (`AM_MEDIA_TYPE`, `CLSID_FilterGraph`,
//! `MEDIATYPE_Audio`, …) live in windows-rs's `Media::MediaFoundation` module.

// COM interface methods must match the vtable names (PascalCase).
#![allow(non_snake_case)]
// AM_MEDIA_TYPE has no ergonomic struct literal (many COM fields), so the
// Default::default() + field-assignment pattern is how we build media types.
#![allow(clippy::field_reassign_with_default)]

use std::ffi::c_void;
use std::sync::Mutex;
use std::time::Duration;

use shared::{AudioFormat, SampleFormat, ShmAudioBuffer, ShmVideoBuffer, VideoFormat};
// The `#[interface]`/`#[implement]` macros expand to `windows_core::` crate
// paths (hence the direct `windows-core` dep). `IUnknown_Vtbl` is referenced by
// the generated vtable for an `: IUnknown` interface, so it must be in scope.
use windows::core::{
    implement, interface, Interface, Result, BOOL, GUID, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR,
};
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, S_OK};
use windows::Win32::Media::Audio::{WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE};
use windows::Win32::Media::MediaFoundation::{
    AM_MEDIA_TYPE, CLSID_AudioInputDeviceCategory, CLSID_CaptureGraphBuilder2, CLSID_FilterGraph,
    CLSID_SystemDeviceEnum, FORMAT_VideoInfo, FORMAT_WaveFormatEx, MEDIASUBTYPE_NV12,
    MEDIASUBTYPE_PCM, MEDIATYPE_Audio, MEDIATYPE_Video, PIN_CATEGORY_CAPTURE, VIDEOINFOHEADER,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::Media::DirectShow::{
    IBaseFilter, ICaptureGraphBuilder2, ICreateDevEnum, IGraphBuilder, IMediaControl,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CreateBindCtx, IBindCtx, IEnumMoniker,
    IMoniker, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BSTR};

// qedit CLSIDs / IIDs (not in windows-rs).
const CLSID_SAMPLE_GRABBER: GUID = GUID::from_u128(0xc1f400a0_3f08_11d3_9f0b_006008039e37);
const CLSID_NULL_RENDERER: GUID = GUID::from_u128(0xc1f400a4_3f08_11d3_9f0b_006008039e37);
const CLSID_VIDEO_INPUT_DEVICE_CATEGORY: GUID = GUID::from_u128(0x860bb310_5d01_11d0_bd3b_00a0c911ce86);

/// `ISampleGrabber` — the subset we call.
#[interface("6b652fff-11fe-4fce-92ad-0266b5d7c78f")]
unsafe trait ISampleGrabber: IUnknown {
    fn SetOneShot(&self, one_shot: i32) -> HRESULT;
    fn SetMediaType(&self, p_type: *const AM_MEDIA_TYPE) -> HRESULT;
    fn GetConnectedMediaType(&self, p_type: *mut AM_MEDIA_TYPE) -> HRESULT;
    fn SetBufferSamples(&self, buffer_them: i32) -> HRESULT;
    fn GetCurrentBuffer(&self, p_buffer_size: *mut i32, p_buffer: *mut i32) -> HRESULT;
    fn GetCurrentSample(&self, pp_sample: *mut *mut c_void) -> HRESULT;
    fn SetCallback(&self, p_callback: *mut c_void, which_method: i32) -> HRESULT;
}

/// `ISampleGrabberCB` — implemented by us; DShow calls `BufferCB` per buffer.
#[interface("0579154a-2b53-4994-b0d0-e773148eff85")]
unsafe trait ISampleGrabberCB: IUnknown {
    fn SampleCB(&self, sample_time: f64, p_sample: *mut c_void) -> HRESULT;
    fn BufferCB(&self, sample_time: f64, p_buffer: *mut u8, buffer_len: i32) -> HRESULT;
}

/// Our callback: pushes each delivered PCM buffer into the ring. DShow invokes
/// `BufferCB` on its streaming thread; the `Mutex` makes that sound (contention
/// is nil — only this one thread touches the ring).
#[implement(ISampleGrabberCB)]
struct GrabberCb {
    ring: Mutex<ShmAudioBuffer>,
}

impl ISampleGrabberCB_Impl for GrabberCb_Impl {
    unsafe fn SampleCB(&self, _time: f64, _sample: *mut c_void) -> HRESULT {
        E_NOTIMPL
    }

    unsafe fn BufferCB(&self, _time: f64, buffer: *mut u8, len: i32) -> HRESULT {
        if !buffer.is_null() && len > 0 {
            let slice = unsafe { std::slice::from_raw_parts(buffer, len as usize) };
            if let Ok(mut ring) = self.ring.lock() {
                ring.push_overwrite(slice);
            }
        }
        S_OK
    }
}

/// Video variant: pushes each grabbed raw frame into the shared video ring. Same
/// threading model as [`GrabberCb`] — DShow calls `BufferCB` on one streaming
/// thread, so the `Mutex` is uncontended.
#[implement(ISampleGrabberCB)]
struct VideoGrabberCb {
    vid: Mutex<ShmVideoBuffer>,
}

impl ISampleGrabberCB_Impl for VideoGrabberCb_Impl {
    unsafe fn SampleCB(&self, _time: f64, _sample: *mut c_void) -> HRESULT {
        E_NOTIMPL
    }

    unsafe fn BufferCB(&self, _time: f64, buffer: *mut u8, len: i32) -> HRESULT {
        if !buffer.is_null() && len > 0 {
            let slice = unsafe { std::slice::from_raw_parts(buffer, len as usize) };
            if let Ok(mut vid) = self.vid.lock() {
                vid.push_frame(slice);
            }
        }
        S_OK
    }
}

/// Capture audio from a DirectShow audio device into `ring`. Blocks (the graph
/// runs on its own threads) until the process is terminated. `audio_device`, if
/// given, selects the audio device whose friendly name contains it.
///
/// A video device is opened in the SAME Filter Graph when either:
///  * `video_ring` is `Some` — EXPERIMENTAL passthrough: a video SampleGrabber
///    forwards raw frames into the shared video region; or
///  * `video_renderer_type` is `Some` (keep-alive only): the video is rendered to
///    a null/window renderer and discarded, purely to keep the card streaming.
pub fn capture_dshow_audio(
    audio_device: Option<&str>,
    video_device: Option<&str>,
    video_renderer_type: Option<&str>,
    video_ring: Option<ShmVideoBuffer>,
    ring: ShmAudioBuffer,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        // Filter graph + capture-graph builder + media control.
        let graph: IGraphBuilder = CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER)?;
        let builder: ICaptureGraphBuilder2 =
            CoCreateInstance(&CLSID_CaptureGraphBuilder2, None, CLSCTX_INPROC_SERVER)?;
        builder.SetFiltergraph(&graph)?;
        let control: IMediaControl = graph.cast()?;

        // A video source is needed for passthrough OR keep-alive.
        let want_video = video_renderer_type.is_some() || video_ring.is_some();
        let video_source_opt = if want_video {
            match find_video_source(video_device) {
                Ok(video_source) => {
                    graph.AddFilter(&video_source, PCWSTR::null())?;
                    Some(video_source)
                }
                Err(e) => {
                    eprintln!("lalah-vm(dshow): failed to find or add video device: {e}");
                    None
                }
            }
        } else {
            None
        };

        // Sample grabber, asking for uncompressed PCM audio.
        let grabber_filter: IBaseFilter =
            CoCreateInstance(&CLSID_SAMPLE_GRABBER, None, CLSCTX_INPROC_SERVER)?;
        let grabber: ISampleGrabber = grabber_filter.cast()?;

        let mut wfx = WAVEFORMATEX::default();
        wfx.wFormatTag = WAVE_FORMAT_PCM as u16;
        wfx.nChannels = 2;
        wfx.nSamplesPerSec = 48000;
        wfx.wBitsPerSample = 16;
        wfx.nBlockAlign = (wfx.nChannels * wfx.wBitsPerSample) / 8; // 4
        wfx.nAvgBytesPerSec = wfx.nSamplesPerSec * wfx.nBlockAlign as u32; // 192000
        wfx.cbSize = 0;

        let mut want = AM_MEDIA_TYPE::default();
        want.majortype = MEDIATYPE_Audio;
        want.subtype = MEDIASUBTYPE_PCM;
        want.formattype = FORMAT_WaveFormatEx;
        want.bFixedSizeSamples = BOOL::from(true);
        want.lSampleSize = wfx.nBlockAlign as u32;
        want.cbFormat = std::mem::size_of::<WAVEFORMATEX>() as u32;
        want.pbFormat = &mut wfx as *mut _ as *mut u8;

        grabber.SetMediaType(&want).ok()?;
        grabber.SetOneShot(0).ok()?;
        grabber.SetBufferSamples(0).ok()?;
        graph.AddFilter(&grabber_filter, PCWSTR::null())?;

        // Null renderer to terminate the graph.
        let null_renderer: IBaseFilter =
            CoCreateInstance(&CLSID_NULL_RENDERER, None, CLSCTX_INPROC_SERVER)?;
        graph.AddFilter(&null_renderer, PCWSTR::null())?;

        // Connect source -> grabber -> null for the audio capture pin.
        // 1. Try to route audio directly from the video source filter if available.
        let mut audio_routed = false;
        if let Some(video_source) = &video_source_opt {
            println!("lalah-vm(dshow): attempting to route audio pin from the video capture device...");
            let route_res = builder.RenderStream(
                Some(&PIN_CATEGORY_CAPTURE),
                &MEDIATYPE_Audio,
                video_source,
                &grabber_filter,
                &null_renderer,
            );
            match route_res {
                Ok(_) => {
                    println!("lalah-vm(dshow): successfully routed audio directly from the video device.");
                    audio_routed = true;
                }
                Err(e) => {
                    eprintln!("lalah-vm(dshow): video device has no audio pin or routing failed: {e}");
                }
            }
        }

        // 2. Fallback: find dedicated audio source and render it.
        if !audio_routed {
            println!("lalah-vm(dshow): using separate audio capture device...");
            let source = find_audio_source(audio_device)?;
            graph.AddFilter(&source, PCWSTR::null())?;
            builder.RenderStream(
                Some(&PIN_CATEGORY_CAPTURE),
                &MEDIATYPE_Audio,
                &source,
                &grabber_filter,
                &null_renderer,
            )?;
            println!("lalah-vm(dshow): successfully routed audio from the dedicated audio device.");
        }

        // Optional: video. Held to the end of the function so the SampleGrabber's
        // un-AddRef'd callback pointer stays valid while the graph runs.
        let mut _video_cb: Option<ISampleGrabberCB> = None;
        if video_ring.is_some() && video_source_opt.is_none() {
            eprintln!(
                "lalah-vm(dshow): video passthrough requested but no video device was found; continuing audio-only."
            );
        }
        if let Some(video_source) = &video_source_opt {
            if let Some(mut vid) = video_ring {
                // EXPERIMENTAL passthrough: source -> video SampleGrabber -> null,
                // grabbed frames forwarded into the shared video ring.
                let vgf: IBaseFilter =
                    CoCreateInstance(&CLSID_SAMPLE_GRABBER, None, CLSCTX_INPROC_SERVER)?;
                let vgrab: ISampleGrabber = vgf.cast()?;
                // Prefer NV12; the Sample Grabber will accept NV12 if the source
                // offers it, otherwise a converter is inserted.
                let mut vwant = AM_MEDIA_TYPE::default();
                vwant.majortype = MEDIATYPE_Video;
                vwant.subtype = MEDIASUBTYPE_NV12;
                vgrab.SetMediaType(&vwant).ok()?;
                vgrab.SetOneShot(0).ok()?;
                vgrab.SetBufferSamples(0).ok()?;
                graph.AddFilter(&vgf, PCWSTR::null())?;

                let vnull: IBaseFilter =
                    CoCreateInstance(&CLSID_NULL_RENDERER, None, CLSCTX_INPROC_SERVER)?;
                graph.AddFilter(&vnull, PCWSTR::null())?;

                builder.RenderStream(
                    Some(&PIN_CATEGORY_CAPTURE),
                    &MEDIATYPE_Video,
                    video_source,
                    &vgf,
                    &vnull,
                )?;

                let mut vconn = AM_MEDIA_TYPE::default();
                vgrab.GetConnectedMediaType(&mut vconn).ok()?;
                let vf = parse_video_format(&vconn)
                    .ok_or_else(|| windows::core::Error::from(E_FAIL))?;
                free_media_type(&mut vconn);

                match vid.set_geometry(vf) {
                    Ok(slots) => {
                        println!(
                            "lalah-vm(dshow): video {}x{} fourcc {} ({} B/frame), {} ring slots.",
                            vf.width,
                            vf.height,
                            fourcc_str(vf.fourcc),
                            vf.frame_size,
                            slots
                        );
                        let vcb: ISampleGrabberCB = VideoGrabberCb {
                            vid: Mutex::new(vid),
                        }
                        .into();
                        vgrab.SetCallback(vcb.as_raw(), 1).ok()?; // 1 = BufferCB
                        _video_cb = Some(vcb);
                    }
                    Err(e) => {
                        eprintln!(
                            "lalah-vm(dshow): video geometry rejected ({e}); the IVSHMEM region \
                             is too small for >= 3 frame slots past 16 MiB. Continuing audio-only."
                        );
                    }
                }
            } else if let Some(r_type) = video_renderer_type {
                // Keep-alive only: render video to null (no window) or the default
                // renderer (a window), discarding frames.
                let video_renderer = if r_type == "null" {
                    let video_null_renderer: IBaseFilter =
                        CoCreateInstance(&CLSID_NULL_RENDERER, None, CLSCTX_INPROC_SERVER)?;
                    graph.AddFilter(&video_null_renderer, PCWSTR::null())?;
                    Some(video_null_renderer)
                } else {
                    None
                };

                builder.RenderStream(
                    Some(&PIN_CATEGORY_CAPTURE),
                    std::ptr::null(), // accept any media type
                    video_source,
                    None,
                    video_renderer.as_ref(),
                )?;
            }
        }

        // Read the negotiated format from the connection.
        let mut connected = AM_MEDIA_TYPE::default();
        grabber.GetConnectedMediaType(&mut connected).ok()?;
        let fmt = parse_wfx(&connected).ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        free_media_type(&mut connected);

        // Publish the format, then route buffers into the ring via the callback.
        ring.set_format(fmt);
        println!(
            "lalah-vm(dshow): capturing {} Hz, {} ch, {:?} (frame {} B).",
            fmt.sample_rate, fmt.channels, fmt.format, fmt.frame_bytes
        );
        let cb: ISampleGrabberCB = GrabberCb {
            ring: Mutex::new(ring),
        }
        .into();
        grabber.SetCallback(cb.as_raw(), 1).ok()?; // 1 = BufferCB

        control.Run()?;
        if _video_cb.is_some() {
            println!("lalah-vm(dshow): graph running; forwarding audio + video. Ctrl-C to stop.");
        } else {
            println!("lalah-vm(dshow): graph running; forwarding audio. Ctrl-C to stop.");
        }

        // Keep every COM object (incl. the callback) alive while the graph runs.
        loop {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// Enumerate DirectShow audio capture devices and bind the chosen one.
fn find_audio_source(name_filter: Option<&str>) -> Result<IBaseFilter> {
    unsafe {
        let dev_enum: ICreateDevEnum =
            CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)?;
        let mut monikers: Option<IEnumMoniker> = None;
        // S_FALSE (and a null enumerator) means the category is empty.
        // Returns S_FALSE (Ok, but a null enumerator) when the category is empty.
        dev_enum.CreateClassEnumerator(&CLSID_AudioInputDeviceCategory, &mut monikers, 0)?;
        let monikers = monikers.ok_or_else(|| windows::core::Error::from(E_FAIL))?;

        let bind_ctx = CreateBindCtx(0)?;
        let mut seen: Vec<String> = Vec::new();
        loop {
            let mut one: [Option<IMoniker>; 1] = [None];
            if monikers.Next(&mut one, None) != S_OK {
                break;
            }
            let Some(moniker) = one[0].take() else { break };
            let name = moniker_name(&moniker, &bind_ctx).unwrap_or_default();
            let is_match =
                name_filter.is_none_or(|f| name.to_lowercase().contains(&f.to_lowercase()));
            seen.push(name.clone());
            if is_match {
                let filter: IBaseFilter = moniker.BindToObject(&bind_ctx, None::<&IMoniker>)?;
                println!("lalah-vm(dshow): using audio device \"{name}\"");
                return Ok(filter);
            }
        }
        eprintln!("lalah-vm(dshow): no matching audio capture device. Seen: {seen:?}");
        Err(windows::core::Error::from(E_FAIL))
    }
}

/// Enumerate DirectShow video capture devices and bind the chosen one.
fn find_video_source(name_filter: Option<&str>) -> Result<IBaseFilter> {
    unsafe {
        let dev_enum: ICreateDevEnum =
            CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)?;
        let mut monikers: Option<IEnumMoniker> = None;
        dev_enum.CreateClassEnumerator(&CLSID_VIDEO_INPUT_DEVICE_CATEGORY, &mut monikers, 0)?;
        let monikers = monikers.ok_or_else(|| windows::core::Error::from(E_FAIL))?;

        let bind_ctx = CreateBindCtx(0)?;
        let mut seen: Vec<String> = Vec::new();
        loop {
            let mut one: [Option<IMoniker>; 1] = [None];
            if monikers.Next(&mut one, None) != S_OK {
                break;
            }
            let Some(moniker) = one[0].take() else { break };
            let name = moniker_name(&moniker, &bind_ctx).unwrap_or_default();
            let is_match =
                name_filter.is_none_or(|f| name.to_lowercase().contains(&f.to_lowercase()));
            seen.push(name.clone());
            if is_match {
                let filter: IBaseFilter = moniker.BindToObject(&bind_ctx, None::<&IMoniker>)?;
                println!("lalah-vm(dshow): using video device \"{name}\"");
                return Ok(filter);
            }
        }
        eprintln!("lalah-vm(dshow): no matching video capture device. Seen: {seen:?}");
        Err(windows::core::Error::from(E_FAIL))
    }
}

/// NUL-terminated UTF-16 buffer for a `PCWSTR` (kept alive by the caller).
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a moniker's `FriendlyName` via its property bag.
fn moniker_name(moniker: &IMoniker, bind_ctx: &IBindCtx) -> Option<String> {
    unsafe {
        let bag: IPropertyBag = moniker.BindToStorage(bind_ctx, None::<&IMoniker>).ok()?;
        let mut var = VARIANT::default();
        let prop = wide("FriendlyName");
        bag.Read(PCWSTR(prop.as_ptr()), &mut var, None).ok()?;
        let name = if var.Anonymous.Anonymous.vt == VT_BSTR {
            (*var.Anonymous.Anonymous.Anonymous.bstrVal).to_string()
        } else {
            String::new()
        };
        let _ = VariantClear(&mut var);
        (!name.is_empty()).then_some(name)
    }
}

/// Parse the connected `AM_MEDIA_TYPE`'s WAVEFORMATEX into our wire format.
fn parse_wfx(mt: &AM_MEDIA_TYPE) -> Option<AudioFormat> {
    let format_type = mt.formattype;
    if format_type != FORMAT_WaveFormatEx
        || mt.pbFormat.is_null()
        || (mt.cbFormat as usize) < core::mem::size_of::<WAVEFORMATEX>()
    {
        return None;
    }
    // pbFormat may be unaligned, so read the whole struct out by value.
    let wfx = unsafe { core::ptr::read_unaligned(mt.pbFormat as *const WAVEFORMATEX) };
    let tag = wfx.wFormatTag;
    let bits = wfx.wBitsPerSample;
    let channels = wfx.nChannels;
    let rate = wfx.nSamplesPerSec;
    let block = wfx.nBlockAlign;

    let is_float = match tag {
        3 => true,  // WAVE_FORMAT_IEEE_FLOAT
        1 => false, // WAVE_FORMAT_PCM
        0xFFFE => {
            // WAVE_FORMAT_EXTENSIBLE: inspect the SubFormat GUID.
            if (mt.cbFormat as usize) < core::mem::size_of::<WAVEFORMATEXTENSIBLE>() {
                return None;
            }
            let ext = unsafe { core::ptr::read_unaligned(mt.pbFormat as *const WAVEFORMATEXTENSIBLE) };
            let sub = ext.SubFormat;
            sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        }
        _ => return None,
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

/// Parse the connected video `AM_MEDIA_TYPE` (a `VIDEOINFOHEADER`) into our wire
/// geometry. Returns `None` for a non-`FORMAT_VideoInfo` or degenerate type.
fn parse_video_format(mt: &AM_MEDIA_TYPE) -> Option<VideoFormat> {
    if mt.formattype != FORMAT_VideoInfo
        || mt.pbFormat.is_null()
        || (mt.cbFormat as usize) < core::mem::size_of::<VIDEOINFOHEADER>()
    {
        return None;
    }
    // pbFormat may be unaligned, so read the whole struct out by value.
    let vih = unsafe { core::ptr::read_unaligned(mt.pbFormat as *const VIDEOINFOHEADER) };
    let bih = vih.bmiHeader;
    let width = bih.biWidth.unsigned_abs();
    let height = bih.biHeight.unsigned_abs(); // negative => top-down DIB
    let fourcc = bih.biCompression;
    let mut frame_size = bih.biSizeImage;
    if frame_size == 0 {
        // Some sources omit biSizeImage; derive it from the geometry.
        frame_size = width
            .saturating_mul(height)
            .saturating_mul(bih.biBitCount as u32)
            / 8;
    }
    if width == 0 || height == 0 || frame_size == 0 {
        return None;
    }
    let stride = frame_size / height;
    Some(VideoFormat {
        width,
        height,
        stride,
        fourcc,
        frame_size,
    })
}

/// Render a FourCC as its 4 ASCII chars (or `0x…` when not printable), for logs.
fn fourcc_str(fourcc: u32) -> String {
    if fourcc == 0 {
        return "RGB".to_string();
    }
    let b = fourcc.to_le_bytes();
    if b.iter().all(|&c| c.is_ascii_graphic() || c == b' ') {
        String::from_utf8_lossy(&b).trim_end().to_string()
    } else {
        format!("0x{fourcc:08X}")
    }
}

/// Free the format block + any embedded object of an `AM_MEDIA_TYPE` we own.
fn free_media_type(mt: &mut AM_MEDIA_TYPE) {
    unsafe {
        if !mt.pbFormat.is_null() && mt.cbFormat > 0 {
            CoTaskMemFree(Some(mt.pbFormat as *const c_void));
        }
        mt.pbFormat = std::ptr::null_mut();
        mt.cbFormat = 0;
        let _ = std::mem::ManuallyDrop::take(&mut mt.pUnk); // release if present
    }
}
