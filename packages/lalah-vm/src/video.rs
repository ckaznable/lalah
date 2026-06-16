//! DirectShow video keep-alive (OBS-style).
//!
//! Many capture cards (e.g. AVerMedia Live Gamer 4K / GC573) only start their
//! audio stream once video capture is active — if nothing opens the video pin,
//! the WASAPI audio endpoint reports/produces nothing. OBS keeps the card
//! streaming by opening the video source; we do the same here: open the video
//! capture device with DirectShow's Filter Graph and run it on a background
//! thread, purely to keep the audio pin alive. No frames are decoded or stored.
//!
//! All COM objects are created and used inside the worker thread, so nothing
//! that is not `Send` ever crosses a thread boundary.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::DirectShow::{
    FilgraphManager, IBaseFilter, ICaptureGraphBuilder2, ICreateDevEnum, IGraphBuilder,
    IMediaControl, IMediaEvent, EC_COMPLETE, EC_ERRORABORT, EC_USERABORT,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IEnumMoniker, IMoniker, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{Interface, Result as WinResult, GUID, PCWSTR};

// CLSIDs that are only exposed under `Win32_Media_MediaFoundation` in windows-rs
// 0.62, so we declare them as raw GUID constants to avoid pulling in that feature.
const CLSID_CAPTURE_GRAPH_BUILDER2: GUID =
    GUID::from_u128(0xbf87b6e1_8c27_11d0_b3f0_00aa003761c5);
const CLSID_SYSTEM_DEVICE_ENUM: GUID =
    GUID::from_u128(0x62be5d10_60eb_11d0_bd3b_00a0c911ce86);
const CLSID_VIDEO_INPUT_DEVICE_CATEGORY: GUID =
    GUID::from_u128(0x860bb310_5d01_11d0_bd3b_00a0c911ce86);
/// PIN_CATEGORY_CAPTURE — the correct *pin* category for RenderStream.
/// (AM_KSCATEGORY_CAPTURE is a *device* category and will cause E_INVALIDARG.)
const PIN_CATEGORY_CAPTURE: GUID =
    GUID::from_u128(0xfb6c4281_0353_11d1_905f_0000c0cc16ba);
/// CLSID_NullRenderer — renders nothing and creates no window.
/// We pass this explicitly to RenderStream so DirectShow doesn't fall back to
/// the default Video Renderer (which opens a visible window).
const CLSID_NULL_RENDERER: GUID =
    GUID::from_u128(0xc1f400a4_3f08_11d3_9f0b_006008039e37);
/// S_FALSE HRESULT (0x00000001) — returned by CreateClassEnumerator / Next
/// when the enumeration is empty or exhausted.
const S_FALSE: i32 = 1;

/// A running video keep-alive. Dropping (or calling [`stop`](Self::stop)) signals
/// the worker thread to stop and joins it.
pub struct VideoKeepAlive {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl VideoKeepAlive {
    /// Start the keep-alive. `name_filter`, if given, selects the video capture
    /// device whose friendly name contains it (case-insensitive); otherwise the
    /// first video device is used. Returns once the worker has opened the device
    /// (or failed to).
    pub fn start(name_filter: Option<String>, use_null_renderer: bool) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<String, String>>(1);

        let handle = std::thread::Builder::new()
            .name("video-keepalive".into())
            .spawn(move || worker(stop_worker, name_filter, use_null_renderer, tx))
            .map_err(|e| format!("spawn video thread: {e}"))?;

        match rx.recv() {
            Ok(Ok(name)) => {
                println!(
                    "lalah-vm: video keep-alive streaming \"{name}\" (DirectShow graph running)."
                );
                Ok(Self {
                    stop,
                    handle: Some(handle),
                })
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err("video keep-alive thread exited before reporting".into())
            }
        }
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for VideoKeepAlive {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(
    stop: Arc<AtomicBool>,
    name_filter: Option<String>,
    use_null_renderer: bool,
    tx: SyncSender<Result<String, String>>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED); // S_FALSE if already init
    }

