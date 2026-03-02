use anyhow::Context as _;
use bytes::BytesMut;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use std::sync::LazyLock;
use std::sync::Arc;
use std::sync::Mutex;
use url::Url;

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

#[derive(Default)]
struct State {
	pub media: Option<moq_mux::import::Fmp4>,
	pub buffer: BytesMut,
}

#[derive(Default)]
pub struct MoqSink {
	settings: Mutex<Settings>,
	state: Arc<Mutex<State>>,
}

#[glib::object_subclass]
impl ObjectSubclass for MoqSink {
	const NAME: &'static str = "MoqSink";
	type Type = super::MoqSink;
	type ParentType = gst_base::BaseSink;

	fn new() -> Self {
		Self::default()
	}
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
			let caps = gst::Caps::builder("video/quicktime")
				.field("variant", "iso-fragmented")
				.build();

			let pad_template =
				gst::PadTemplate::new("sink", gst::PadDirection::Sink, gst::PadPresence::Always, &caps).unwrap();

			vec![pad_template]
		});
		PAD_TEMPLATES.as_ref()
	}
}

impl BaseSinkImpl for MoqSink {
	fn start(&self) -> Result<(), gst::ErrorMessage> {
		let _guard = RUNTIME.enter();
		self.setup()
			.map_err(|e| gst::error_msg!(gst::ResourceError::Failed, ["Failed to connect: {}", e]))
	}

	fn stop(&self) -> Result<(), gst::ErrorMessage> {
		Ok(())
	}

	fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
		let _guard = RUNTIME.enter();
		let data = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

		let mut state = self.state.lock().unwrap();

		// Append incoming data to our buffer
		state.buffer.extend_from_slice(data.as_slice());

		// Take media out temporarily to avoid borrow conflict
		let mut media = state.media.take().expect("not initialized");

		// Try to decode what we have buffered
		let result = media.decode(&mut state.buffer);

		// Put media back
		state.media = Some(media);

		if let Err(e) = result {
			gst::error!(gst::CAT_DEFAULT, "Failed to decode: {}", e);
			return Err(gst::FlowError::Error);
		}

		Ok(gst::FlowSuccess::Ok)
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
			let _session = client.connect(url).await.context("failed to connect")?;

			let media = moq_mux::import::Fmp4::new(broadcast, catalog, Default::default());

			let mut state = self.state.lock().unwrap();
			state.media = Some(media);

			anyhow::Ok(())
		})
	}
}
