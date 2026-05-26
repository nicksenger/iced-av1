use matroska_demuxer::{Frame, MatroskaFile, TrackType};
use re_rav1d::dav1d::{
    Decoder as Av1Decoder, Error as Av1DecodeError, Picture, PixelLayout, PlanarImageComponent,
};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub use crate::yuv::{Format, Size, Yuv};

const DEFAULT_MEDIA_FILE_NAME: &str = "mitb.mkv";
const DEFAULT_FRAME_DURATION_NS: u64 = 33_333_333;

#[derive(Debug, Clone)]
pub enum AvEvent {
    VideoFrame(Yuv),
    PlaybackEnded,
    PlaybackError(String),
}

#[derive(Debug, Error)]
pub enum AvError {
    #[error("missing HOME for AV media path: {0}")]
    MissingHome(std::env::VarError),
    #[error("failed opening media file `{path}`: {source}")]
    OpenMedia {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("matroska demux error: {0}")]
    Demux(String),
    #[error("missing AV1 video track in media file")]
    MissingVideoTrack,
    #[error("failed to initialize AV1 decoder: {0}")]
    VideoDecoder(String),
}

#[derive(Debug)]
struct SelectedTracks {
    timestamp_scale_ns: u64,
    video_track: u64,
    video_default_duration_ns: Option<u64>,
}

struct PlaybackClock {
    start: Instant,
    base_timestamp_ns: Option<u64>,
    synthetic_timestamp_ns: u64,
}

impl PlaybackClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            base_timestamp_ns: None,
            synthetic_timestamp_ns: 0,
        }
    }

    fn wait_until(&mut self, timestamp_ns: Option<u64>, frame_duration_ns: u64) {
        let target_ns = match timestamp_ns {
            Some(ts) => {
                self.synthetic_timestamp_ns = ts;
                ts
            }
            None => {
                self.synthetic_timestamp_ns = self
                    .synthetic_timestamp_ns
                    .saturating_add(frame_duration_ns.max(1));
                self.synthetic_timestamp_ns
            }
        };

        let Some(base) = self.base_timestamp_ns else {
            self.base_timestamp_ns = Some(target_ns);
            return;
        };

        let relative_ns = target_ns.saturating_sub(base);
        let target = self.start + Duration::from_nanos(relative_ns);
        if let Some(remaining) = target.checked_duration_since(Instant::now()) {
            thread::sleep(remaining);
        }
    }
}

pub fn spawn_default() -> Result<mpsc::UnboundedReceiver<AvEvent>, AvError> {
    let media_path = default_media_path()?;
    Ok(spawn(media_path))
}

pub fn spawn(media_path: PathBuf) -> mpsc::UnboundedReceiver<AvEvent> {
    let (tx, rx) = mpsc::unbounded_channel();

    thread::Builder::new()
        .name(String::from("iced-av1-playback"))
        .spawn(move || run_worker(media_path, tx))
        .map_err(|error| {
            warn!(%error, "failed to spawn AV worker thread");
        })
        .ok();

    rx
}

fn run_worker(media_path: PathBuf, tx: mpsc::UnboundedSender<AvEvent>) {
    debug!(path = %media_path.display(), "started AV worker");
    if !media_path.exists() {
        debug!(path = %media_path.display(), "AV media file not found; worker exiting");
        let _ = tx.send(AvEvent::PlaybackEnded);
        return;
    }

    let mut loop_index: u64 = 0;
    loop {
        loop_index = loop_index.saturating_add(1);
        debug!(
            path = %media_path.display(),
            loop_index,
            "starting AV playback loop"
        );

        match play_media_file(media_path.as_path(), &tx) {
            Ok(()) => {
                if tx.is_closed() {
                    break;
                }

                debug!(
                    path = %media_path.display(),
                    loop_index,
                    "completed AV playback loop"
                );
            }
            Err(error) => {
                warn!(%error, loop_index, "AV playback failed");
                let _ = tx.send(AvEvent::PlaybackError(error.to_string()));
                thread::sleep(Duration::from_secs(1));
            }
        }

        if tx.is_closed() {
            break;
        }
    }

    let _ = tx.send(AvEvent::PlaybackEnded);
}

