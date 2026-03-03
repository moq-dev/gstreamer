use anyhow::Context as _;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::Mutex;
use url::Url;

static CAT: LazyLock<gst::DebugCategory> =
	LazyLock::new(|| gst::DebugCategory::new("moq-sink", gst::DebugColorFlags::empty(), Some("MoQ Sink Element")));

pub static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.worker_threads(1)
		.build()
		.unwrap()
});

#[derive(Default, Clone)]
struct Settings {
	pub url: Option<String>,
	pub broadcast: Option<String>,
	pub tls_disable_verify: bool,
}

struct PadState {
	decoder: moq_mux::import::Decoder,
	reference_pts: Option<gst::ClockTime>,
}

struct State {
	_session: moq_lite::Session,
	broadcast: moq_lite::BroadcastProducer,
	catalog: hang::CatalogProducer,
	pads: HashMap<String, PadState>,
}

#[derive(Default)]
pub struct MoqSink {
	settings: Mutex<Settings>,
	state: Mutex<Option<State>>,
}

#[glib::object_subclass]
impl ObjectSubclass for MoqSink {
	const NAME: &'static str = "MoqSink";
	type Type = super::MoqSink;
	type ParentType = gst::Element;
}

impl ObjectImpl for MoqSink {
	fn properties() -> &'static [glib::ParamSpec] {
		static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
			vec![
				glib::ParamSpecString::builder("url")
					.nick("Source URL")
					.blurb("Connect to the given URL")
					.build(),
				glib::ParamSpecString::builder("broadcast")
					.nick("Broadcast")
					.blurb("The name of the broadcast to publish")
					.build(),
				glib::ParamSpecBoolean::builder("tls-disable-verify")
					.nick("TLS disable verify")
					.blurb("Disable TLS verification")
					.default_value(false)
					.build(),
			]
		});
		PROPERTIES.as_ref()
	}

	fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
		let mut settings = self.settings.lock().unwrap();

		match pspec.name() {
			"url" => settings.url = value.get().unwrap(),
			"broadcast" => settings.broadcast = value.get().unwrap(),
			"tls-disable-verify" => settings.tls_disable_verify = value.get().unwrap(),
			_ => unimplemented!(),
		}
	}

	fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
		let settings = self.settings.lock().unwrap();

		match pspec.name() {
			"url" => settings.url.to_value(),
			"broadcast" => settings.broadcast.to_value(),
			"tls-disable-verify" => settings.tls_disable_verify.to_value(),
			_ => unimplemented!(),
		}
	}
}

impl GstObjectImpl for MoqSink {}

impl ElementImpl for MoqSink {
	fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
		static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
			gst::subclass::ElementMetadata::new(
				"MoQ Sink",
				"Sink/Network/MoQ",
				"Transmits media over the network via MoQ",
				"Luke Curley <kixelated@gmail.com>",
			)
		});

		Some(&*ELEMENT_METADATA)
	}

	fn pad_templates() -> &'static [gst::PadTemplate] {
		static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
			let mut caps = gst::Caps::new_empty();
			// Video
			caps.merge(
				gst::Caps::builder("video/x-h264")
					.field("stream-format", "byte-stream")
					.field("alignment", "au")
					.build(),
			);
			caps.merge(
				gst::Caps::builder("video/x-h265")
					.field("stream-format", "byte-stream")
					.field("alignment", "au")
					.build(),
			);
			caps.merge(gst::Caps::builder("video/x-av1").build());
			// Audio
			caps.merge(
				gst::Caps::builder("audio/mpeg")
					.field("mpegversion", 4i32)
					.field("stream-format", "raw")
					.build(),
			);
			caps.merge(gst::Caps::builder("audio/x-opus").build());

			let templ =
				gst::PadTemplate::new("sink_%u", gst::PadDirection::Sink, gst::PadPresence::Request, &caps).unwrap();

			vec![templ]
		});
		PAD_TEMPLATES.as_ref()
	}

	fn request_new_pad(
		&self,
		templ: &gst::PadTemplate,
		name: Option<&str>,
		_caps: Option<&gst::Caps>,
	) -> Option<gst::Pad> {
		let builder = gst::Pad::builder_from_template(templ)
			.chain_function(|pad, parent, buffer| {
				let element = parent
					.and_then(|p| p.downcast_ref::<super::MoqSink>())
					.ok_or(gst::FlowError::Error)?;
				element.imp().sink_chain(pad, buffer)
			})
			.event_function(|pad, parent, event| {
				let Some(element) = parent.and_then(|p| p.downcast_ref::<super::MoqSink>()) else {
					return false;
				};
				element.imp().sink_event(pad, event)
			});

		let pad = if let Some(name) = name {
			builder.name(name).build()
		} else {
			builder.build()
		};

		self.obj().add_pad(&pad).ok()?;
		Some(pad)
	}

	fn release_pad(&self, pad: &gst::Pad) {
		let pad_name = pad.name().to_string();
		if let Some(ref mut state) = *self.state.lock().unwrap() {
			state.pads.remove(&pad_name);
		}
		let _ = self.obj().remove_pad(pad);
	}

	fn change_state(&self, transition: gst::StateChange) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
		match transition {
			gst::StateChange::ReadyToPaused => {
				let _guard = RUNTIME.enter();
				self.setup().map_err(|e| {
					gst::error!(CAT, obj = self.obj(), "Failed to setup: {:?}", e);
					gst::StateChangeError
				})?;
			}
			gst::StateChange::PausedToReady => {
				*self.state.lock().unwrap() = None;
			}
			_ => (),
		}

		self.parent_change_state(transition)
	}
}

