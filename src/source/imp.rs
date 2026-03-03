use anyhow::Context as _;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::LazyLock;
use std::sync::Mutex;

static CAT: LazyLock<gst::DebugCategory> =
	LazyLock::new(|| gst::DebugCategory::new("moq-src", gst::DebugColorFlags::empty(), Some("MoQ Source Element")));

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
pub struct MoqSrc {
	settings: Mutex<Settings>,
	session: Mutex<Option<moq_lite::Session>>,
	tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

#[glib::object_subclass]
impl ObjectSubclass for MoqSrc {
	const NAME: &'static str = "MoqSrc";
	type Type = super::MoqSrc;
	type ParentType = gst::Bin;

	fn new() -> Self {
		Self::default()
	}
}

impl GstObjectImpl for MoqSrc {}
impl BinImpl for MoqSrc {}

impl ObjectImpl for MoqSrc {
	fn properties() -> &'static [glib::ParamSpec] {
		static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
			vec![
				glib::ParamSpecString::builder("url")
					.nick("Source URL")
					.blurb("Connect to the given URL")
					.build(),
				glib::ParamSpecString::builder("broadcast")
					.nick("Broadcast")
					.blurb("The name of the broadcast to consume")
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

impl ElementImpl for MoqSrc {
	fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
		static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
			gst::subclass::ElementMetadata::new(
				"MoQ Src",
				"Source/Network/MoQ",
				"Receives media over the network via MoQ",
				"Luke Curley <kixelated@gmail.com>",
			)
		});

		Some(&*ELEMENT_METADATA)
	}

	fn pad_templates() -> &'static [gst::PadTemplate] {
		static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
			let video = gst::PadTemplate::new(
				"video_%u",
				gst::PadDirection::Src,
				gst::PadPresence::Sometimes,
				&gst::Caps::new_any(),
			)
			.unwrap();

			let audio = gst::PadTemplate::new(
				"audio_%u",
				gst::PadDirection::Src,
				gst::PadPresence::Sometimes,
				&gst::Caps::new_any(),
			)
			.unwrap();

			vec![video, audio]
		});

		PAD_TEMPLATES.as_ref()
	}

	fn change_state(&self, transition: gst::StateChange) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
		match transition {
			gst::StateChange::ReadyToPaused => {
				if let Err(e) = RUNTIME.block_on(self.setup()) {
					gst::error!(CAT, obj = self.obj(), "Failed to setup: {:?}", e);
					return Err(gst::StateChangeError);
				}
				// Chain up first to let the bin handle the state change
				let result = self.parent_change_state(transition);
				result?;
				// This is a live source - no preroll needed
				return Ok(gst::StateChangeSuccess::NoPreroll);
			}

			gst::StateChange::PausedToReady => {
				// Cleanup publisher
				self.cleanup();
			}

			_ => (),
		}

		// Chain up for other transitions
		self.parent_change_state(transition)
	}
}

