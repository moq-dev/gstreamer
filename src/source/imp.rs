#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use tokio::sync::{mpsc, oneshot, watch};

use hang::moq_lite;

static CAT: Lazy<gst::DebugCategory> =
	Lazy::new(|| gst::DebugCategory::new("moq-src", gst::DebugColorFlags::empty(), Some("MoQ Source Element")));

/// Dedicated runtime with several worker threads so each element instance
/// does not contend for a single executor.
static RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
	tokio::runtime::Builder::new_multi_thread()
		.worker_threads(4)
		.enable_all()
		.build()
		.expect("spawn tokio runtime")
});

#[derive(Debug, Clone, Default)]
struct Settings {
	url: Option<String>,
	broadcast: Option<String>,
	tls_disable_verify: bool,
}

#[derive(Debug, Clone)]
struct ResolvedSettings {
	url: url::Url,
	broadcast: String,
	tls_disable_verify: bool,
}

impl TryFrom<Settings> for ResolvedSettings {
	type Error = anyhow::Error;

	fn try_from(value: Settings) -> Result<Self> {
		Ok(Self {
			url: url::Url::parse(value.url.as_ref().context("url property is required")?)?,
			broadcast: value
				.broadcast
				.as_ref()
				.context("broadcast property is required")?
				.clone(),
			tls_disable_verify: value.tls_disable_verify,
		})
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TrackKind {
	Video,
	Audio,
}

impl TrackKind {
	fn template_name(&self) -> &'static str {
		match self {
			TrackKind::Video => "video_%u",
			TrackKind::Audio => "audio_%u",
		}
	}
}

#[derive(Debug, Clone)]
struct TrackDescriptor {
	kind: TrackKind,
	name: String,
}

impl TrackDescriptor {
	fn pad_name(&self) -> String {
		match self.kind {
			TrackKind::Video => format!("video_{}", self.name),
			TrackKind::Audio => format!("audio_{}", self.name),
		}
	}
}

#[derive(Debug)]
enum ControlMessage {
	CreatePad {
		descriptor: TrackDescriptor,
		caps: gst::Caps,
		reply: oneshot::Sender<PadEndpoint>,
	},
	DropPad {
		pad_name: String,
	},
	NoMorePads,
	ReportError(anyhow::Error),
}

#[derive(Debug, Clone)]
struct PadEndpoint {
	sender: mpsc::UnboundedSender<PadMessage>,
}

impl PadEndpoint {
	fn send(&self, msg: PadMessage) -> bool {
		self.sender.send(msg).is_ok()
	}
}

#[derive(Debug)]
enum PadMessage {
	Buffer(gst::Buffer),
	Eos,
	Drop,
}

struct PadHandle {
	sender: mpsc::UnboundedSender<PadMessage>,
	task: glib::JoinHandle<()>,
}

struct SessionManager {
	shutdown: watch::Sender<bool>,
	join: tokio::task::JoinHandle<()>,
}

impl SessionManager {
	fn start(
		weak_obj: glib::WeakRef<super::MoqSrc>,
		settings: ResolvedSettings,
		control_tx: mpsc::UnboundedSender<ControlMessage>,
	) -> Self {
		let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
		let join = RUNTIME.spawn(async move {
			let result = run_session(settings, control_tx.clone(), &mut shutdown_rx).await;
			if let Err(err) = result {
				let _ = control_tx.send(ControlMessage::ReportError(err));
			}
			// Dropping `weak_obj` is fine; pads will be removed by cleanup.
			drop(weak_obj);
		});

		Self {
			shutdown: shutdown_tx,
			join,
		}
	}

	fn stop(self) {
		let _ = self.shutdown.send(true);
		RUNTIME.spawn(async move {
			if let Err(err) = self.join.await {
				gst::warning!(CAT, "session task ended with error: {err:?}");
			}
		});
	}
}

#[derive(Default)]
pub struct MoqSrc {
	settings: Mutex<Settings>,
	pads: Mutex<HashMap<String, PadHandle>>,
	control_task: Mutex<Option<glib::JoinHandle<()>>>,
	control_sender: Mutex<Option<mpsc::UnboundedSender<ControlMessage>>>,
	session: Mutex<Option<SessionManager>>,
}

#[glib::object_subclass]
impl ObjectSubclass for MoqSrc {
	const NAME: &'static str = "MoqSrcRef";
	type Type = super::MoqSrc;
	type ParentType = gst::Element;

