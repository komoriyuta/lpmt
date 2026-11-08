//! Window capture through the ScreenCast portal and native PipeWire streams.
//! Each window has an independent, bounded newest-frame mailbox. All blocking
//! setup and uploads run off the application/render thread. Borrowed frames are
//! consumed before returning their buffers to PipeWire; no intermediate pixel copy.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use anyhow::{Context, Result};
use ashpd::desktop::{
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    PersistMode,
};
use futures_lite::StreamExt;
use pipewire as pw;
use pw::spa;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{Pod, Property, Value};

use crate::{CaptureConsumer, CaptureFrame, CapturePixelFormat};
use std::{cell::RefCell, rc::Rc};

type Consumer<T> = Rc<RefCell<dyn CaptureConsumer<Frame = T>>>;

struct WindowState<T> {
    frame: Option<T>,
    status: String,
    stopped: bool,
}

struct State<T> {
    ready: bool,
    error: Option<String>,
    windows: Vec<WindowState<T>>,
}

impl<T> Default for WindowState<T> {
    fn default() -> Self {
        Self {
            frame: None,
            status: String::new(),
            stopped: false,
        }
    }
}
impl<T> Default for State<T> {
    fn default() -> Self {
        Self {
            ready: false,
            error: None,
            windows: Vec::new(),
        }
    }
}

pub struct CaptureSession<T> {
    state: Arc<Mutex<State<T>>>,
    stop: Arc<AtomicBool>,
}

impl<T: Send + 'static> CaptureSession<T> {
    pub fn select_windows(
        multiple: bool,
        consume: impl CaptureConsumer<Frame = T>,
    ) -> Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = state.clone();
        let worker_stop = stop.clone();
        std::thread::Builder::new()
            .name("lpmt-pipewire".into())
            .spawn(move || {
                let result = async_io::block_on(run(
                    multiple,
                    &worker_state,
                    &worker_stop,
                    Rc::new(RefCell::new(consume)),
                ));
                let mut state = worker_state.lock().unwrap();
                if let Err(error) = result {
                    let message = format!("{error:#}");
                    for window in &mut state.windows {
                        window.frame = None;
                        window.status = message.clone();
                        window.stopped = true;
                    }
                    state.error = Some(message);
                }
                state.ready = true;
            })?;
        Ok(Self { state, stop })
    }

    /// None while the desktop's chooser is open; zero after cancellation/error.
    pub fn window_count(&self) -> Option<usize> {
        let state = self.state.lock().unwrap();
        state.ready.then_some(state.windows.len())
    }

    pub fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    pub fn take_frame(&self, index: usize) -> Option<T> {
        self.state
            .lock()
            .unwrap()
            .windows
            .get_mut(index)?
            .frame
            .take()
    }

    pub fn status(&self, index: usize) -> String {
        self.state
            .lock()
            .unwrap()
            .windows
            .get(index)
            .map_or_else(|| "Disconnected".into(), |window| window.status.clone())
    }

    pub fn is_stopped(&self, index: usize) -> bool {
        self.state
            .lock()
            .unwrap()
            .windows
            .get(index)
            .is_none_or(|window| window.stopped)
    }

    pub fn stop_window(&self, index: usize) {
        if let Some(window) = self.state.lock().unwrap().windows.get_mut(index) {
            window.stopped = true;
            window.frame = None;
            window.status = "Disconnected".into();
        }
    }
}

impl<T> Drop for CaptureSession<T> {
    fn drop(&mut self) {
        // Never join here: a portal dialog must not stall the UI on shutdown.
        self.stop.store(true, Ordering::Relaxed);
    }
}

async fn cancelled(stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        async_io::Timer::after(Duration::from_millis(20)).await;
    }
}