fn play_media_file(path: &Path, tx: &mpsc::UnboundedSender<AvEvent>) -> Result<(), AvError> {
    let file = File::open(path).map_err(|source| AvError::OpenMedia {
        path: path.to_path_buf(),
        source,
    })?;

    let mut matroska =
        MatroskaFile::open(file).map_err(|error| AvError::Demux(error.to_string()))?;
    let tracks = select_tracks(&matroska)?;

    debug!(
        path = %path.display(),
        video_track = tracks.video_track,
        "starting AV decode"
    );

    let mut video_decoder =
        Av1Decoder::new().map_err(|error| AvError::VideoDecoder(error.to_string()))?;

    let mut frame = Frame::default();
    let mut playback_clock = PlaybackClock::new();

    while matroska
        .next_frame(&mut frame)
        .map_err(|error| AvError::Demux(error.to_string()))?
    {
        if tx.is_closed() {
            return Ok(());
        }

        if frame.track == tracks.video_track {
            let timestamp_ns = frame.timestamp.saturating_mul(tracks.timestamp_scale_ns);
            let frame_duration_ns = frame
                .duration
                .map(|duration| duration.saturating_mul(tracks.timestamp_scale_ns))
                .or(tracks.video_default_duration_ns)
                .unwrap_or(DEFAULT_FRAME_DURATION_NS);

            submit_video_packet(
                &mut video_decoder,
                frame.data.clone(),
                timestamp_ns,
                frame_duration_ns,
                tx,
                &mut playback_clock,
            )?;
        }
    }

    drain_video_decoder(
        &mut video_decoder,
        tx,
        &mut playback_clock,
        tracks
            .video_default_duration_ns
            .unwrap_or(DEFAULT_FRAME_DURATION_NS),
    )?;

    Ok(())
}

fn select_tracks(matroska: &MatroskaFile<File>) -> Result<SelectedTracks, AvError> {
    let timestamp_scale_ns = matroska.info().timestamp_scale().get();

    let mut video_track = None;
    let mut video_default_duration_ns = None;

    for track in matroska.tracks() {
        if track.track_type() == TrackType::Video
            && track.codec_id() == "V_AV1"
            && video_track.is_none()
        {
            video_track = Some(track.track_number().get());
            video_default_duration_ns = track.default_duration().map(|duration| duration.get());
        }
    }

    let video_track = video_track.ok_or(AvError::MissingVideoTrack)?;

    Ok(SelectedTracks {
        timestamp_scale_ns,
        video_track,
        video_default_duration_ns,
    })
}

fn submit_video_packet(
    decoder: &mut Av1Decoder,
    data: Vec<u8>,
    timestamp_ns: u64,
    frame_duration_ns: u64,
    tx: &mpsc::UnboundedSender<AvEvent>,
    playback_clock: &mut PlaybackClock,
) -> Result<(), AvError> {
    match decoder.send_data(
        data,
        None,
        Some(timestamp_ns_to_i64(timestamp_ns)),
        Some(timestamp_ns_to_i64(frame_duration_ns)),
    ) {
        Ok(()) | Err(Av1DecodeError::Again) => {}
        Err(error) => {
            return Err(AvError::VideoDecoder(error.to_string()));
        }
    }

    drain_video_decoder(decoder, tx, playback_clock, frame_duration_ns)?;

    loop {
        match decoder.send_pending_data() {
            Ok(()) => break,
            Err(Av1DecodeError::Again) => {
                drain_video_decoder(decoder, tx, playback_clock, frame_duration_ns)?;
            }
            Err(error) => {
                return Err(AvError::VideoDecoder(error.to_string()));
            }
        }
    }

    drain_video_decoder(decoder, tx, playback_clock, frame_duration_ns)?;

    Ok(())
}

