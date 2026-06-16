//! Media Foundation video keep-alive (OBS-style).
//!
//! Many capture cards (e.g. AVerMedia Live Gamer 4K / GC573) only start their
//! audio stream once video capture is active — if nothing opens the video pin,
//! the WASAPI audio endpoint reports/produces nothing. OBS keeps the card
//! streaming by opening the video source; we do the same here: open the video
//! capture device with Media Foundation's `IMFSourceReader` and read & DISCARD
//! every frame on a background thread, purely to keep the audio pin alive.
//!
//! All COM/MF objects are created and used inside the worker thread, so nothing
//! that is not `Send` ever crosses a thread boundary.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{Result as WinResult, PWSTR};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFSample, IMFSourceReader, MFCreateAttributes,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFShutdown, MFStartup,
    MFSTARTUP_LITE, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

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
    pub fn start(name_filter: Option<String>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<String, String>>(1);

        let handle = std::thread::Builder::new()
            .name("video-keepalive".into())
            .spawn(move || worker(stop_worker, name_filter, tx))
            .map_err(|e| format!("spawn video thread: {e}"))?;

        match rx.recv() {
            Ok(Ok(name)) => {
                println!("lalah-vm: video keep-alive streaming \"{name}\" (frames discarded).");
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

fn worker(stop: Arc<AtomicBool>, name_filter: Option<String>, tx: SyncSender<Result<String, String>>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED); // S_FALSE if already init
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_LITE) {
            let _ = tx.send(Err(format!("MFStartup failed: {e}")));
            return;
        }
    }

    match open_reader(name_filter.as_deref()) {
        Ok((reader, name)) => {
            let _ = tx.send(Ok(name));
            discard_loop(&reader, &stop);
        }
        Err(e) => {
            let _ = tx.send(Err(format!("open video device: {e}")));
        }
    }

    unsafe {
        let _ = MFShutdown();
    }
}

/// Enumerate video capture devices, pick one, and build a source reader.
fn open_reader(name_filter: Option<&str>) -> WinResult<(IMFSourceReader, String)> {
    unsafe {
        // Attributes: request video-capture device sources.
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;

        // Enumerate. MF allocates an array of IMFActivate we must free.
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFEnumDeviceSources(&attrs, &mut arr, &mut count)?;

        let mut chosen: Option<(IMFActivate, String)> = None;
        let mut seen: Vec<String> = Vec::new();
        for i in 0..count as usize {
            // Move each element out (leaving None) so the un-chosen ones are
            // released when dropped; the array memory is freed afterwards.
            if let Some(act) = (*arr.add(i)).take() {
                let name = device_name(&act).unwrap_or_default();
                seen.push(name.clone());
                let is_match =
                    name_filter.is_none_or(|f| name.to_lowercase().contains(&f.to_lowercase()));
                if chosen.is_none() && is_match {
                    chosen = Some((act, name));
                }
            }
        }
        if !arr.is_null() {
            CoTaskMemFree(Some(arr as *const core::ffi::c_void));
        }

        let (activate, name) = match chosen {
            Some(c) => c,
            None => {
                eprintln!("lalah-vm: no matching video capture device. Seen: {seen:?}");
                return Err(windows::core::Error::from(E_FAIL));
            }
        };

        let source: IMFMediaSource = activate.ActivateObject()?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, None::<&IMFAttributes>)?;

        // Select only the first video stream.
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;

        Ok((reader, name))
    }
}

/// Read the device's friendly name (a CoTaskMem-allocated wide string).
fn device_name(activate: &IMFActivate) -> Option<String> {
    unsafe {
        let mut pwstr = PWSTR::null();
        let mut len: u32 = 0;
        activate
            .GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut pwstr, &mut len)
            .ok()?;
        let s = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.0 as *const core::ffi::c_void));
        s
    }
}

/// Pull frames and immediately discard them until told to stop. `ReadSample`
/// blocks until a frame is ready, which is exactly what keeps the card streaming.
fn discard_loop(reader: &IMFSourceReader, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let mut flags: u32 = 0;
        let mut sample: Option<IMFSample> = None;
        let r = unsafe {
            reader.ReadSample(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                0,
                None,
                Some(&mut flags as *mut u32),
                None,
                Some(&mut sample as *mut Option<IMFSample>),
            )
        };
        match r {
            Ok(()) => drop(sample), // discard the frame
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}