async fn run<T: Send + 'static>(
    multiple: bool,
    state: &Arc<Mutex<State<T>>>,
    stop: &AtomicBool,
    consume: Consumer<T>,
) -> Result<()> {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(pw::init);
    let proxy = Screencast::new()
        .await
        .context("ScreenCast portal unavailable")?;
    anyhow::ensure!(
        proxy
            .available_source_types()
            .await?
            .contains(SourceType::WINDOW),
        "This desktop portal does not support window capture"
    );
    let session = proxy.create_session(Default::default()).await?;
    // Subscribe before Start, so revocation is not missed during setup.
    let mut closed = session.receive_closed().await?;
    let work = async {
        let modes = proxy
            .available_cursor_modes()
            .await
            .unwrap_or(CursorMode::HIDDEN);
        let mut options = SelectSourcesOptions::default()
            .set_sources(SourceType::WINDOW)
            .set_multiple(multiple)
            .set_persist_mode(PersistMode::DoNot);
        if modes.contains(CursorMode::HIDDEN) {
            options = options.set_cursor_mode(CursorMode::HIDDEN);
        } else if modes.contains(CursorMode::EMBEDDED) {
            options = options.set_cursor_mode(CursorMode::EMBEDDED);
        }
        proxy.select_sources(&session, options).await?.response()?;
        let response = proxy
            .start(&session, None, Default::default())
            .await?
            .response()?;
        anyhow::ensure!(!response.streams().is_empty(), "No windows selected");
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let remote = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await?;
        let core = context.connect_fd_rc(remote, None)?;
        {
            let mut state = state.lock().unwrap();
            state.windows = response
                .streams()
                .iter()
                .map(|_| WindowState {
                    status: "Waiting for frames".into(),
                    ..Default::default()
                })
                .collect();
        }
        let error_state = state.clone();
        let _core_listener = core
            .add_listener_local()
            .error(move |id, _, _, message| {
                if id != pw::core::PW_ID_CORE {
                    log::warn!("PipeWire object {id}: {message}");
                    return;
                }
                let mut state = error_state.lock().unwrap();
                for window in &mut state.windows {
                    window.status = format!("PipeWire: {message}");
                    window.stopped = true;
                    window.frame = None;
                }
            })
            .register();
        let mut streams = Vec::new();
        for (index, stream) in response.streams().iter().enumerate() {
            streams.push(Some(CaptureStream::new(
                core.clone(),
                stream,
                state.clone(),
                index,
                consume.clone(),
            )?));
        }
        state.lock().unwrap().ready = true;
        let readiness = async_io::Async::new(mainloop.loop_().fd().try_clone_to_owned()?)?;
        loop {
            mainloop.loop_().iterate(pw::loop_::Timeout::None);
            for (index, stream) in streams.iter_mut().enumerate() {
                if state.lock().unwrap().windows[index].stopped && stream.take().is_some() {
                    consume.borrow_mut().release(index);
                }
            }
            if streams.iter().all(Option::is_none) {
                return Ok(());
            }
            // Wake on PipeWire activity, instead of adding up to 8 ms polling
            // latency. The slow timer only services UI disconnects on idle inputs.
            futures_lite::future::race(async { readiness.readable().await }, async {
                async_io::Timer::after(Duration::from_millis(100)).await;
                Ok(())
            })
            .await?;
        }
    };
    let result = futures_lite::future::race(work, async {
        futures_lite::future::race(cancelled(stop), async {
            closed.next().await;
        })
        .await;
        anyhow::bail!("Screen sharing ended");
    })
    .await;
    // Close even when selection is cancelled or pipeline setup fails.
    let _ = session.close().await;
    result
}

struct StreamData<T> {
    format: VideoInfoRaw,
    state: Arc<Mutex<State<T>>>,
    index: usize,
    consume: Consumer<T>,
    last_size: Option<(u32, u32)>,
}

impl<T> StreamData<T> {
    fn fail(&self, error: impl std::fmt::Display) {
        let mut state = self.state.lock().unwrap();
        let window = &mut state.windows[self.index];
        window.status = error.to_string();
        window.frame = None;
        window.stopped = true;
    }
}

struct CaptureStream<T> {
    // Drop the listener before the stream and its core.
    _listener: pw::stream::StreamListener<StreamData<T>>,
    _stream: pw::stream::StreamRc,
}

