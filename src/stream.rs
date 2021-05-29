use std::io::{Read, Seek};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::{error, fmt};

use crate::decoder;
use crate::dynamic_mixer::{self, DynamicMixer, DynamicMixerController};
use crate::sink::Sink;
use crate::source::Source;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;

struct StreamWrapper(cpal::Stream);

unsafe impl Send for StreamWrapper {}

/// `cpal::Stream` container. Also see the more useful `OutputStreamHandle`.
///
/// If this is dropped playback will end & attached `OutputStreamHandle`s will no longer work.
pub struct OutputStream {
    mixer: Arc<DynamicMixerController<f32>>,
    mixer_rx: Arc<Mutex<DynamicMixer<f32>>>,
    stream: Arc<Mutex<Option<StreamWrapper>>>,
    recovery_thread: JoinHandle<()>,
}

/// More flexible handle to a `OutputStream` that provides playback.
#[derive(Clone)]
pub struct OutputStreamHandle {
    mixer: Weak<DynamicMixerController<f32>>,
}

fn find_device_by_name(name: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    let device = host.output_devices().and_then(|devices| Ok(devices.filter(|d| d.name().unwrap() == name))).unwrap().next();

    device
}

impl OutputStream {
    /// Returns a new stream & handle using the given output device.
    pub fn try_from_device(
        device: &cpal::Device,
    ) -> Result<(Self, OutputStreamHandle), StreamError> {
        let device_name = device.name().unwrap();

        // Determine the format to use for the new stream.
        let format = device.default_output_config()?;

        let (mixer, mixer_rx) =
            dynamic_mixer::mixer::<f32>(format.channels(), format.sample_rate().0);

        let (device_broken, stream) = device.new_output_stream_with_format(format.clone(), mixer_rx.clone())?;
        stream.play()?;

        let stream = Arc::new(Mutex::new(Some(StreamWrapper(stream))));

        let recovery_thread = {
            let mixer_rx = mixer_rx.clone();
            let stream = stream.clone();

            // Spawn recovery thread
            std::thread::spawn(move || {
                let mut device_broken = device_broken;

                let mg = Mutex::new(());
                loop {
                    let _ = device_broken.wait(mg.lock().unwrap());

                    dbg!("BROKEN!");

                    loop {
                        if let Some(new_device) = find_device_by_name(&device_name) {
                            if let Ok((new_device_broken, new_stream)) = new_device.new_output_stream_with_format(format.clone(), mixer_rx.clone()) {
                                if new_stream.play().is_ok() {
                                    dbg!("FIXED!");
                                    device_broken = new_device_broken;
                                    stream.lock().unwrap().replace(StreamWrapper(new_stream));

                                    break;
                                }
                            }
                        }
                    }
                }
            })
        };

        let out = Self { mixer, mixer_rx, stream, recovery_thread };
        let handle = OutputStreamHandle {
            mixer: Arc::downgrade(&out.mixer),
        };
        Ok((out, handle))
    }

    /// Return a new stream & handle using the default output device.
    ///
    /// On failure will fallback to trying any non-default output devices.
    pub fn try_default() -> Result<(Self, OutputStreamHandle), StreamError> {
        let default_device = cpal::default_host()
            .default_output_device()
            .ok_or(StreamError::NoDevice)?;

        let default_stream = Self::try_from_device(&default_device);

        default_stream.or_else(|original_err| {
            // default device didn't work, try other ones
            let mut devices = match cpal::default_host().output_devices() {
                Ok(d) => d,
                Err(_) => return Err(original_err),
            };

            devices
                .find_map(|d| Self::try_from_device(&d).ok())
                .ok_or(original_err)
        })
    }
}

impl OutputStreamHandle {
    /// Plays a source with a device until it ends.
    pub fn play_raw<S>(&self, source: S) -> Result<(), PlayError>
    where
        S: Source<Item = f32> + Send + 'static,
    {
        let mixer = self.mixer.upgrade().ok_or(PlayError::NoDevice)?;
        mixer.add(source);
        Ok(())
    }

    /// Plays a sound once. Returns a `Sink` that can be used to control the sound.
    pub fn play_once<R>(&self, input: R) -> Result<Sink, PlayError>
    where
        R: Read + Seek + Send + 'static,
    {
        let input = decoder::Decoder::new(input)?;
        let sink = Sink::try_new(self)?;
        sink.append(input);
        Ok(sink)
    }
}

/// An error occurred while attemping to play a sound.
#[derive(Debug)]
pub enum PlayError {
    /// Attempting to decode the audio failed.
    DecoderError(decoder::DecoderError),
    /// The output device was lost.
    NoDevice,
}

impl From<decoder::DecoderError> for PlayError {
    fn from(err: decoder::DecoderError) -> Self {
        Self::DecoderError(err)
    }
}

impl fmt::Display for PlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DecoderError(e) => e.fmt(f),
            Self::NoDevice => write!(f, "NoDevice"),
        }
    }
}