	fn new() -> Self {
		Self::default()
	}
}

impl ObjectImpl for MoqSrc {
	fn properties() -> &'static [glib::ParamSpec] {
		static PROPS: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
			vec![
				glib::ParamSpecString::builder("url")
					.nick("Source URL")
					.blurb("Connect to the given URL")
					.build(),
				glib::ParamSpecString::builder("broadcast")
					.nick("Broadcast")
					.blurb("The broadcast name to subscribe to")
					.build(),
				glib::ParamSpecBoolean::builder("tls-disable-verify")
					.nick("TLS Disable Verify")
					.blurb("Disable TLS certificate verification")
					.default_value(false)
					.build(),
			]
		});
		PROPS.as_ref()
	}

	fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
		let mut settings = self.settings.lock().unwrap();
		match pspec.name() {
			"url" => settings.url = value.get().unwrap(),
			"broadcast" => settings.broadcast = value.get().unwrap(),
			"tls-disable-verify" => settings.tls_disable_verify = value.get().unwrap(),
			_ => unreachable!(),
		}
	}

	fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
		let settings = self.settings.lock().unwrap();
		match pspec.name() {
			"url" => settings.url.to_value(),
			"broadcast" => settings.broadcast.to_value(),
			"tls-disable-verify" => settings.tls_disable_verify.to_value(),
			_ => unreachable!(),
		}
	}

	fn constructed(&self) {
		self.parent_constructed();
		let obj = self.obj();
		self.install_control_plane(&obj);
	}
}

impl GstObjectImpl for MoqSrc {}
impl ElementImpl for MoqSrc {
	fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
		static META: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
			gst::subclass::ElementMetadata::new(
				"MoQ Src (ref)",
				"Source/Network/MoQ",
				"Receives media over the network via MoQ",
				"Luke Curley <kixelated@gmail.com>, Steve McFarlin <steve@stevemcfarlin.com>",
			)
		});
		Some(&*META)
	}

	fn pad_templates() -> &'static [gst::PadTemplate] {
		static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
			vec![
				gst::PadTemplate::new(
					"video_%u",
					gst::PadDirection::Src,
					gst::PadPresence::Sometimes,
					&gst::Caps::new_any(),
				)
				.unwrap(),
				gst::PadTemplate::new(
					"audio_%u",
					gst::PadDirection::Src,
					gst::PadPresence::Sometimes,
					&gst::Caps::new_any(),
				)
				.unwrap(),
			]
		});
		PAD_TEMPLATES.as_ref()
	}

	fn change_state(&self, transition: gst::StateChange) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
		match transition {
			gst::StateChange::ReadyToPaused => {
				if let Err(err) = self.start_session() {
					gst::error!(CAT, obj = self.obj(), "failed to start session: {err:?}");
					return Err(gst::StateChangeError);
				}
				let success = self.parent_change_state(transition)?;
				let result = match success {
					gst::StateChangeSuccess::Async => gst::StateChangeSuccess::Async,
					_ => gst::StateChangeSuccess::NoPreroll,
				};
				return Ok(result);
			}
			gst::StateChange::PausedToReady => {
				self.stop_session();
			}
			_ => (),
		}

		self.parent_change_state(transition)
	}
}

impl MoqSrc {
	fn start_session(&self) -> Result<()> {
		let settings = {
			let settings = self.settings.lock().unwrap().clone();
			ResolvedSettings::try_from(settings)?
		};

		let (control_tx, control_rx) = mpsc::unbounded_channel::<ControlMessage>();
		let obj = self.obj();
		let weak = obj.downgrade();
		let context = glib::MainContext::default();
		let control_task = spawn_main_context_forwarder(&context, control_rx, move |msg| {
			if let Some(obj) = weak.upgrade() {
				obj.imp().handle_control_message(msg);
				true
			} else {
				false
			}
		});

		*self.control_task.lock().unwrap() = Some(control_task);
		*self.control_sender.lock().unwrap() = Some(control_tx.clone());

		let session = SessionManager::start(obj.downgrade(), settings, control_tx);
		*self.session.lock().unwrap() = Some(session);
		Ok(())
	}