impl<T: Send + 'static> CaptureStream<T> {
    fn new(
        core: pw::core::CoreRc,
        source: &ashpd::desktop::screencast::Stream,
        state: Arc<Mutex<State<T>>>,
        index: usize,
        consume: Consumer<T>,
    ) -> Result<Self> {
        let mut properties = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        };
        if let Some(serial) = source.pipewire_serial() {
            properties.insert(*pw::keys::TARGET_OBJECT, serial.to_string());
        }
        let stream = pw::stream::StreamRc::new(core, "LPMT window", properties)?;
        let listener = stream
            .add_local_listener_with_user_data(StreamData {
                format: VideoInfoRaw::default(),
                state,
                index,
                consume,
                last_size: None,
            })
            .state_changed(|_, data, old, new| match new {
                pw::stream::StreamState::Error(error) => data.fail(format!("PipeWire: {error}")),
                pw::stream::StreamState::Unconnected
                    if !matches!(old, pw::stream::StreamState::Unconnected) =>
                {
                    data.fail("Window disconnected")
                }
                _ => {}
            })
            .param_changed(|stream, data, id, param| {
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    return;
                };
                let result = (|| -> Result<()> {
                    data.format
                        .parse(param)
                        .map_err(|error| anyhow::anyhow!("Invalid video format: {error:?}"))?;
                    anyhow::ensure!(
                        supported_format(data.format.format()),
                        "Unsupported window pixel format"
                    );
                    let buffers = encode(spa::pod::Object {
                        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
                        id: spa::param::ParamType::Buffers.as_raw(),
                        properties: vec![Property::new(
                            spa::sys::SPA_PARAM_BUFFERS_dataType,
                            Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                                spa::utils::ChoiceFlags::empty(),
                                spa::utils::ChoiceEnum::Flags {
                                    default: (1 << spa::sys::SPA_DATA_MemFd)
                                        | (1 << spa::sys::SPA_DATA_MemPtr),
                                    flags: vec![],
                                },
                            ))),
                        )],
                    })?;
                    let crop = meta_param(
                        spa::sys::SPA_META_VideoCrop,
                        std::mem::size_of::<spa::sys::spa_meta_region>(),
                    )?;
                    let header = meta_param(
                        spa::sys::SPA_META_Header,
                        std::mem::size_of::<spa::sys::spa_meta_header>(),
                    )?;
                    stream.update_params(&mut [pod(&buffers)?, pod(&crop)?, pod(&header)?])?;
                    Ok(())
                })();
                if let Err(error) = result {
                    data.fail(error);
                }
            })
            .process(|stream, data| {
                // Drain queued buffers and process only the newest one. Dropping a
                // buffer returns it to PipeWire, including on validation failures.
                let mut latest = stream.dequeue_buffer();
                while let Some(buffer) = stream.dequeue_buffer() {
                    latest = Some(buffer);
                }
                let Some(mut buffer) = latest else {
                    return;
                };
                // One unconsumed upload per window bounds GPU queue work when
                // rendering stalls/minimizes. Never block the PipeWire loop on it.
                {
                    let state = data.state.lock().unwrap();
                    let window = &state.windows[data.index];
                    if window.stopped || window.frame.is_some() {
                        return;
                    }
                }
                let result = (|| -> Result<_> {
                    let Some(frame) = decode_buffer(&mut buffer, &data.format)? else {
                        return Ok(None);
                    };
                    let size = (frame.width, frame.height);
                    let uploaded = data.consume.borrow_mut().upload(data.index, frame)?;
                    Ok(Some((uploaded, size)))
                })();
                match result {
                    Ok(Some((frame, size))) => {
                        let mut state = data.state.lock().unwrap();
                        let window = &mut state.windows[data.index];
                        if !window.stopped {
                            if data.last_size != Some(size) {
                                window.status = format!("Live · {}×{}", size.0, size.1);
                                data.last_size = Some(size);
                            }
                            window.frame = Some(frame);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => data.fail(error),
                }
            })
            .register()?;
        let format = spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::EnumFormat,
            spa::pod::property!(
                spa::param::format::FormatProperties::MediaType,
                Id,
                spa::param::format::MediaType::Video
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::MediaSubtype,
                Id,
                spa::param::format::MediaSubtype::Raw
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                VideoFormat::BGRA,
                VideoFormat::BGRA,
                VideoFormat::RGBA,
                VideoFormat::BGRx,
                VideoFormat::RGBx
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                spa::utils::Rectangle {
                    width: 1920,
                    height: 1080
                },
                spa::utils::Rectangle {
                    width: 1,
                    height: 1
                },
                spa::utils::Rectangle {
                    width: 16384,
                    height: 16384
                }
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                spa::utils::Fraction { num: 60, denom: 1 },
                spa::utils::Fraction { num: 0, denom: 1 },
                spa::utils::Fraction { num: 240, denom: 1 }
            ),
        );
        let bytes = encode(format)?;
        let target = source
            .pipewire_serial()
            .is_none()
            .then_some(source.pipe_wire_node_id());
        stream.connect(
            spa::utils::Direction::Input,
            target,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [pod(&bytes)?],
        )?;
        Ok(Self {
            _listener: listener,
            _stream: stream,
        })
    }
}

