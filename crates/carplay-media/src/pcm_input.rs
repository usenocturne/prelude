use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use super::{gst_error, make, set_checked, set_enum_checked};
use crate::{Error, Result};

pub const PCM_INPUT_SAMPLE_RATE: u32 = 48_000;
pub const PCM_INPUT_CHANNELS: u32 = 2;
const PCM_INPUT_QUEUE_BYTES: u64 = (PCM_INPUT_SAMPLE_RATE * PCM_INPUT_CHANNELS * 2) as u64;
const MIXER_INPUT_QUEUE_NS: u64 = 200_000_000;
const FALLBACK_OUTPUT_LATENCY: Duration = Duration::from_millis(120);
const DEVICE_BUFFER_ALLOWANCE: Duration = Duration::from_millis(40);

pub struct PcmInput {
    pipeline: gst::Pipeline,
    mixer: gst::Element,
    mixer_pad: gst::Pad,
    elements: Vec<gst::Element>,
    source: gst_app::AppSrc,
    output_latency: Duration,
}

impl std::fmt::Debug for PcmInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PcmInput")
            .field("output_latency", &self.output_latency)
            .finish_non_exhaustive()
    }
}

impl PcmInput {
    pub(super) fn attach(pipeline: &gst::Pipeline) -> Result<Self> {
        let mixer = pipeline
            .by_name("carplay-mixer")
            .ok_or_else(|| Error::Backend("the audio mixer has no mixer element".to_owned()))?;
        let mix_caps = pipeline
            .by_name("float-mixer-output")
            .ok_or_else(|| Error::Backend("the audio mixer has no mix caps".to_owned()))?
            .property::<gst::Caps>("caps");
        let source_element = make("appsrc", "video-pcm-source")?;
        let source = source_element
            .clone()
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| Error::Backend("video PCM appsrc has the wrong type".to_owned()))?;
        source.set_caps(Some(
            &gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .field("rate", PCM_INPUT_SAMPLE_RATE as i32)
                .field("channels", PCM_INPUT_CHANNELS as i32)
                .field("layout", "interleaved")
                .build(),
        ));
        source.set_format(gst::Format::Time);
        source.set_is_live(true);
        source.set_do_timestamp(true);
        source.set_block(false);
        source.set_max_bytes(PCM_INPUT_QUEUE_BYTES);
        set_enum_checked(&source, "leaky-type", "downstream")?;
        let input_queue = make("queue", "video-pcm-input-queue")?;
        set_checked(&input_queue, "max-size-time", 2_000_000_000_u64)?;
        set_checked(&input_queue, "max-size-bytes", 0_u32)?;
        set_checked(&input_queue, "max-size-buffers", 0_u32)?;
        let convert = make("audioconvert", "video-pcm-converter")?;
        let resample = make("audioresample", "video-pcm-resampler")?;
        let caps = make("capsfilter", "video-pcm-mix-caps")?;
        set_checked(&caps, "caps", mix_caps)?;
        let mixer_queue = make("queue", "video-pcm-mixer-queue")?;
        set_checked(&mixer_queue, "max-size-time", MIXER_INPUT_QUEUE_NS)?;
        set_checked(&mixer_queue, "max-size-bytes", 0_u32)?;
        set_checked(&mixer_queue, "max-size-buffers", 0_u32)?;
        let elements = vec![
            source_element,
            input_queue,
            convert,
            resample,
            caps,
            mixer_queue,
        ];
        pipeline
            .add_many(&elements)
            .map_err(gst_error("cannot add the video PCM branch"))?;
        let attached = (|| {
            gst::Element::link_many(&elements)
                .map_err(gst_error("cannot link the video PCM branch"))?;
            let source_pad = elements
                .last()
                .and_then(|element| element.static_pad("src"))
                .ok_or_else(|| Error::Backend("video PCM branch has no source pad".to_owned()))?;
            let mixer_pad = mixer.request_pad_simple("sink_%u").ok_or_else(|| {
                Error::Backend("audiomixer did not allocate a video PCM pad".to_owned())
            })?;
            source_pad.link(&mixer_pad).map_err(|error| {
                Error::Backend(format!("cannot link the video PCM mixer pad: {error:?}"))
            })?;
            for element in &elements {
                element
                    .sync_state_with_parent()
                    .map_err(gst_error("cannot start the video PCM branch"))?;
            }
            Ok(mixer_pad)
        })();
        let mixer_pad = match attached {
            Ok(mixer_pad) => mixer_pad,
            Err(error) => {
                remove_elements(pipeline, &elements);
                return Err(error);
            }
        };
        Ok(Self {
            pipeline: pipeline.clone(),
            mixer,
            mixer_pad,
            elements,
            source,
            output_latency: output_latency(pipeline),
        })
    }

    pub fn push(&self, samples: &[u8]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        self.source
            .push_buffer(gst::Buffer::from_slice(samples.to_vec()))
            .map(|_| ())
            .map_err(|error| Error::Backend(format!("the audio mixer refused video PCM: {error}")))
    }

    pub fn output_latency(&self) -> Duration {
        self.output_latency
    }
}

impl Drop for PcmInput {
    fn drop(&mut self) {
        if let Some(source_pad) = self
            .elements
            .last()
            .and_then(|element| element.static_pad("src"))
        {
            let _ = source_pad.unlink(&self.mixer_pad);
        }
        self.mixer.release_request_pad(&self.mixer_pad);
        remove_elements(&self.pipeline, &self.elements);
    }
}

fn remove_elements(pipeline: &gst::Pipeline, elements: &[gst::Element]) {
    for element in elements {
        let _ = element.set_state(gst::State::Null);
    }
    let _ = pipeline.remove_many(elements);
}

fn output_latency(pipeline: &gst::Pipeline) -> Duration {
    let mut query = gst::query::Latency::new();
    if !pipeline.query(&mut query) {
        return FALLBACK_OUTPUT_LATENCY;
    }
    let (_, minimum, _) = query.result();
    Duration::from_nanos(minimum.nseconds()) + DEVICE_BUFFER_ALLOWANCE
}
