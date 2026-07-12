#[cfg(feature = "opus")]
use matroska_demuxer::TrackEntry;
use matroska_demuxer::{Frame, MatroskaFile, TrackType};
#[cfg(feature = "opus")]
use opus_decoder::OpusDecoder;
use re_rav1d::dav1d::{
    Decoder as Av1Decoder, Error as Av1DecodeError, Picture, PixelLayout, PlanarImageComponent,
};
#[cfg(feature = "opus")]
use rodio::buffer::SamplesBuffer;
#[cfg(feature = "opus")]
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player};
use std::fs::File;
use std::io::{Cursor, Read, Seek};
#[cfg(feature = "opus")]
use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
#[cfg(feature = "opus")]
use symphonia::core::audio::{AudioBuffer, Channels, Layout, Signal, SignalSpec};
#[cfg(feature = "opus")]
use symphonia::core::codecs::{CODEC_TYPE_OPUS, CodecParameters};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub use crate::yuv::{Format, Size, Yuv};

const DEFAULT_MEDIA_FILE_NAME: &str = "mitb.mkv";
const DEFAULT_FRAME_DURATION_NS: u64 = 33_333_333;
#[cfg(feature = "opus")]
const DEFAULT_AUDIO_SAMPLE_RATE: u32 = 48_000;

#[derive(Debug, Clone)]
pub enum MediaSource {
    File(PathBuf),
    Directory(PathBuf),
    Bytes(Arc<[u8]>),
}