fn encode(object: spa::pod::Object) -> Result<Vec<u8>> {
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .map_err(|error| anyhow::anyhow!("PipeWire parameter: {error:?}"))?
    .0
    .into_inner())
}

fn pod(bytes: &[u8]) -> Result<&Pod> {
    Pod::from_bytes(bytes).context("Invalid PipeWire parameter")
}

fn meta_param(kind: u32, size: usize) -> Result<Vec<u8>> {
    encode(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(spa::utils::Id(kind)),
            ),
            Property::new(spa::sys::SPA_PARAM_META_size, Value::Int(size as i32)),
        ],
    })
}

fn supported_format(format: VideoFormat) -> bool {
    matches!(
        format,
        VideoFormat::RGBA | VideoFormat::RGBx | VideoFormat::BGRA | VideoFormat::BGRx
    )
}

fn decode_buffer<'a>(
    buffer: &'a mut pw::buffer::Buffer<'_>,
    format: &VideoInfoRaw,
) -> Result<Option<CaptureFrame<'a>>> {
    use spa::buffer::meta::{MetaHeader, MetaHeaderFlags, MetaVideoCrop};
    if buffer
        .find_meta::<MetaHeader>()
        .is_some_and(|header| header.flags().contains(MetaHeaderFlags::CORRUPTED))
    {
        return Ok(None);
    }
    let crop = buffer
        .find_meta::<MetaVideoCrop>()
        .map(|crop| crop.meta_region())
        .filter(|region| region.is_valid())
        .map(|region| *region.region());
    let Some(data) = buffer.datas_mut().first_mut() else {
        return Ok(None);
    };
    if data.chunk().size() == 0
        || data
            .chunk()
            .flags()
            .contains(spa::buffer::ChunkFlags::CORRUPTED)
    {
        return Ok(None);
    }
    let offset = data.chunk().offset() as usize;
    let size = data.chunk().size() as usize;
    let stride = data.chunk().stride();
    let bytes = data
        .data()
        .context("PipeWire did not supply mapped CPU memory")?;
    let end = offset.checked_add(size).context("Frame offset overflow")?;
    let bytes = bytes
        .get(offset..end)
        .context("Invalid PipeWire buffer bounds")?;
    borrow_frame(format.size(), format.format(), stride, crop, bytes).map(Some)
}

