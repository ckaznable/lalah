//! PipeWire playback: a clock-driven output stream configured from the runtime
//! [`AudioFormat`] negotiated by the VM, whose RT `process` callback pulls a
//! fixed, frame-aligned quantum out of the shared-memory ring each cycle.
//!
//! The API here mirrors the pipewire 0.10 `tone.rs` example (the only public
//! shape that compiles against this version): `MainLoopRc` / `ContextRc` /
//! `connect_rc` / `StreamBox` / `add_local_listener_with_user_data`.

use pipewire as pw;
use pw::spa::sys as spa_sys;
use pw::{properties::properties, spa};
use shared::{AudioFormat, SampleFormat, ShmAudioBuffer};
use spa::pod::Pod;

/// Map our wire format enum onto the SPA audio format.
fn spa_format(fmt: SampleFormat) -> spa::param::audio::AudioFormat {
    match fmt {
        SampleFormat::S16Le => spa::param::audio::AudioFormat::S16LE,
        SampleFormat::S32Le => spa::param::audio::AudioFormat::S32LE,
        SampleFormat::F32Le => spa::param::audio::AudioFormat::F32LE,
    }
}

/// State handed to the RT `process` callback as user data. Owning the ring here
/// keeps the callback alloc-free and lock-free (the only work is
/// [`ShmAudioBuffer::read_quantum`], which is wait-free).
struct PlaybackState {
    ring: ShmAudioBuffer,
    max_latency_bytes: u64,
    stride: usize,
}

/// Run the playback loop. Blocks until the process is terminated.
///
/// `ring` is moved in and lives for the duration of the loop; it must outlive the
/// mmap it points into (the caller keeps the mapping alive for the process).
pub fn run_playback(
    fmt: AudioFormat,
    ring: ShmAudioBuffer,
    max_latency_bytes: u64,
) -> Result<(), pw::Error> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let stride = fmt.frame_bytes as usize;
    // Low-latency quantum hint; the server may clamp to its global min/max.
    let latency = format!("256/{}", fmt.sample_rate);

    let stream = pw::stream::StreamBox::new(
        &core,
        "lalah-playback",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_LATENCY => latency.as_str(),
        },
    )?;

    let state = PlaybackState {
        ring,
        max_latency_bytes,
        stride,
    };

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .process(|stream, st| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let data = &mut datas[0];
            let stride = st.stride;

            let size = if let Some(slice) = data.data() {
                // Frame-align the writable window, then pull the newest audio.
                // read_quantum zero-fills anything beyond live data, so we always
                // emit the full window and the sink clock keeps ticking even when
                // the producer is briefly starved (silence, never stale bytes).
                let window = (slice.len() / stride) * stride;
                let _live = st.ring.read_quantum(&mut slice[..window], st.max_latency_bytes);
                window
            } else {
                0
            };

            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as _;
            *chunk.size_mut() = size as _;
        })
        .register()?;

    // Build the EnumFormat param from the runtime-negotiated format.
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa_format(fmt.format));
    audio_info.set_rate(fmt.sample_rate);
    audio_info.set_channels(fmt.channels);

    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    if fmt.channels >= 1 {
        position[0] = spa_sys::SPA_AUDIO_CHANNEL_FL;
    }
    if fmt.channels >= 2 {
        position[1] = spa_sys::SPA_AUDIO_CHANNEL_FR;
    }
    audio_info.set_position(position);

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: spa_sys::SPA_TYPE_OBJECT_Format,
            id: spa_sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .expect("serialize audio format pod")
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values).expect("audio format pod")];

    stream.connect(
        spa::utils::Direction::Output,
        None, // let the session manager route to the default sink
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    println!(
        "lalah-host: playing {} Hz, {} ch, {:?} (frame {} B); latency budget {} B. Ctrl-C to stop.",
        fmt.sample_rate, fmt.channels, fmt.format, fmt.frame_bytes, max_latency_bytes
    );

    mainloop.run();
    Ok(())
}