fn drain_video_decoder(
    decoder: &mut Av1Decoder,
    tx: &mpsc::UnboundedSender<AvEvent>,
    playback_clock: &mut PlaybackClock,
    default_frame_duration_ns: u64,
) -> Result<(), AvError> {
    loop {
        let picture = match decoder.get_picture() {
            Ok(picture) => picture,
            Err(Av1DecodeError::Again) => break,
            Err(error) => return Err(AvError::VideoDecoder(error.to_string())),
        };

        let frame_duration_ns = if picture.duration() > 0 {
            picture.duration() as u64
        } else {
            default_frame_duration_ns
        };

        let timestamp_ns = picture
            .timestamp()
            .and_then(|timestamp| u64::try_from(timestamp).ok());
        playback_clock.wait_until(timestamp_ns, frame_duration_ns);

        if let Some(yuv) = picture_to_yuv(&picture)
            && tx.send(AvEvent::VideoFrame(yuv)).is_err()
        {
            return Ok(());
        }
    }

    Ok(())
}

fn picture_to_yuv(picture: &Picture) -> Option<Yuv> {
    if picture.bit_depth() != 8 {
        warn!(
            bit_depth = picture.bit_depth(),
            "unsupported AV1 bit depth for overlay"
        );
        return None;
    }

    let width = picture.width();
    let height = picture.height();
    let dimensions = (width, height).into();

    match picture.pixel_layout() {
        PixelLayout::I420 => {
            let y = copy_plane(
                picture.plane(PlanarImageComponent::Y).as_ref(),
                picture.stride(PlanarImageComponent::Y),
                width,
                height,
            )?;
            let chroma_width = width.div_ceil(2);
            let chroma_height = height.div_ceil(2);
            let u = copy_plane(
                picture.plane(PlanarImageComponent::U).as_ref(),
                picture.stride(PlanarImageComponent::U),
                chroma_width,
                chroma_height,
            )?;
            let v = copy_plane(
                picture.plane(PlanarImageComponent::V).as_ref(),
                picture.stride(PlanarImageComponent::V),
                chroma_width,
                chroma_height,
            )?;

            let mut data = Vec::with_capacity(y.len() + u.len() + v.len());
            data.extend_from_slice(y.as_slice());
            data.extend_from_slice(u.as_slice());
            data.extend_from_slice(v.as_slice());

            Some(Yuv {
                format: Format::I420,
                data,
                dimensions,
            })
        }
        PixelLayout::I444 => {
            let y = copy_plane(
                picture.plane(PlanarImageComponent::Y).as_ref(),
                picture.stride(PlanarImageComponent::Y),
                width,
                height,
            )?;
            let u = copy_plane(
                picture.plane(PlanarImageComponent::U).as_ref(),
                picture.stride(PlanarImageComponent::U),
                width,
                height,
            )?;
            let v = copy_plane(
                picture.plane(PlanarImageComponent::V).as_ref(),
                picture.stride(PlanarImageComponent::V),
                width,
                height,
            )?;

            let mut data = Vec::with_capacity(y.len() + u.len() + v.len());
            data.extend_from_slice(y.as_slice());
            data.extend_from_slice(u.as_slice());
            data.extend_from_slice(v.as_slice());

            Some(Yuv {
                format: Format::Y444,
                data,
                dimensions,
            })
        }
        layout => {
            warn!(?layout, "unsupported AV1 pixel layout for overlay");
            None
        }
    }
}

fn copy_plane(plane: &[u8], stride: u32, width: u32, height: u32) -> Option<Vec<u8>> {
    let stride = usize::try_from(stride).ok()?;
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;

    if stride < width {
        return None;
    }

    let required = stride.checked_mul(height)?;
    if plane.len() < required {
        return None;
    }

    let mut output = Vec::with_capacity(width.checked_mul(height)?);
    for row in 0..height {
        let start = row.checked_mul(stride)?;
        let end = start.checked_add(width)?;
        output.extend_from_slice(plane.get(start..end)?);
    }

    Some(output)
}

fn timestamp_ns_to_i64(value: u64) -> i64 {
    if value > i64::MAX as u64 {
        i64::MAX
    } else {
        value as i64
    }
}

pub fn default_media_path() -> Result<PathBuf, AvError> {
    let home = std::env::var("HOME").map_err(AvError::MissingHome)?;
    let mut path = PathBuf::from(home);
    path.push(".mitb");
    path.push(DEFAULT_MEDIA_FILE_NAME);
    Ok(path)
}
