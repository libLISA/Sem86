use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, FromSample, I24, SampleRate, SizedSample, Stream, StreamConfig, SupportedBufferSize,
    SupportedStreamConfig, SupportedStreamConfigRange,
};
use log::{debug, error, info};

use crate::hw::sound::backends::Frontend;

pub struct DeviceBackend {
    _device: Device,
    stream: Stream,
    buffer_size: u32,
}

impl std::fmt::Debug for DeviceBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceBackend").finish()
    }
}

impl Drop for DeviceBackend {
    fn drop(&mut self) {
        self.stream.pause().ok();
    }
}

impl DeviceBackend {
    pub fn new(frequency: u32, frontend: impl Frontend + 'static) -> Result<Self, ()> {
        let host = cpal::default_host();
        let device = host.default_output_device().unwrap();
        for item in device.supported_output_configs().unwrap() {
            debug!("Supported config: {item:?}");
        }

        let config = device.default_output_config().unwrap();
        let config_range = device
            .supported_output_configs()
            .unwrap()
            .filter(|c| c.channels() == config.channels() && c.sample_format().sample_size() >= 2)
            // Find the supported sample rate that is closest to the desired rate.
            .min_by_key(|c| {
                let buffer_size = if let SupportedBufferSize::Range {
                    min, ..
                } = c.buffer_size()
                {
                    *min
                } else {
                    u32::MAX
                };

                let frequency_delta = if frequency < c.min_sample_rate().0 {
                    c.min_sample_rate().0 - frequency
                } else if frequency > c.max_sample_rate().0 {
                    frequency - c.max_sample_rate().0
                } else {
                    0
                };

                (frequency_delta, buffer_size)
            })
            .unwrap();

        info!("Supported config range: {config_range:#?}");

        match config_range.sample_format() {
            cpal::SampleFormat::I8 => Self::build::<i8>(device, config, config_range, frequency, frontend),
            cpal::SampleFormat::I16 => Self::build::<i16>(device, config, config_range, frequency, frontend),
            cpal::SampleFormat::I24 => Self::build::<I24>(device, config, config_range, frequency, frontend),
            cpal::SampleFormat::I32 => Self::build::<i32>(device, config, config_range, frequency, frontend),
            cpal::SampleFormat::I64 => Self::build::<i64>(device, config, config_range, frequency, frontend),
            cpal::SampleFormat::U8 => Self::build::<u8>(device, config, config_range, frequency, frontend),
            cpal::SampleFormat::U16 => Self::build::<u16>(device, config, config_range, frequency, frontend),
            cpal::SampleFormat::U32 => Self::build::<u32>(device, config, config_range, frequency, frontend),
            cpal::SampleFormat::U64 => Self::build::<u64>(device, config, config_range, frequency, frontend),
            cpal::SampleFormat::F32 => Self::build::<f32>(device, config, config_range, frequency, frontend),
            cpal::SampleFormat::F64 => Self::build::<f64>(device, config, config_range, frequency, frontend),
            sample_format => panic!("Unsupported sample format '{sample_format}'"),
        }
    }

    pub fn build<T>(
        device: cpal::Device, config: SupportedStreamConfig, config_range: SupportedStreamConfigRange, frequency: u32,
        frontend: impl Frontend + 'static,
    ) -> Result<Self, ()>
    where
        T: SizedSample + FromSample<f32>,
    {
        // TODO: We might have multiple items here...

        // TODO: Take frequency into account when deciding on buffer size
        let buffer_size = match config_range.buffer_size() {
            SupportedBufferSize::Range {
                min,
                max,
            } => (*min).max(128).min(*max),
            SupportedBufferSize::Unknown => 128,
        };

        let mut config = StreamConfig::from(config);
        let channels = config.channels as usize;
        config.buffer_size = BufferSize::Fixed(buffer_size);
        config.sample_rate = SampleRate(
            frequency
                .max(config_range.min_sample_rate().0)
                .min(config_range.max_sample_rate().0),
        );

        info!("Creating a stream with buffer size {buffer_size}");

        let err_fn = |err| eprintln!("an error occurred on stream: {err}");
        let stream = match device.build_output_stream(
            &config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                frontend.fill_buffer(data, channels);
            },
            err_fn,
            None,
        ) {
            Ok(stream) => stream,
            Err(e) => {
                error!(
                    "Unable to create stream with buffer size {buffer_size} and config {config:?}: {e}\n\nConfig range used: {config_range:?}\n\nAll config ranges:\n{:#?}",
                    device.supported_output_configs().unwrap().collect::<Vec<_>>()
                );
                return Err(())
            },
        };
        stream.play().unwrap();
        std::thread::sleep(Duration::from_millis(30));

        Ok(Self {
            _device: device,
            stream,
            buffer_size,
        })
    }

    pub fn buffer_size(&self) -> u32 {
        self.buffer_size
    }
}