fn borrow_frame(
    size: spa::utils::Rectangle,
    format: VideoFormat,
    stride: i32,
    crop: Option<spa::utils::Region>,
    data: &[u8],
) -> Result<CaptureFrame<'_>> {
    anyhow::ensure!(supported_format(format), "Unsupported pixel format");
    let row = (size.width as usize)
        .checked_mul(4)
        .context("Frame row overflow")?;
    anyhow::ensure!(
        size.width > 0 && size.height > 0 && stride > 0 && stride as usize >= row,
        "Invalid frame dimensions/stride"
    );
    let stride = stride as usize;
    let crop = crop.unwrap_or(spa::utils::Region {
        position: spa::utils::Point { x: 0, y: 0 },
        size,
    });
    anyhow::ensure!(
        crop.position.x >= 0 && crop.position.y >= 0 && crop.size.width > 0 && crop.size.height > 0,
        "Invalid crop rectangle"
    );
    let x = crop.position.x as usize;
    let y = crop.position.y as usize;
    let width = crop.size.width as usize;
    let height = crop.size.height as usize;
    anyhow::ensure!(
        x + width <= size.width as usize && y + height <= size.height as usize,
        "Crop outside window buffer"
    );
    let required = (y + height - 1)
        .checked_mul(stride)
        .and_then(|n| n.checked_add((x + width) * 4))
        .context("Frame size overflow")?;
    anyhow::ensure!(data.len() >= required, "Truncated window frame");
    let format = match format {
        VideoFormat::RGBA => CapturePixelFormat::Rgba,
        VideoFormat::RGBx => CapturePixelFormat::Rgbx,
        VideoFormat::BGRA => CapturePixelFormat::Bgra,
        VideoFormat::BGRx => CapturePixelFormat::Bgrx,
        _ => unreachable!(),
    };
    // Preserve the producer's row pitch; wgpu copies only visible pixels.
    let start = y * stride + x * 4;
    Ok(CaptureFrame {
        width: width as u32,
        height: height as u32,
        stride: stride as u32,
        format,
        data: &data[start..required],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spa::utils::{Point, Rectangle, Region};

    #[test]
    fn padded_cropped_frames_borrow_original_memory() {
        let size = Rectangle {
            width: 2,
            height: 2,
        };
        let crop = Region {
            position: Point { x: 1, y: 0 },
            size: Rectangle {
                width: 1,
                height: 2,
            },
        };
        let bytes = [0, 0, 0, 0, 3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 5, 4, 0];
        let image = borrow_frame(size, VideoFormat::BGRx, 12, Some(crop), &bytes).unwrap();
        assert_eq!((image.width, image.height), (1, 2));
        assert_eq!(image.data.as_ptr(), bytes[4..].as_ptr());
        assert_eq!(image.stride, 12);
        assert_eq!(image.format, CapturePixelFormat::Bgrx);
        assert_eq!(&image.data[..4], &[3, 2, 1, 0]);
        assert_eq!(&image.data[12..], &[6, 5, 4, 0]);
        assert!(borrow_frame(size, VideoFormat::BGRx, 12, Some(crop), &bytes[..19]).is_err());
        assert!(borrow_frame(size, VideoFormat::BGRx, -12, None, &bytes).is_err());
        assert!(borrow_frame(size, VideoFormat::BGRx, 4, None, &bytes).is_err());
    }

    #[test]
    fn rgba_alpha_is_preserved_and_crop_bounds_are_checked() {
        let size = Rectangle {
            width: 1,
            height: 1,
        };
        assert_eq!(
            borrow_frame(size, VideoFormat::RGBA, 4, None, &[1, 2, 3, 4])
                .unwrap()
                .data,
            [1, 2, 3, 4]
        );
        let crop = Region {
            position: Point { x: 1, y: 0 },
            size,
        };
        assert!(borrow_frame(size, VideoFormat::RGBA, 4, Some(crop), &[1, 2, 3, 4]).is_err());
    }

    #[test]
    fn latest_frame_mailboxes_are_independent() {
        use crate::DecodedImage;
        let mut state = State {
            windows: vec![WindowState::default(), WindowState::default()],
            ..Default::default()
        };
        state.windows[0].frame = Some(DecodedImage::from_rgba8(1, 1, vec![1, 0, 0, 255]).unwrap());
        state.windows[1].frame = Some(DecodedImage::from_rgba8(1, 1, vec![0, 0, 2, 255]).unwrap());
        state.windows[0].frame = Some(DecodedImage::from_rgba8(1, 1, vec![3, 0, 0, 255]).unwrap());
        let session = CaptureSession {
            state: Arc::new(Mutex::new(state)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(session.take_frame(0).unwrap().rgba, [3, 0, 0, 255]);
        assert!(session.take_frame(0).is_none());
        session.stop_window(0);
        assert_eq!(session.take_frame(1).unwrap().rgba, [0, 0, 2, 255]);
        assert!(!session.is_stopped(1));
    }
}
