#!/usr/bin/env just --justfile

# Using Just: https://github.com/casey/just?tab=readme-ov-file#installation

export RUST_BACKTRACE := "1"
export RUST_LOG := "info"
export URL := "http://localhost:4443"
#export GST_DEBUG:="*:4"

# List all of the available commands.
default:
  just --list

# Install any required dependencies.
setup:
	# Upgrade Rust
	rustup update

	# Make sure the right components are installed.
	rustup component add rustfmt clippy

	# Install cargo binstall if needed.
	cargo install cargo-binstall

	# Install cargo shear if needed.
	cargo binstall --no-confirm cargo-shear

# Download the video and convert it to a fragmented MP4 that we can stream
download name url:
	if [ ! -f dev/{{name}}.mp4 ]; then \
		wget {{url}} -O dev/{{name}}.mp4; \
	fi

	if [ ! -f dev/{{name}}.fmp4 ]; then \
		ffmpeg -i dev/{{name}}.mp4 \
			-c copy \
			-f mp4 -movflags cmaf+separate_moof+delay_moov+skip_trailer+frag_every_frame \
			dev/{{name}}.fmp4; \
	fi

# Publish a video using gstreamer to the localhost relay server
pub broadcast: (download "bbb" "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4")
	# Build the plugins
	cargo build

	# Run gstreamer and pipe the output to our plugin
	GST_PLUGIN_PATH="${PWD}/target/debug${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}" \
	gst-launch-1.0 -v -e multifilesrc location="dev/bbb.fmp4" loop=true ! qtdemux name=demux \
		demux.video_0 ! h264parse ! queue ! identity sync=true ! isofmp4mux name=mux chunk-duration=1 fragment-duration=1 ! moqsink url="$URL" broadcast="{{broadcast}}" tls-disable-verify=true \
		demux.audio_0 ! aacparse ! queue ! mux.

# Subscribe to a video using gstreamer
sub broadcast:
	# Build the plugins
	cargo build

	# Run gstreamer and pipe the output to our plugin
	# This will render the video to the screen
	GST_PLUGIN_PATH="${PWD}/target/debug${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}" \
	gst-launch-1.0 -v -e moqsrc url="$URL" broadcast="{{broadcast}}" tls-disable-verify=true ! decodebin ! videoconvert ! autovideosink

# Run the CI checks
check $RUSTFLAGS="-D warnings":
	cargo check --all-targets
	cargo clippy --all-targets -- -D warnings
	cargo fmt -- --check
	cargo shear # requires: cargo binstall cargo-shear

# Run any CI tests
test $RUSTFLAGS="-D warnings":
	cargo test

# Automatically fix some issues.
fix:
	cargo fix --allow-staged --all-targets --all-features
	cargo clippy --fix --allow-staged --all-targets --all-features
	cargo fmt --all
	cargo shear --fix

# Upgrade any tooling
upgrade:
	rustup upgrade

	# Install cargo-upgrades if needed.
	cargo install cargo-upgrades cargo-edit
	cargo upgrade

# Build the plugins
build:
	cargo build