impl MoqSrc {
	async fn setup(&self) -> anyhow::Result<()> {
		let (client, url, name, origin_consumer) = {
			let settings = self.settings.lock().unwrap();
			let url = url::Url::parse(settings.url.as_ref().expect("url is required"))?;
			let name = settings.broadcast.as_ref().expect("broadcast is required").clone();

			let mut config = moq_native::ClientConfig::default();
			config.tls.disable_verify = Some(settings.tls_disable_verify);

			let origin = moq_lite::Origin::produce();
			let origin_consumer = origin.consume();

			let client = config.init()?.with_consume(origin);

			(client, url, name, origin_consumer)
		};

		let session = client.connect(url).await?;
		*self.session.lock().unwrap() = Some(session);

		let broadcast = origin_consumer
			.consume_broadcast(&name)
			.ok_or_else(|| anyhow::anyhow!("Broadcast '{}' not found", name))?;

		let catalog = broadcast.subscribe_track(&hang::Catalog::default_track())?;
		let mut catalog = hang::CatalogConsumer::new(catalog);

		// TODO handle catalog updates
		let catalog = catalog.next().await?.context("no catalog found")?.clone();

		{
			for (track_name, config) in catalog.video.renditions {
				let track_ref = moq_lite::Track::new(&track_name);
				let track_consumer = broadcast.subscribe_track(&track_ref)?;
				let mut track =
					hang::container::OrderedConsumer::new(track_consumer, std::time::Duration::from_secs(1));

				let caps = match config.codec {
					hang::catalog::VideoCodec::H264(_) => {
						let builder = gst::Caps::builder("video/x-h264")
							//.field("width", video.resolution.width)
							//.field("height", video.resolution.height)
							.field("alignment", "au");

						if let Some(description) = config.description {
							builder
								.field("stream-format", "avc")
								.field("codec_data", gst::Buffer::from_slice(description.clone()))
								.build()
						} else {
							builder.field("stream-format", "annexb").build()
						}
					}
					_ => unimplemented!(),
				};

				gst::info!(CAT, "caps: {:?}", caps);

				let templ = self.obj().element_class().pad_template("video_%u").unwrap();

				let srcpad = gst::Pad::builder_from_template(&templ).name(&track_name).build();
				srcpad.set_active(true).unwrap();

				let stream_start = gst::event::StreamStart::builder(&track_name)
					.group_id(gst::GroupId::next())
					.build();
				srcpad.push_event(stream_start);

				let caps_evt = gst::event::Caps::new(&caps);
				srcpad.push_event(caps_evt);

				let segment = gst::event::Segment::new(&gst::FormattedSegment::<gst::ClockTime>::new());
				srcpad.push_event(segment);

				self.obj().add_pad(&srcpad).expect("Failed to add pad");

				// Push to the srcpad in a background task.
				let mut reference = None;
				let handle = tokio::spawn(async move {
					loop {
						match track.read().await {
							Ok(Some(frame)) => {
								let payload: Vec<u8> = frame.payload.into_iter().flatten().collect();
								let mut buffer = gst::Buffer::from_slice(payload);
								let buffer_mut = buffer.get_mut().unwrap();

								// Make timestamps relative to the first frame for proper playback
								let pts = if let Some(reference_ts) = reference {
									let timestamp: std::time::Duration = (frame.timestamp - reference_ts).into();
									gst::ClockTime::from_nseconds(timestamp.as_nanos() as _)
								} else {
									reference = Some(frame.timestamp);
									gst::ClockTime::ZERO
								};
								buffer_mut.set_pts(Some(pts));

								let mut flags = buffer_mut.flags();
								// First frame in each group is a keyframe
								match frame.index == 0 {
									true => flags.remove(gst::BufferFlags::DELTA_UNIT),
									false => flags.insert(gst::BufferFlags::DELTA_UNIT),
								};

								buffer_mut.set_flags(flags);

								gst::info!(CAT, "pushing sample: {:?}", buffer);

								if let Err(err) = srcpad.push(buffer) {
									gst::warning!(CAT, "Failed to push sample: {:?}", err);
								}
							}
							Ok(None) => {
								// Stream ended normally
								gst::info!(CAT, "Stream ended normally");
								break;
							}
							Err(e) => {
								// Handle connection errors gracefully
								gst::warning!(CAT, "Failed to read frame: {:?}", e);
								break;
							}
						}
					}
				});
				self.tasks.lock().unwrap().push(handle);
			}
		}

		{
			for (track_name, config) in catalog.audio.renditions {
				let track_ref = moq_lite::Track::new(&track_name);
				let track_consumer = broadcast.subscribe_track(&track_ref)?;
				let mut track =
					hang::container::OrderedConsumer::new(track_consumer, std::time::Duration::from_secs(1));

				let caps = match &config.codec {
					hang::catalog::AudioCodec::AAC(_aac) => {
						let builder = gst::Caps::builder("audio/mpeg")
							.field("mpegversion", 4)
							.field("channels", config.channel_count)
							.field("rate", config.sample_rate);

						if let Some(description) = config.description {
							builder
								.field("codec_data", gst::Buffer::from_slice(description.clone()))
								.field("stream-format", "aac")
								.build()
						} else {
							builder.field("stream-format", "adts").build()
						}
					}
					hang::catalog::AudioCodec::Opus => {
						let builder = gst::Caps::builder("audio/x-opus")
							.field("rate", config.sample_rate)
							.field("channels", config.channel_count);

						if let Some(description) = config.description {
							builder
								.field("codec_data", gst::Buffer::from_slice(description.clone()))
								.field("stream-format", "ogg")
								.build()
						} else {
							builder.field("stream-format", "opus").build()
						}
					}
					_ => unimplemented!(),
				};

				gst::info!(CAT, "caps: {:?}", caps);

				let templ = self.obj().element_class().pad_template("audio_%u").unwrap();

				let srcpad = gst::Pad::builder_from_template(&templ).name(&track_name).build();
				srcpad.set_active(true).unwrap();

				let stream_start = gst::event::StreamStart::builder(&track_name)
					.group_id(gst::GroupId::next())
					.build();
				srcpad.push_event(stream_start);

				let caps_evt = gst::event::Caps::new(&caps);
				srcpad.push_event(caps_evt);

				let segment = gst::event::Segment::new(&gst::FormattedSegment::<gst::ClockTime>::new());
				srcpad.push_event(segment);

				self.obj().add_pad(&srcpad).expect("Failed to add pad");

				// Push to the srcpad in a background task.
				let mut reference = None;
				let handle = tokio::spawn(async move {
					loop {
						match track.read().await {
							Ok(Some(frame)) => {
								let payload: Vec<u8> = frame.payload.into_iter().flatten().collect();
								let mut buffer = gst::Buffer::from_slice(payload);
								let buffer_mut = buffer.get_mut().unwrap();

								// Make timestamps relative to the first frame for proper playback
								let pts = if let Some(reference_ts) = reference {
									let timestamp: std::time::Duration = (frame.timestamp - reference_ts).into();
									gst::ClockTime::from_nseconds(timestamp.as_nanos() as _)
								} else {
									reference = Some(frame.timestamp);
									gst::ClockTime::ZERO
								};
								buffer_mut.set_pts(Some(pts));

								let mut flags = buffer_mut.flags();
								flags.remove(gst::BufferFlags::DELTA_UNIT);
								buffer_mut.set_flags(flags);

								gst::info!(CAT, "pushing sample: {:?}", buffer);

								if let Err(err) = srcpad.push(buffer) {
									gst::warning!(CAT, "Failed to push sample: {:?}", err);
								}
							}
							Ok(None) => {
								// Stream ended normally
								gst::info!(CAT, "Stream ended normally");
								break;
							}
							Err(e) => {
								// Handle connection errors gracefully
								gst::warning!(CAT, "Failed to read frame: {:?}", e);
								break;
							}
						}
					}
				});
				self.tasks.lock().unwrap().push(handle);
			}
		}

		// We downloaded the catalog and created all the pads.
		self.obj().no_more_pads();

		Ok(())
	}

	fn cleanup(&self) {
		for task in self.tasks.lock().unwrap().drain(..) {
			task.abort();
		}
		*self.session.lock().unwrap() = None;
	}
}