impl MoqSink {
	fn setup(&self) -> anyhow::Result<()> {
		let settings = self.settings.lock().unwrap();

		let url = settings.url.as_ref().expect("url is required");
		let url = Url::parse(url).context("invalid URL")?;
		let name = settings.broadcast.as_ref().expect("broadcast is required").clone();

		let mut config = moq_native::ClientConfig::default();
		config.tls.disable_verify = Some(settings.tls_disable_verify);

		drop(settings);

		let origin = moq_lite::Origin::produce();
		let mut broadcast = moq_lite::Broadcast::produce();
		let broadcast_consumer = broadcast.consume();
		let catalog_track = broadcast.create_track(hang::Catalog::default_track());
		let catalog = hang::CatalogProducer::new(catalog_track, Default::default());

		origin.publish_broadcast(&name, broadcast_consumer);

		let client = config.init()?.with_publish(origin.consume());

		RUNTIME.block_on(async {
			let session = client.connect(url).await.context("failed to connect")?;

			*self.state.lock().unwrap() = Some(State {
				_session: session,
				broadcast,
				catalog,
				pads: HashMap::new(),
			});

			anyhow::Ok(())
		})
	}

	fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
		match event.view() {
			gst::EventView::Caps(caps_event) => {
				let caps = caps_event.caps();
				if let Err(e) = self.handle_caps(pad, caps) {
					gst::error!(CAT, obj = pad, "Failed to handle caps: {:?}", e);
					return false;
				}
				true
			}
			_ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
		}
	}

	fn handle_caps(&self, pad: &gst::Pad, caps: &gst::CapsRef) -> anyhow::Result<()> {
		let structure = caps.structure(0).context("empty caps")?;
		let pad_name = pad.name().to_string();

		let format = match structure.name().as_str() {
			"video/x-h264" => moq_mux::import::DecoderFormat::Avc3,
			"video/x-h265" => moq_mux::import::DecoderFormat::Hev1,
			"video/x-av1" => moq_mux::import::DecoderFormat::Av01,
			"audio/mpeg" => moq_mux::import::DecoderFormat::Aac,
			"audio/x-opus" => moq_mux::import::DecoderFormat::Opus,
			other => anyhow::bail!("unsupported caps: {}", other),
		};

		let mut state = self.state.lock().unwrap();
		let state = state.as_mut().context("not connected")?;

		let mut decoder = moq_mux::import::Decoder::new(state.broadcast.clone(), state.catalog.clone(), format);

		// Initialize audio decoders that need external config
		match format {
			moq_mux::import::DecoderFormat::Aac => {
				// aacparse provides AudioSpecificConfig as codec_data in caps
				let codec_data = structure
					.get::<gst::Buffer>("codec_data")
					.context("AAC caps missing codec_data")?;
				let map = codec_data.map_readable().unwrap();
				let mut data = bytes::Bytes::copy_from_slice(map.as_slice());
				decoder.initialize(&mut data)?;
			}
			moq_mux::import::DecoderFormat::Opus => {
				// Synthesize OpusHead from caps fields
				let channels: i32 = structure.get("channels").unwrap_or(2);
				let rate: i32 = structure.get("rate").unwrap_or(48000);

				let mut opus_head = Vec::with_capacity(19);
				opus_head.extend_from_slice(b"OpusHead");
				opus_head.push(1); // version
				opus_head.push(channels as u8);
				opus_head.extend_from_slice(&0u16.to_le_bytes()); // pre_skip
				opus_head.extend_from_slice(&(rate as u32).to_le_bytes());
				opus_head.extend_from_slice(&0i16.to_le_bytes()); // gain
				opus_head.push(0); // channel mapping family

				let mut data = bytes::Bytes::from(opus_head);
				decoder.initialize(&mut data)?;
			}
			_ => {} // Video codecs self-initialize from inline data
		}

		state.pads.insert(
			pad_name.clone(),
			PadState {
				decoder,
				reference_pts: None,
			},
		);

		gst::info!(CAT, obj = pad, "Configured pad {} with format {:?}", pad_name, format);

		Ok(())
	}

	fn sink_chain(&self, pad: &gst::Pad, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
		let _guard = RUNTIME.enter();

		let pad_name = pad.name();
		let mut state = self.state.lock().unwrap();
		let state = state.as_mut().ok_or(gst::FlowError::Error)?;

		let pad_state = state.pads.get_mut(pad_name.as_str()).ok_or_else(|| {
			gst::error!(CAT, obj = pad, "Pad {} not configured", pad_name);
			gst::FlowError::Error
		})?;

		// Compute relative PTS in microseconds
		let pts = buffer.pts().and_then(|pts| {
			let reference = *pad_state.reference_pts.get_or_insert(pts);
			let relative = pts.checked_sub(reference)?;
			hang::container::Timestamp::from_micros(relative.nseconds() / 1000).ok()
		});

		let data = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
		let mut bytes = bytes::Bytes::copy_from_slice(data.as_slice());

		pad_state.decoder.decode_frame(&mut bytes, pts).map_err(|e| {
			gst::error!(CAT, obj = pad, "Failed to decode: {}", e);
			gst::FlowError::Error
		})?;

		Ok(gst::FlowSuccess::Ok)
	}
}