	fn stop_session(&self) {
		if let Some(session) = self.session.lock().unwrap().take() {
			session.stop();
		}

		if let Some(control_task) = self.control_task.lock().unwrap().take() {
			control_task.abort();
		}

		// Flush pads so downstream can reconfigure cleanly.
		let pad_handles = self.pads.lock().unwrap().drain().collect::<Vec<_>>();
		for (name, handle) in pad_handles {
			gst::debug!(CAT, "dropping pad {name}");
			let _ = handle.sender.send(PadMessage::Drop);
			handle.task.abort();
		}

		*self.control_sender.lock().unwrap() = None;
	}

	fn install_control_plane(&self, _obj: &super::MoqSrc) {
		// Nothing else to do—the plane is installed per session in start_session.
	}

	fn handle_control_message(&self, msg: ControlMessage) {
		match msg {
			ControlMessage::CreatePad {
				descriptor,
				caps,
				reply,
			} => {
				if let Err(err) = self.create_pad(descriptor, caps, reply) {
					gst::error!(CAT, obj = self.obj(), "failed to create pad: {err:?}");
				}
			}
			ControlMessage::DropPad { pad_name } => {
				if let Some(handle) = self.pads.lock().unwrap().remove(&pad_name) {
					let _ = handle.sender.send(PadMessage::Drop);
					handle.task.abort();
				}
			}
			ControlMessage::NoMorePads => {
				self.obj().no_more_pads();
			}
			ControlMessage::ReportError(err) => {
				gst::element_error!(self.obj(), gst::CoreError::Failed, ("session error"), ["{err:?}"]);
			}
		}
	}

	fn create_pad(
		&self,
		descriptor: TrackDescriptor,
		caps: gst::Caps,
		reply: oneshot::Sender<PadEndpoint>,
	) -> Result<()> {
		let obj = self.obj();
		let templ = obj
			.element_class()
			.pad_template(descriptor.kind.template_name())
			.context("missing pad template")?;

		let pad = gst::Pad::builder_from_template(&templ)
			.name(descriptor.pad_name())
			.build();

		pad.set_active(true)?;

		let stream_start = gst::event::StreamStart::builder(&descriptor.name)
			.group_id(gst::GroupId::next())
			.build();
		pad.push_event(stream_start);
		pad.push_event(gst::event::Caps::new(&caps));
		pad.push_event(gst::event::Segment::new(&gst::FormattedSegment::<gst::ClockTime>::new()));

		obj.add_pad(&pad)?;

		// Channel used by the async tasks to push buffers via the main context.
		let (pad_tx, pad_rx) = mpsc::unbounded_channel::<PadMessage>();
		let pad_clone = pad.clone();
		let weak = obj.downgrade();
		let context = glib::MainContext::default();
		let task = spawn_main_context_forwarder(&context, pad_rx, move |msg| {
			if let Some(obj) = weak.upgrade() {
				let imp = obj.imp();
				imp.dispatch_pad_message(&pad_clone, msg)
			} else {
				false
			}
		});

		self.pads.lock().unwrap().insert(
			descriptor.pad_name(),
			PadHandle {
				sender: pad_tx.clone(),
				task,
			},
		);

		let _ = reply.send(PadEndpoint { sender: pad_tx });
		Ok(())
	}

	fn dispatch_pad_message(&self, pad: &gst::Pad, msg: PadMessage) -> bool {
		match msg {
			PadMessage::Buffer(buffer) => {
				if let Err(err) = pad.push(buffer) {
					gst::warning!(CAT, "failed to push buffer: {err:?}");
					return false;
				}
				true
			}
			PadMessage::Eos => {
				pad.push_event(gst::event::Eos::builder().build());
				true
			}
			PadMessage::Drop => {
				let _ = pad.set_active(false);
				let _ = self.obj().remove_pad(pad);
				false
			}
		}
	}
}