impl MediaSource {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::Bytes(Arc::<[u8]>::from(bytes))
    }

    fn describe(&self) -> String {
        match self {
            Self::File(path) => format!("file:{}", path.display()),
            Self::Directory(path) => format!("dir:{}", path.display()),
            Self::Bytes(bytes) => format!("bytes:{}", bytes.len()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AvEvent {
    VideoFrame(Yuv),
    PlaybackEnded,
    PlaybackError(String),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlaybackOptions {
    pub start_offset_ns: u64,
}

#[derive(Debug, Clone)]
pub struct PlaybackController {
    tx: mpsc::UnboundedSender<PlaybackCommand>,
}

impl PlaybackController {
    pub fn seek(&self, timestamp_ns: u64) -> Result<(), PlaybackControlError> {
        self.tx
            .send(PlaybackCommand::SeekTo(timestamp_ns))
            .map_err(|_| PlaybackControlError::WorkerClosed)
    }

    pub fn set_start_offset(&self, timestamp_ns: u64) -> Result<(), PlaybackControlError> {
        self.tx
            .send(PlaybackCommand::SetStartOffset(timestamp_ns))
            .map_err(|_| PlaybackControlError::WorkerClosed)
    }

    pub fn pause(&self) -> Result<(), PlaybackControlError> {
        self.tx
            .send(PlaybackCommand::Pause)
            .map_err(|_| PlaybackControlError::WorkerClosed)
    }

    pub fn resume(&self) -> Result<(), PlaybackControlError> {
        self.tx
            .send(PlaybackCommand::Resume)
            .map_err(|_| PlaybackControlError::WorkerClosed)
    }
}

pub struct PlaybackSession {
    pub events: mpsc::UnboundedReceiver<AvEvent>,
    pub controller: PlaybackController,
}

#[derive(Debug, Error)]
pub enum PlaybackControlError {
    #[error("playback worker is closed")]
    WorkerClosed,
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
    #[cfg(feature = "opus")]
    #[error("failed to initialize Opus decoder: {0}")]
    AudioDecoder(String),
    #[cfg(feature = "opus")]
    #[error("audio output channel count is invalid: {0}")]
    AudioChannels(usize),
    #[cfg(feature = "opus")]
    #[error("audio output sample rate is invalid: {0}")]
    AudioSampleRate(u32),
    #[cfg(feature = "opus")]
    #[error("failed initializing audio output: {0}")]
    AudioOutput(String),
    #[error("seek requested to timestamp: {0}")]
    SeekRequested(u64),
}

#[derive(Debug)]
struct SelectedTracks {
    timestamp_scale_ns: u64,
    video_track: u64,
    video_default_duration_ns: Option<u64>,
    video_display_aspect_ratio: f32,
    #[cfg(feature = "opus")]
    audio: Option<AudioTrack>,
}

#[cfg(feature = "opus")]
#[derive(Debug)]
struct AudioTrack {
    track: u64,
    channels: usize,
    sample_rate: u32,
    pre_skip: usize,
}

#[cfg(feature = "opus")]
#[derive(Debug, Clone, Copy)]
struct OpusHead {
    channels: usize,
    pre_skip: usize,
    sample_rate: u32,
    mapping_family: u8,
}

#[cfg(feature = "opus")]
struct SymphoniaOpusDecoder {
    opus: OpusDecoder,
    codec_params: CodecParameters,
    buffer: AudioBuffer<f32>,
    scratch: Vec<f32>,
    channels: usize,
}

#[cfg(feature = "opus")]
struct AudioOutput {
    _sink: MixerDeviceSink,
    player: Player,
    channels: NonZeroU16,
    sample_rate: NonZeroU32,
}

#[derive(Debug)]
enum PlaybackCommand {
    SeekTo(u64),
    SetStartOffset(u64),
    Pause,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackAction {
    Continue,
    RestartAt(u64),
    Pause,
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

    fn set_base_timestamp(&mut self, base_timestamp_ns: u64) {
        self.start = Instant::now();
        self.base_timestamp_ns = Some(base_timestamp_ns);
        self.synthetic_timestamp_ns = base_timestamp_ns;
    }
}

#[cfg(feature = "opus")]
impl SymphoniaOpusDecoder {
    fn new(track: &AudioTrack) -> Result<Self, AvError> {
        let mut codec_params = CodecParameters::new();
        codec_params
            .for_codec(CODEC_TYPE_OPUS)
            .with_sample_rate(track.sample_rate)
            .with_channels(match track.channels {
                1 => Layout::Mono.into_channels(),
                _ => Layout::Stereo.into_channels(),
            })
            .with_time_base(symphonia::core::units::TimeBase::new(1, track.sample_rate));

        let opus = OpusDecoder::new(track.sample_rate, track.channels)
            .map_err(|error| AvError::AudioDecoder(error.to_string()))?;

        let spec = SignalSpec::new(
            track.sample_rate,
            match track.channels {
                1 => Channels::FRONT_LEFT,
                _ => Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
            },
        );
        let max_frame = opus.max_frame_size_per_channel() as u64;
        let buffer = AudioBuffer::<f32>::new(max_frame, spec);
        let scratch = vec![0.0_f32; opus.max_frame_size_per_channel() * track.channels];

        Ok(Self {
            opus,
            codec_params,
            buffer,
            scratch,
            channels: track.channels,
        })
    }

    fn decode_packet(&mut self, packet: &[u8]) -> Result<&AudioBuffer<f32>, AvError> {
        let max_samples = self.opus.max_frame_size_per_channel() * self.channels;
        if self.scratch.len() < max_samples {
            self.scratch.resize(max_samples, 0.0);
        }

        let samples_per_channel = self
            .opus
            .decode_float(packet, &mut self.scratch[..max_samples], false)
            .map_err(|error| AvError::AudioDecoder(error.to_string()))?;

        self.buffer.clear();
        self.buffer.render_reserved(Some(samples_per_channel));

        for channel in 0..self.channels {
            let channel_samples = self.buffer.chan_mut(channel);
            for (frame_index, sample) in channel_samples
                .iter_mut()
                .enumerate()
                .take(samples_per_channel)
            {
                let source_index = frame_index * self.channels + channel;
                *sample = self.scratch[source_index];
            }
        }

        Ok(&self.buffer)
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.codec_params
    }
}

#[cfg(feature = "opus")]
impl AudioOutput {
    fn new(channels: usize, sample_rate: u32) -> Result<Self, AvError> {
        let channels = NonZeroU16::new(channels as u16).ok_or(AvError::AudioChannels(channels))?;
        let sample_rate =
            NonZeroU32::new(sample_rate).ok_or(AvError::AudioSampleRate(sample_rate))?;

        let sink = DeviceSinkBuilder::open_default_sink()
            .map_err(|error| AvError::AudioOutput(error.to_string()))?;
        let player = Player::connect_new(sink.mixer());

        Ok(Self {
            _sink: sink,
            player,
            channels,
            sample_rate,
        })
    }

    fn append(&self, samples: Vec<f32>) {
        if samples.is_empty() {
            return;
        }

        let source = SamplesBuffer::new(self.channels, self.sample_rate, samples);
        self.player.append(source);
    }
}

pub fn spawn_default() -> Result<mpsc::UnboundedReceiver<AvEvent>, AvError> {
    Ok(spawn_with_source(default_media_source()?))
}

pub fn spawn(media_path: PathBuf) -> mpsc::UnboundedReceiver<AvEvent> {
    spawn_with_source(MediaSource::File(media_path))
}

pub fn spawn_with_source(media_source: MediaSource) -> mpsc::UnboundedReceiver<AvEvent> {
    spawn_session_with_source(media_source).events
}

pub fn spawn_session_default() -> Result<PlaybackSession, AvError> {
    Ok(spawn_session_with_source(default_media_source()?))
}

pub fn spawn_session(media_path: PathBuf) -> PlaybackSession {
    spawn_session_with_source(MediaSource::File(media_path))
}

pub fn spawn_session_with_source(media_source: MediaSource) -> PlaybackSession {
    spawn_session_with_source_and_options(media_source, PlaybackOptions::default())
}

pub fn spawn_session_with_source_and_options(
    media_source: MediaSource,
    options: PlaybackOptions,
) -> PlaybackSession {
    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    thread::Builder::new()
        .name(String::from("iced-av1-playback"))
        .spawn(move || run_worker(media_source, options, tx, cmd_rx))
        .map_err(|error| {
            warn!(%error, "failed to spawn AV worker thread");
        })
        .ok();

    PlaybackSession {
        events: rx,
        controller: PlaybackController { tx: cmd_tx },
    }
}

fn run_worker(
    media_source: MediaSource,
    options: PlaybackOptions,
    tx: mpsc::UnboundedSender<AvEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<PlaybackCommand>,
) {
    debug!(source = %media_source.describe(), "started AV worker");
    if let Some(media_path) = media_source_file_path(&media_source)
        && !media_path.exists()
    {
        debug!(
            source = %media_source.describe(),
            path = %media_path.display(),
            "AV media file not found; worker exiting"
        );
        let _ = tx.send(AvEvent::PlaybackEnded);
        return;
    }

    let mut loop_index: u64 = 0;
    let mut start_offset_ns = options.start_offset_ns;
    let mut seek_override_ns = Some(start_offset_ns);
    let mut paused = false;
    loop {
        loop_index = loop_index.saturating_add(1);

        match drain_commands(&mut cmd_rx, &mut start_offset_ns, &mut paused) {
            PlaybackAction::Continue => {}
            PlaybackAction::RestartAt(offset_ns) => {
                seek_override_ns = Some(offset_ns);
            }
            PlaybackAction::Pause => {}
        }

        if paused {
            if !wait_while_paused(
                &mut cmd_rx,
                &mut start_offset_ns,
                &mut seek_override_ns,
                &mut paused,
                &tx,
            ) {
                break;
            }

            continue;
        }

        debug!(
            source = %media_source.describe(),
            loop_index,
            "starting AV playback loop"
        );

        let loop_offset_ns = seek_override_ns.take().unwrap_or(start_offset_ns);
        match play_media_source(
            &media_source,
            loop_offset_ns,
            &tx,
            &mut cmd_rx,
            &mut start_offset_ns,
            &mut paused,
        ) {
            Ok(()) => {
                if tx.is_closed() {
                    break;
                }

                debug!(
                    source = %media_source.describe(),
                    loop_index,
                    "completed AV playback loop"
                );
            }
            Err(AvError::SeekRequested(offset_ns)) => {
                seek_override_ns = Some(offset_ns);
            }
            Err(error) => {
                warn!(%error, source = %media_source.describe(), loop_index, "AV playback failed");
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

fn play_media_source(
    media_source: &MediaSource,
    start_offset_ns: u64,
    tx: &mpsc::UnboundedSender<AvEvent>,
    cmd_rx: &mut mpsc::UnboundedReceiver<PlaybackCommand>,
    default_start_offset_ns: &mut u64,
    paused: &mut bool,
) -> Result<(), AvError> {
    match media_source {
        MediaSource::File(path) => play_media_file(
            path,
            start_offset_ns,
            tx,
            cmd_rx,
            default_start_offset_ns,
            paused,
        ),
        MediaSource::Directory(path) => play_media_file(
            path.join(DEFAULT_MEDIA_FILE_NAME),
            start_offset_ns,
            tx,
            cmd_rx,
            default_start_offset_ns,
            paused,
        ),
        MediaSource::Bytes(bytes) => {
            let reader = Cursor::new(bytes.as_ref());
            play_media(
                reader,
                "memory",
                start_offset_ns,
                tx,
                cmd_rx,
                default_start_offset_ns,
                paused,
            )
        }
    }
}

fn media_source_file_path(media_source: &MediaSource) -> Option<PathBuf> {
    match media_source {
        MediaSource::File(path) => Some(path.clone()),
        MediaSource::Directory(path) => Some(path.join(DEFAULT_MEDIA_FILE_NAME)),
        MediaSource::Bytes(_) => None,
    }
}

fn play_media_file(
    path: impl AsRef<Path>,
    start_offset_ns: u64,
    tx: &mpsc::UnboundedSender<AvEvent>,
    cmd_rx: &mut mpsc::UnboundedReceiver<PlaybackCommand>,
    default_start_offset_ns: &mut u64,
    paused: &mut bool,
) -> Result<(), AvError> {
    let path = path.as_ref();
    let file = File::open(path).map_err(|source| AvError::OpenMedia {
        path: path.to_path_buf(),
        source,
    })?;

    play_media(
        file,
        path.to_string_lossy().as_ref(),
        start_offset_ns,
        tx,
        cmd_rx,
        default_start_offset_ns,
        paused,
    )
}

fn play_media<R: Read + Seek>(
    reader: R,
    source_label: &str,
    start_offset_ns: u64,
    tx: &mpsc::UnboundedSender<AvEvent>,
    cmd_rx: &mut mpsc::UnboundedReceiver<PlaybackCommand>,
    default_start_offset_ns: &mut u64,
    paused: &mut bool,
) -> Result<(), AvError> {
    let mut matroska =
        MatroskaFile::open(reader).map_err(|error| AvError::Demux(error.to_string()))?;
    let tracks = select_tracks(&matroska)?;

    debug!(
        source = source_label,
        video_track = tracks.video_track,
        "starting AV decode"
    );

    let mut video_decoder =
        Av1Decoder::new().map_err(|error| AvError::VideoDecoder(error.to_string()))?;

    #[cfg(feature = "opus")]
    let mut audio_decoder = match tracks.audio.as_ref() {
        Some(audio_track) => Some(SymphoniaOpusDecoder::new(audio_track)?),
        None => None,
    };

    #[cfg(feature = "opus")]
    if let Some(decoder) = audio_decoder.as_ref() {
        let params = decoder.codec_params();
        debug!(
            codec = %params.codec,
            sample_rate = params.sample_rate.unwrap_or(DEFAULT_AUDIO_SAMPLE_RATE),
            "configured symphonia-backed Opus decode pipeline"
        );
    }

    #[cfg(feature = "opus")]
    let mut audio_output = match tracks.audio.as_ref() {
        Some(audio_track) => {
            match AudioOutput::new(audio_track.channels, audio_track.sample_rate) {
                Ok(output) => Some(output),
                Err(error) => {
                    warn!(%error, "audio output unavailable; continuing video-only playback");
                    None
                }
            }
        }
        None => None,
    };

    #[cfg(feature = "opus")]
    let mut remaining_pre_skip = tracks.audio.as_ref().map_or(0, |audio| audio.pre_skip);

    let mut frame = Frame::default();
    let mut playback_clock = PlaybackClock::new();
    let mut reached_start_offset = start_offset_ns == 0;
    let mut playback_position_ns = start_offset_ns;

    while matroska
        .next_frame(&mut frame)
        .map_err(|error| AvError::Demux(error.to_string()))?
    {
        if tx.is_closed() {
            return Ok(());
        }

        match drain_commands(cmd_rx, default_start_offset_ns, paused) {
            PlaybackAction::Continue => {}
            PlaybackAction::RestartAt(offset_ns) => return Err(AvError::SeekRequested(offset_ns)),
            PlaybackAction::Pause => return Err(AvError::SeekRequested(playback_position_ns)),
        }

        if frame.track == tracks.video_track {
            let timestamp_ns = frame.timestamp.saturating_mul(tracks.timestamp_scale_ns);
            playback_position_ns = timestamp_ns;
            let frame_duration_ns = frame
                .duration
                .map(|duration| duration.saturating_mul(tracks.timestamp_scale_ns))
                .or(tracks.video_default_duration_ns)
                .unwrap_or(DEFAULT_FRAME_DURATION_NS);

            submit_video_packet(
                &mut video_decoder,
                frame.data.clone(),
                tracks.video_display_aspect_ratio,
                timestamp_ns,
                frame_duration_ns,
                tx,
                &mut playback_clock,
                cmd_rx,
                default_start_offset_ns,
                paused,
                &mut playback_position_ns,
                start_offset_ns,
                &mut reached_start_offset,
            )?;
            continue;
        }

        #[cfg(feature = "opus")]
        if let Some(audio_track) = tracks.audio.as_ref()
            && frame.track == audio_track.track
            && let (Some(decoder), Some(output)) = (audio_decoder.as_mut(), audio_output.as_ref())
        {
            let timestamp_ns = frame.timestamp.saturating_mul(tracks.timestamp_scale_ns);
            match decoder.decode_packet(frame.data.as_slice()) {
                Ok(buffer) => {
                    let samples = interleave_audio_buffer(
                        buffer,
                        audio_track.channels,
                        &mut remaining_pre_skip,
                    );
                    if timestamp_ns >= start_offset_ns {
                        output.append(samples);
                    }
                }
                Err(error) => {
                    warn!(%error, "failed decoding Opus audio packet");
                }
            }
        }
    }

    drain_video_decoder(
        &mut video_decoder,
        tx,
        &mut playback_clock,
        tracks.video_display_aspect_ratio,
        tracks
            .video_default_duration_ns
            .unwrap_or(DEFAULT_FRAME_DURATION_NS),
        cmd_rx,
        default_start_offset_ns,
        paused,
        &mut playback_position_ns,
        start_offset_ns,
        &mut reached_start_offset,
    )?;

    #[cfg(feature = "opus")]
    if let Some(output) = audio_output.take() {
        output.player.sleep_until_end();
    }

    Ok(())
}

fn select_tracks<R: Read + Seek>(matroska: &MatroskaFile<R>) -> Result<SelectedTracks, AvError> {
    let timestamp_scale_ns = matroska.info().timestamp_scale().get();

    let mut video_track = None;
    let mut video_default_duration_ns = None;
    let mut video_display_aspect_ratio = None;
    #[cfg(feature = "opus")]
    let mut audio_track = None;

    for track in matroska.tracks() {
        if track.track_type() == TrackType::Video
            && track.codec_id() == "V_AV1"
            && video_track.is_none()
        {
            video_track = Some(track.track_number().get());
            video_default_duration_ns = track.default_duration().map(|duration| duration.get());
            video_display_aspect_ratio = track.video().map(|video| {
                let display_width = video
                    .display_width()
                    .map(|value| value.get())
                    .unwrap_or(video.pixel_width().get());
                let display_height = video
                    .display_height()
                    .map(|value| value.get())
                    .unwrap_or(video.pixel_height().get());
                display_width as f32 / display_height as f32
            });
        }

        #[cfg(feature = "opus")]
        if track.track_type() == TrackType::Audio
            && track.codec_id() == "A_OPUS"
            && audio_track.is_none()
        {
            audio_track = parse_audio_track(track);
        }
    }

    let video_track = video_track.ok_or(AvError::MissingVideoTrack)?;
    let video_display_aspect_ratio = video_display_aspect_ratio.unwrap_or(1.0);

    Ok(SelectedTracks {
        timestamp_scale_ns,
        video_track,
        video_default_duration_ns,
        video_display_aspect_ratio,
        #[cfg(feature = "opus")]
        audio: audio_track,
    })
}

#[cfg(feature = "opus")]
fn parse_audio_track(track: &TrackEntry) -> Option<AudioTrack> {
    let audio = track.audio()?;

    let mut channels = audio.channels().get() as usize;
    let mut pre_skip = 0_usize;
    let mut sample_rate = audio
        .output_sampling_frequency()
        .or_else(|| Some(audio.sampling_frequency()))
        .unwrap_or(DEFAULT_AUDIO_SAMPLE_RATE as f64) as u32;

    if let Some(opus_head) = parse_opus_head(track.codec_private()) {
        channels = opus_head.channels.max(1);
        pre_skip = opus_head.pre_skip;
        if opus_head.sample_rate > 0 {
            sample_rate = opus_head.sample_rate;
        }

        if opus_head.mapping_family != 0 && channels > 2 {
            warn!(
                mapping_family = opus_head.mapping_family,
                channels, "unsupported Opus mapping family for this build; skipping audio playback"
            );
            return None;
        }
    }

    if channels == 0 || channels > 2 {
        warn!(
            channels,
            "unsupported Opus channel count for this build; skipping audio playback"
        );
        return None;
    }

    Some(AudioTrack {
        track: track.track_number().get(),
        channels,
        sample_rate: sample_rate.max(DEFAULT_AUDIO_SAMPLE_RATE),
        pre_skip,
    })
}

#[cfg(feature = "opus")]
fn parse_opus_head(codec_private: Option<&[u8]>) -> Option<OpusHead> {
    let data = codec_private?;
    if data.len() < 19 || &data[..8] != b"OpusHead" {
        return None;
    }

    let channels = data.get(9).copied().unwrap_or(1) as usize;
    let pre_skip = u16::from_le_bytes([data[10], data[11]]) as usize;
    let sample_rate = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    let mapping_family = data[18];

    Some(OpusHead {
        channels,
        pre_skip,
        sample_rate,
        mapping_family,
    })
}

#[cfg(feature = "opus")]
fn interleave_audio_buffer(
    buffer: &AudioBuffer<f32>,
    channels: usize,
    remaining_pre_skip: &mut usize,
) -> Vec<f32> {
    let frames = buffer.frames();
    if frames == 0 {
        return Vec::new();
    }

    let skip_frames = (*remaining_pre_skip).min(frames);
    *remaining_pre_skip = remaining_pre_skip.saturating_sub(skip_frames);

    if skip_frames >= frames {
        return Vec::new();
    }

    let output_frames = frames - skip_frames;
    let mut output = vec![0.0_f32; output_frames * channels];

    for frame_index in skip_frames..frames {
        let output_index = frame_index - skip_frames;
        for channel in 0..channels {
            if let Some(sample) = buffer.chan(channel).get(frame_index) {
                output[output_index * channels + channel] = *sample;
            }
        }
    }

    output
}

fn submit_video_packet(
    decoder: &mut Av1Decoder,
    data: Vec<u8>,
    display_aspect_ratio: f32,
    timestamp_ns: u64,
    frame_duration_ns: u64,
    tx: &mpsc::UnboundedSender<AvEvent>,
    playback_clock: &mut PlaybackClock,
    cmd_rx: &mut mpsc::UnboundedReceiver<PlaybackCommand>,
    default_start_offset_ns: &mut u64,
    paused: &mut bool,
    playback_position_ns: &mut u64,
    start_offset_ns: u64,
    reached_start_offset: &mut bool,
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

    drain_video_decoder(
        decoder,
        tx,
        playback_clock,
        display_aspect_ratio,
        frame_duration_ns,
        cmd_rx,
        default_start_offset_ns,
        paused,
        playback_position_ns,
        start_offset_ns,
        reached_start_offset,
    )?;

    loop {
        match decoder.send_pending_data() {
            Ok(()) => break,
            Err(Av1DecodeError::Again) => {
                drain_video_decoder(
                    decoder,
                    tx,
                    playback_clock,
                    display_aspect_ratio,
                    frame_duration_ns,
                    cmd_rx,
                    default_start_offset_ns,
                    paused,
                    playback_position_ns,
                    start_offset_ns,
                    reached_start_offset,
                )?;
            }
            Err(error) => {
                return Err(AvError::VideoDecoder(error.to_string()));
            }
        }
    }

    drain_video_decoder(
        decoder,
        tx,
        playback_clock,
        display_aspect_ratio,
        frame_duration_ns,
        cmd_rx,
        default_start_offset_ns,
        paused,
        playback_position_ns,
        start_offset_ns,
        reached_start_offset,
    )?;

    Ok(())
}

fn drain_video_decoder(
    decoder: &mut Av1Decoder,
    tx: &mpsc::UnboundedSender<AvEvent>,
    playback_clock: &mut PlaybackClock,
    display_aspect_ratio: f32,
    default_frame_duration_ns: u64,
    cmd_rx: &mut mpsc::UnboundedReceiver<PlaybackCommand>,
    default_start_offset_ns: &mut u64,
    paused: &mut bool,
    playback_position_ns: &mut u64,
    start_offset_ns: u64,
    reached_start_offset: &mut bool,
) -> Result<(), AvError> {
    loop {
        match drain_commands(cmd_rx, default_start_offset_ns, paused) {
            PlaybackAction::Continue => {}
            PlaybackAction::RestartAt(offset_ns) => return Err(AvError::SeekRequested(offset_ns)),
            PlaybackAction::Pause => return Err(AvError::SeekRequested(*playback_position_ns)),
        }

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

        let next_playback_position_ns = if let Some(timestamp_ns) = timestamp_ns {
            timestamp_ns
        } else {
            playback_position_ns.saturating_add(frame_duration_ns.max(1))
        };

        if !*reached_start_offset {
            let should_start = timestamp_ns
                .map(|timestamp_ns| timestamp_ns >= start_offset_ns)
                .unwrap_or(*playback_position_ns >= start_offset_ns);

            if !should_start {
                *playback_position_ns = next_playback_position_ns;
                continue;
            }

            let base_timestamp_ns = timestamp_ns.unwrap_or(*playback_position_ns);
            playback_clock.set_base_timestamp(base_timestamp_ns);
            *reached_start_offset = true;
        }

        playback_clock.wait_until(timestamp_ns, frame_duration_ns);
        *playback_position_ns = next_playback_position_ns;

        if let Some(yuv) = picture_to_yuv(&picture, display_aspect_ratio)
            && tx.send(AvEvent::VideoFrame(yuv)).is_err()
        {
            return Ok(());
        }
    }

    Ok(())
}

fn picture_to_yuv(picture: &Picture, display_aspect_ratio: f32) -> Option<Yuv> {
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
                display_aspect_ratio,
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
                display_aspect_ratio,
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

fn drain_commands(
    cmd_rx: &mut mpsc::UnboundedReceiver<PlaybackCommand>,
    default_start_offset_ns: &mut u64,
    paused: &mut bool,
) -> PlaybackAction {
    let mut restart_at = None;

    while let Ok(command) = cmd_rx.try_recv() {
        match command {
            PlaybackCommand::SeekTo(offset_ns) => {
                restart_at = Some(offset_ns);
            }
            PlaybackCommand::SetStartOffset(offset_ns) => {
                *default_start_offset_ns = offset_ns;
            }
            PlaybackCommand::Pause => {
                *paused = true;
            }
            PlaybackCommand::Resume => {
                *paused = false;
            }
        }
    }

    if let Some(offset_ns) = restart_at {
        PlaybackAction::RestartAt(offset_ns)
    } else if *paused {
        PlaybackAction::Pause
    } else {
        PlaybackAction::Continue
    }
}

fn wait_while_paused(
    cmd_rx: &mut mpsc::UnboundedReceiver<PlaybackCommand>,
    default_start_offset_ns: &mut u64,
    seek_override_ns: &mut Option<u64>,
    paused: &mut bool,
    tx: &mpsc::UnboundedSender<AvEvent>,
) -> bool {
    while *paused {
        if tx.is_closed() {
            return false;
        }

        match drain_commands(cmd_rx, default_start_offset_ns, paused) {
            PlaybackAction::Continue | PlaybackAction::Pause => {}
            PlaybackAction::RestartAt(offset_ns) => {
                *seek_override_ns = Some(offset_ns);
            }
        }

        if *paused {
            thread::sleep(Duration::from_millis(10));
        }
    }

    true
}

pub fn default_media_path() -> Result<PathBuf, AvError> {
    let home = std::env::var("HOME").map_err(AvError::MissingHome)?;
    let mut path = PathBuf::from(home);
    path.push(".mitb");
    path.push(DEFAULT_MEDIA_FILE_NAME);
    Ok(path)
}

pub fn default_media_source() -> Result<MediaSource, AvError> {
    Ok(MediaSource::File(default_media_path()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "opus")]
    #[test]
    fn parses_opus_head_metadata() {
        let mut data = [0_u8; 19];
        data[..8].copy_from_slice(b"OpusHead");
        data[8] = 1;
        data[9] = 2;
        data[10..12].copy_from_slice(&312_u16.to_le_bytes());
        data[12..16].copy_from_slice(&48_000_u32.to_le_bytes());
        data[18] = 0;

        let head = parse_opus_head(Some(&data)).expect("valid OpusHead");

        assert_eq!(head.channels, 2);
        assert_eq!(head.pre_skip, 312);
        assert_eq!(head.sample_rate, 48_000);
        assert_eq!(head.mapping_family, 0);
    }

    #[cfg(feature = "opus")]
    #[test]
    fn interleaves_audio_after_applying_pre_skip() {
        let spec = SignalSpec::new(48_000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        let mut buffer = AudioBuffer::<f32>::new(3, spec);
        buffer.render_reserved(Some(3));
        buffer.chan_mut(0).copy_from_slice(&[1.0, 2.0, 3.0]);
        buffer.chan_mut(1).copy_from_slice(&[4.0, 5.0, 6.0]);
        let mut remaining_pre_skip = 1;

        let samples = interleave_audio_buffer(&buffer, 2, &mut remaining_pre_skip);

        assert_eq!(samples, vec![2.0, 5.0, 3.0, 6.0]);
        assert_eq!(remaining_pre_skip, 0);
    }

    #[test]
    fn drain_commands_pauses_and_resumes() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut start_offset_ns = 0;
        let mut paused = false;

        tx.send(PlaybackCommand::Pause).expect("send pause command");
        let action = drain_commands(&mut rx, &mut start_offset_ns, &mut paused);
        assert_eq!(action, PlaybackAction::Pause);
        assert!(paused);

        tx.send(PlaybackCommand::Resume)
            .expect("send resume command");
        let action = drain_commands(&mut rx, &mut start_offset_ns, &mut paused);
        assert_eq!(action, PlaybackAction::Continue);
        assert!(!paused);
    }

    #[test]
    fn drain_commands_keeps_seek_while_paused() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut start_offset_ns = 0;
        let mut paused = false;

        tx.send(PlaybackCommand::Pause).expect("send pause command");
        tx.send(PlaybackCommand::SeekTo(42))
            .expect("send seek command");
        let action = drain_commands(&mut rx, &mut start_offset_ns, &mut paused);

        assert_eq!(action, PlaybackAction::RestartAt(42));
        assert!(paused);
    }
}
