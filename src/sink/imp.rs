#![allow(dead_code)]

//! Reference implementation of `MoqSink` mirroring the refactored source
//! element. This sketch keeps network I/O inside an async session controller
//! and pushes buffers to it via bounded channels so the GStreamer streaming
//! thread never blocks on QUIC or CMAF parsing.

use std::sync::{LazyLock, Mutex};

use anyhow::{Context, Result};
use bytes::BytesMut;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use once_cell::sync::Lazy;
use tokio::sync::{mpsc, watch};
use url::Url;

use hang::moq_lite;

static RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
        .expect("spawn tokio runtime")
});

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "moq-sink-ref",
        gst::DebugColorFlags::empty(),
        Some("MoQ Sink (refactor)"),
    )
});

#[derive(Debug, Clone, Default)]
struct Settings {
    url: Option<String>,
    broadcast: Option<String>,
    tls_disable_verify: bool,
}

#[derive(Debug, Clone)]
struct ResolvedSettings {
    url: Url,
    broadcast: String,
    tls_disable_verify: bool,
}

impl TryFrom<Settings> for ResolvedSettings {
    type Error = anyhow::Error;

    fn try_from(value: Settings) -> Result<Self> {
        Ok(Self {
            url: Url::parse(value.url.as_ref().context("url property is required")?)?,
            broadcast: value
                .broadcast
                .as_ref()
                .context("broadcast property is required")?
                .clone(),
            tls_disable_verify: value.tls_disable_verify,
        })
    }
}

struct SessionHandle {
    sender: mpsc::Sender<BufferPayload>,
    shutdown: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl SessionHandle {
    fn stop(self) {
        let _ = self.shutdown.send(true);
        let join = self.join;
        RUNTIME.spawn(async move {
            if let Err(err) = join.await {
                gst::warning!(CAT, "session task ended with error: {err:?}");
            }
        });
    }
}

#[derive(Clone, Debug)]
struct BufferPayload {
    data: bytes::Bytes,
}

#[derive(Default)]
pub struct MoqSink {
    settings: Mutex<Settings>,
    writer: Mutex<Option<mpsc::Sender<BufferPayload>>>,
    session: Mutex<Option<SessionHandle>>,
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
        static PROPS: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("url")
                    .nick("Relay URL")
                    .blurb("Connect and publish to the given URL")
                    .build(),
                glib::ParamSpecString::builder("broadcast")
                    .nick("Broadcast")
                    .blurb("Broadcast name to publish")
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
}

impl GstObjectImpl for MoqSink {}

impl ElementImpl for MoqSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "MoQ Sink (ref)",
                "Sink/Network/MoQ",
                "Publishes CMAF fragments over MoQ",
                "Luke Curley <kixelated@gmail.com>, Steve McFarlin <steve@stevemcfarlin.com>",
            )
        });
        Some(&*META)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("video/quicktime")
                .field("variant", "iso-fragmented")
                .build();
            let pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            vec![pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}

impl BaseSinkImpl for MoqSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.start_session().map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::Failed,
                ["failed to start MoQ session: {err:#}"]
            )
        })
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        self.stop_session();
        Ok(())
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
        let sender = self
            .writer
            .lock()
            .unwrap()
            .clone()
            .ok_or(gst::FlowError::Flushing)?;

        sender
            .blocking_send(BufferPayload {
                data: bytes::Bytes::copy_from_slice(map.as_slice()),
            })
            .map_err(|_| gst::FlowError::Flushing)?;

        Ok(gst::FlowSuccess::Ok)
    }
}

impl MoqSink {
    fn start_session(&self) -> Result<()> {
        let settings = {
            let settings = self.settings.lock().unwrap().clone();
            ResolvedSettings::try_from(settings)?
        };

        let (tx, rx) = mpsc::channel::<BufferPayload>(32);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let join = RUNTIME.spawn(run_session(settings, rx, shutdown_rx));

        *self.writer.lock().unwrap() = Some(tx.clone());
        *self.session.lock().unwrap() = Some(SessionHandle {
            sender: tx,
            shutdown: shutdown_tx,
            join,
        });

        Ok(())
    }

    fn stop_session(&self) {
        if let Some(session) = self.session.lock().unwrap().take() {
            session.stop();
        }
        self.writer.lock().unwrap().take();
    }
}

async fn run_session(
    settings: ResolvedSettings,
    mut rx: mpsc::Receiver<BufferPayload>,
    mut shutdown: watch::Receiver<bool>,
) {
    let client = match (moq_native::ClientConfig {
        tls: moq_native::ClientTls {
            disable_verify: Some(settings.tls_disable_verify),
            ..Default::default()
        },
        ..Default::default()
    })
    .init()
    {
        Ok(client) => client,
        Err(err) => {
            gst::error!(CAT, "failed to init client: {err:#}");
            return;
        }
    };

    let session = match client.connect(settings.url.clone()).await {
        Ok(session) => session,
        Err(err) => {
            gst::error!(CAT, "failed to connect: {err:#}");
            return;
        }
    };

    let origin = moq_lite::Origin::produce();
    let broadcast = moq_lite::Broadcast::produce();

    origin
        .producer
        .publish_broadcast(&settings.broadcast, broadcast.consumer);

    if let Err(err) = moq_lite::Session::connect(session, origin.consumer, None).await {
        gst::error!(CAT, "session handshake failed: {err:#}");
        return;
    }

    let mut importer = hang::import::Fmp4::new(broadcast.producer.into());
    let mut buffer = BytesMut::new();

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                break;
            }
            chunk = rx.recv() => {
                match chunk {
                    Some(chunk) => {
                        buffer.extend_from_slice(&chunk.data);
                        if let Err(err) = importer.decode(&mut buffer) {
                            gst::warning!(CAT, "failed to decode CMAF fragment: {err:#}");
                            buffer.clear();
                        }
                    }
                    None => break,
                }
            }
        }
    }
}