async fn run_session(
	settings: ResolvedSettings,
	control_tx: mpsc::UnboundedSender<ControlMessage>,
	shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
	let client = moq_native::ClientConfig {
		tls: moq_native::ClientTls {
			disable_verify: Some(settings.tls_disable_verify),
			..Default::default()
		},
		..Default::default()
	}
	.init()?;

	let session = client.connect(settings.url.clone()).await?;
	let origin = moq_lite::Origin::produce();
	let _session = moq_lite::Session::connect(session, None, origin.producer).await?;

	let broadcast = origin
		.consumer
		.consume_broadcast(&settings.broadcast)
		.ok_or_else(|| anyhow::anyhow!("Broadcast '{}' not found", settings.broadcast))?;

	let catalog_track = broadcast.subscribe_track(&hang::catalog::Catalog::default_track());
	let mut catalog = hang::catalog::CatalogConsumer::new(catalog_track);
	let catalog = catalog.next().await?.context("catalog missing")?.clone();

	let mut tasks = Vec::new();

	if let Some(video) = catalog.video {
		for (track_name, config) in video.renditions {
			let descriptor = TrackDescriptor {
				kind: TrackKind::Video,
				name: track_name.clone(),
			};
			let caps = video_caps(&config)?;
			let pad_tx = request_pad(&control_tx, descriptor.clone(), caps).await?;
			let track_ref = hang::moq_lite::Track::new(&track_name);
			let track = hang::TrackConsumer::new(broadcast.subscribe_track(&track_ref), Duration::from_secs(1));
			tasks.push(spawn_track_pump(track, descriptor, pad_tx, shutdown.clone()));
		}
	}

	if let Some(audio) = catalog.audio {
		for (track_name, config) in audio.renditions {
			let descriptor = TrackDescriptor {
				kind: TrackKind::Audio,
				name: track_name.clone(),
			};
			let caps = audio_caps(&config)?;
			let pad_tx = request_pad(&control_tx, descriptor.clone(), caps).await?;
			let track_ref = hang::moq_lite::Track::new(&track_name);
			let track = hang::TrackConsumer::new(broadcast.subscribe_track(&track_ref), Duration::from_secs(1));
			tasks.push(spawn_track_pump(track, descriptor, pad_tx, shutdown.clone()));
		}
	}

	let _ = control_tx.send(ControlMessage::NoMorePads);

	for task in tasks {
		let _ = task.await;
	}

	Ok(())
}

async fn request_pad(
	control_tx: &mpsc::UnboundedSender<ControlMessage>,
	descriptor: TrackDescriptor,
	caps: gst::Caps,
) -> Result<PadEndpoint> {
	let (reply_tx, reply_rx) = oneshot::channel();
	control_tx
		.send(ControlMessage::CreatePad {
			descriptor,
			caps,
			reply: reply_tx,
		})
		.map_err(|_| anyhow::anyhow!("control plane shut down"))?;

	let endpoint = reply_rx.await.context("pad creation cancelled")?;
	Ok(endpoint)
}