impl error::Error for PlayError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::DecoderError(e) => Some(e),
            Self::NoDevice => None,
        }
    }
}

#[derive(Debug)]
pub enum StreamError {
    PlayStreamError(cpal::PlayStreamError),
    DefaultStreamConfigError(cpal::DefaultStreamConfigError),
    BuildStreamError(cpal::BuildStreamError),
    SupportedStreamConfigsError(cpal::SupportedStreamConfigsError),
    NoDevice,
}

impl From<cpal::DefaultStreamConfigError> for StreamError {
    fn from(err: cpal::DefaultStreamConfigError) -> Self {
        Self::DefaultStreamConfigError(err)
    }
}

impl From<cpal::SupportedStreamConfigsError> for StreamError {
    fn from(err: cpal::SupportedStreamConfigsError) -> Self {
        Self::SupportedStreamConfigsError(err)
    }
}

impl From<cpal::BuildStreamError> for StreamError {
    fn from(err: cpal::BuildStreamError) -> Self {
        Self::BuildStreamError(err)
    }
}

impl From<cpal::PlayStreamError> for StreamError {
    fn from(err: cpal::PlayStreamError) -> Self {
        Self::PlayStreamError(err)
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::PlayStreamError(e) => e.fmt(f),
            Self::BuildStreamError(e) => e.fmt(f),
            Self::DefaultStreamConfigError(e) => e.fmt(f),
            Self::SupportedStreamConfigsError(e) => e.fmt(f),
            Self::NoDevice => write!(f, "NoDevice"),
        }
    }
}

impl error::Error for StreamError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::PlayStreamError(e) => Some(e),
            Self::BuildStreamError(e) => Some(e),
            Self::DefaultStreamConfigError(e) => Some(e),
            Self::SupportedStreamConfigsError(e) => Some(e),
            Self::NoDevice => None,
        }
    }
}

/// Extensions to `cpal::Device`
pub(crate) trait CpalDeviceExt {
    fn new_output_stream_with_format(
        &self,
        format: cpal::SupportedStreamConfig,
        mixer_rx: Arc<Mutex<DynamicMixer<f32>>>,
    ) -> Result<(Arc<Condvar>, cpal::Stream), cpal::BuildStreamError>;
}

impl CpalDeviceExt for cpal::Device {
    fn new_output_stream_with_format(
        &self,
        format: cpal::SupportedStreamConfig,
        mixer_rx: Arc<Mutex<DynamicMixer<f32>>>,
    ) -> Result<(Arc<Condvar>, cpal::Stream), cpal::BuildStreamError> {
        let device_broken = Arc::new(Condvar::new());

        let error_callback = {
            let device_broken = device_broken.clone();

            move |err| {
                eprintln!("an error occurred on output stream: {}", err);
                device_broken.notify_one();
            }
        };

        match format.sample_format() {
            cpal::SampleFormat::F32 => self.build_output_stream::<f32, _, _>(
                &format.config(),
                move |data, _| {
                    data.iter_mut()
                        .for_each(|d| *d = mixer_rx.lock().unwrap().next().unwrap_or(0f32))
                },
                error_callback,
            ),
            cpal::SampleFormat::I16 => self.build_output_stream::<i16, _, _>(
                &format.config(),
                move |data, _| {
                    data.iter_mut()
                        .for_each(|d| *d = mixer_rx.lock().unwrap().next().map(|s| s.to_i16()).unwrap_or(0i16))
                },
                error_callback,
            ),
            cpal::SampleFormat::U16 => self.build_output_stream::<u16, _, _>(
                &format.config(),
                move |data, _| {
                    data.iter_mut().for_each(|d| {
                        *d = mixer_rx
                            .lock().unwrap()
                            .next()
                            .map(|s| s.to_u16())
                            .unwrap_or(u16::max_value() / 2)
                    })
                },
                error_callback,
            ),
        }
        .map(|stream| (device_broken, stream))
    }
}

/// All the supported output formats with sample rates
fn supported_output_formats(
    device: &cpal::Device,
) -> Result<impl Iterator<Item = cpal::SupportedStreamConfig>, StreamError> {
    const HZ_44100: cpal::SampleRate = cpal::SampleRate(44_100);

    let mut supported: Vec<_> = device.supported_output_configs()?.collect();
    supported.sort_by(|a, b| b.cmp_default_heuristics(a));

    Ok(supported.into_iter().flat_map(|sf| {
        let max_rate = sf.max_sample_rate();
        let min_rate = sf.min_sample_rate();
        let mut formats = vec![sf.clone().with_max_sample_rate()];
        if HZ_44100 < max_rate && HZ_44100 > min_rate {
            formats.push(sf.clone().with_sample_rate(HZ_44100))
        }
        formats.push(sf.with_sample_rate(min_rate));
        formats
    }))
}