    match open_graph(name_filter.as_deref(), use_null_renderer) {
        Ok((media_control, media_event, name)) => {
            let _ = tx.send(Ok(name));
            keep_alive_loop(&media_control, &media_event, &stop);

            // Stop the graph cleanly before COM objects are released.
            unsafe {
                let _ = media_control.Stop();
            }
        }
        Err(e) => {
            let _ = tx.send(Err(format!("open video device: {e}")));
        }
    }
}

/// Enumerate DirectShow video capture devices, pick one, build a Filter Graph,
/// add the capture filter, render its video stream (DirectShow inserts a Null
/// Renderer automatically), and start running.
///
/// Returns `(IMediaControl, IMediaEvent, device_name)`.
fn open_graph(name_filter: Option<&str>, use_null_renderer: bool) -> WinResult<(IMediaControl, IMediaEvent, String)> {
    unsafe {
        // ── 1. Enumerate video capture devices via ICreateDevEnum ────────────
        let dev_enum: ICreateDevEnum =
            CoCreateInstance(&CLSID_SYSTEM_DEVICE_ENUM, None, CLSCTX_INPROC_SERVER)?;

        // Call the vtable directly so we get the raw HRESULT and can
        // distinguish S_OK (has devices) from S_FALSE (empty category).
        let mut enum_moniker_raw: Option<IEnumMoniker> = None;
        let create_hr = (Interface::vtable(&dev_enum).CreateClassEnumerator)(
            Interface::as_raw(&dev_enum),
            &CLSID_VIDEO_INPUT_DEVICE_CATEGORY,
            &mut enum_moniker_raw as *mut _ as *mut *mut _,
            0,
        );
        if create_hr.0 == S_FALSE || create_hr.is_err() {
            eprintln!("lalah-vm: no video capture devices found (empty category).");
            return Err(windows::core::Error::from(E_FAIL));
        }

        let enum_moniker =
            enum_moniker_raw.ok_or_else(|| windows::core::Error::from(E_FAIL))?;

        // Walk monikers to find the requested device.
        let mut chosen_moniker: Option<IMoniker> = None;
        let mut chosen_name = String::new();
        let mut seen: Vec<String> = Vec::new();

        let mut buf = [None::<IMoniker>; 1];
        loop {
            // Next() returns S_FALSE when enumeration is exhausted.
            let next_hr = (Interface::vtable(&enum_moniker).Next)(
                Interface::as_raw(&enum_moniker),
                1,
                buf.as_mut_ptr() as *mut *mut _,
                std::ptr::null_mut(),
            );
            if next_hr.0 == S_FALSE || next_hr.is_err() {
                break;
            }
            let moniker = match buf[0].take() {
                Some(m) => m,
                None => break,
            };

            let name = device_name_from_moniker(&moniker).unwrap_or_default();
            seen.push(name.clone());

            let is_match = name_filter
                .is_none_or(|f| name.to_lowercase().contains(&f.to_lowercase()));

            if chosen_moniker.is_none() && is_match {
                chosen_moniker = Some(moniker);
                chosen_name = name;
            }
        }

        let moniker = match chosen_moniker {
            Some(m) => m,
            None => {
                eprintln!("lalah-vm: no matching video capture device. Seen: {seen:?}");
                return Err(windows::core::Error::from(E_FAIL));
            }
        };

        // ── 2. Bind moniker → IBaseFilter (the capture filter) ───────────────
        let capture_filter: IBaseFilter = moniker.BindToObject(None, None)?;

        // ── 3. Create the Filter Graph Manager ───────────────────────────────
        let graph: IGraphBuilder =
            CoCreateInstance(&FilgraphManager, None, CLSCTX_INPROC_SERVER)?;

        // ── 4. Create and initialise the Capture Graph Builder ───────────────
        let capture_builder: ICaptureGraphBuilder2 =
            CoCreateInstance(&CLSID_CAPTURE_GRAPH_BUILDER2, None, CLSCTX_INPROC_SERVER)?;
        capture_builder.SetFiltergraph(&graph)?;

        // ── 5. Add the capture filter to the graph ───────────────────────────
        let filter_name: Vec<u16> = chosen_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        graph.AddFilter(&capture_filter, PCWSTR(filter_name.as_ptr()))?;

        let renderer = if use_null_renderer {
            // ── 6. Create a NullRenderer and add it to the graph ────────────────
            // We must pass an explicit NullRenderer to RenderStream; if we pass
            // None, DirectShow falls back to the default Video Renderer which
            // opens a visible window — exactly what we want to avoid.
            let null_renderer: IBaseFilter =
                CoCreateInstance(&CLSID_NULL_RENDERER, None, CLSCTX_INPROC_SERVER)?;
            graph.AddFilter(&null_renderer, PCWSTR::null())?;
            Some(null_renderer)
        } else {
            None
        };

        // ── 7. Render the capture stream through the renderer ─────────────────
        // pcategory must be a *pin* category (PIN_CATEGORY_CAPTURE), NOT the
        // device category (AM_KSCATEGORY_CAPTURE) — the latter causes E_INVALIDARG.
        // ptype is left null so any video media type is accepted.
        capture_builder.RenderStream(
            Some(&PIN_CATEGORY_CAPTURE),
            std::ptr::null(), // accept any media type
            &capture_filter,
            None,             // no intermediate compressor
            renderer.as_ref(), // If Some(&null_renderer) -> no window, frames discarded; If None -> default renderer (window)
        )?;

        // ── 7. Query IMediaControl and IMediaEvent from the graph ─────────────
        let media_control: IMediaControl = graph.cast()?;
        let media_event: IMediaEvent = graph.cast()?;

        // ── 8. Start the graph ───────────────────────────────────────────────
        media_control.Run()?;

        Ok((media_control, media_event, chosen_name))
    }
}

/// Read the friendly name of a DirectShow device moniker via its property bag.
fn device_name_from_moniker(moniker: &IMoniker) -> Option<String> {
    unsafe {
        let prop_bag: IPropertyBag = moniker.BindToStorage(None, None).ok()?;
        let key: Vec<u16> = "FriendlyName\0".encode_utf16().collect();
        let mut var = VARIANT::default();
        prop_bag
            .Read(PCWSTR(key.as_ptr()), &mut var, None)
            .ok()?;
        // The VARIANT holds a BSTR for string-valued property bag entries.
        // BSTR implements Display, so to_string() always succeeds.
        Some(var.Anonymous.Anonymous.Anonymous.bstrVal.to_string())
    }
}

/// Poll `IMediaEvent` for fatal graph events; sleep otherwise until told to
/// stop. The graph keeps running (and the capture pin alive) as long as this
/// function holds `IMediaControl` without calling `Stop()`.
fn keep_alive_loop(
    media_control: &IMediaControl,
    media_event: &IMediaEvent,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Relaxed) {
        let mut event_code: i32 = 0;
        let mut param1: isize = 0;
        let mut param2: isize = 0;

        // Non-blocking poll — timeout = 0 ms.
        let got_event = unsafe {
            media_event
                .GetEvent(&mut event_code, &mut param1, &mut param2, 0)
                .is_ok()
        };

        if got_event {
            unsafe {
                let _ = media_event.FreeEventParams(event_code, param1, param2);
            }
            // EC_* constants are u32; event_code from GetEvent is i32.
            let code = event_code as u32;
            if code == EC_COMPLETE || code == EC_ERRORABORT || code == EC_USERABORT {
                // Graph halted for some reason — restart it.
                unsafe {
                    let _ = media_control.Stop();
                    let _ = media_control.Run();
                }
            }
        } else {
            // No event ready — avoid a tight spin loop.
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