fn spawn_track_pump(
	mut track: hang::TrackConsumer,
	descriptor: TrackDescriptor,
	pad_endpoint: PadEndpoint,
	mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
	RUNTIME.spawn(async move {
		let mut reference_ts = None;
		loop {
			tokio::select! {
				_ = shutdown.changed() => {
					pad_endpoint.send(PadMessage::Drop);
					break;
				}
				frame = track.read_frame() => {
					match frame {
						Ok(Some(frame)) => {
							//TODO: Think about performance here. This is an O(n) copy.
							//		It might not be possible given the buffer is from MoQ.
							let mut buffer = gst::Buffer::from_slice(frame.payload.into_iter().flatten().collect::<Vec<_>>());
							let buffer_mut = buffer.get_mut().unwrap();

							let pts = match reference_ts {
								Some(reference) => {
									let delta: Duration = (frame.timestamp - reference).into();
									gst::ClockTime::from_nseconds(delta.as_nanos() as u64)
								}
								None => {
									reference_ts = Some(frame.timestamp);
									gst::ClockTime::ZERO
								}
							};
							buffer_mut.set_pts(Some(pts));

							let mut flags = buffer_mut.flags();
							match descriptor.kind {
								TrackKind::Video => {
									if frame.keyframe {
										flags.remove(gst::BufferFlags::DELTA_UNIT);
									} else {
										flags.insert(gst::BufferFlags::DELTA_UNIT);
									}
								}
								TrackKind::Audio => {
									flags.remove(gst::BufferFlags::DELTA_UNIT);
								}
							}
							buffer_mut.set_flags(flags);

							if !pad_endpoint.send(PadMessage::Buffer(buffer)) {
								break;
							}
						}
						Ok(None) => {
							pad_endpoint.send(PadMessage::Eos);
							pad_endpoint.send(PadMessage::Drop);
							break;
						}
						Err(err) => {
							gst::warning!(CAT, "track {} failed: {err:?}", descriptor.name);
							pad_endpoint.send(PadMessage::Drop);
							break;
						}
					}
				}
			}
		}
	})
}

fn video_caps(config: &hang::catalog::VideoConfig) -> Result<gst::Caps> {
	let mut builder = gst::Caps::builder("video/x-h264").field("alignment", "au");
	if let Some(description) = &config.description {
		builder = builder
			.field("stream-format", "avc")
			.field("codec_data", gst::Buffer::from_slice(description.clone()));
	} else {
		builder = builder.field("stream-format", "annexb");
	}
	Ok(builder.build())
}

fn audio_caps(config: &hang::catalog::AudioConfig) -> Result<gst::Caps> {
	let caps = match &config.codec {
		hang::catalog::AudioCodec::AAC(_) => {
			let mut builder = gst::Caps::builder("audio/mpeg")
				.field("mpegversion", 4)
				.field("rate", config.sample_rate)
				.field("channels", config.channel_count);
			if let Some(description) = &config.description {
				builder = builder
					.field("codec_data", gst::Buffer::from_slice(description.clone()))
					.field("stream-format", "aac");
			} else {
				builder = builder.field("stream-format", "adts");
			}
			builder.build()
		}
		hang::catalog::AudioCodec::Opus => {
			let mut builder = gst::Caps::builder("audio/x-opus")
				.field("rate", config.sample_rate)
				.field("channels", config.channel_count);
			if let Some(description) = &config.description {
				builder = builder
					.field("codec_data", gst::Buffer::from_slice(description.clone()))
					.field("stream-format", "ogg");
			}
			builder.build()
		}
		_ => anyhow::bail!("unsupported audio codec"),
	};
	Ok(caps)
}

fn spawn_main_context_forwarder<T, F>(
	context: &glib::MainContext,
	mut rx: mpsc::UnboundedReceiver<T>,
	mut handler: F,
) -> glib::JoinHandle<()>
where
	T: Send + 'static,
	F: FnMut(T) -> bool + 'static,
{
	let ctx = context.clone();
	ctx.spawn_local(async move {
		while let Some(msg) = rx.recv().await {
			if !handler(msg) {
				break;
			}
		}
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{
		cell::{Cell, RefCell},
		rc::Rc,
	};

	#[test]
	fn forwarder_delivers_messages_in_order() {
		let context = glib::MainContext::new();
		context
			.with_thread_default(|| {
				let (tx, rx) = mpsc::unbounded_channel();
				let received = Rc::new(RefCell::new(Vec::new()));
				let done = Rc::new(Cell::new(false));

				let handle = spawn_main_context_forwarder(&context, rx, {
					let received = Rc::clone(&received);
					let done = Rc::clone(&done);
					move |msg: i32| {
						received.borrow_mut().push(msg);
						if received.borrow().len() >= 3 {
							done.set(true);
							false
						} else {
							true
						}
					}
				});

				tx.send(1).unwrap();
				tx.send(2).unwrap();
				tx.send(3).unwrap();
				drop(tx);

				while !done.get() {
					context.iteration(true);
				}

				handle.abort();

				assert_eq!(*received.borrow(), vec![1, 2, 3]);
			})
			.unwrap();
	}
}
