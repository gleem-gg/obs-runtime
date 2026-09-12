# The Gleem OBS runtime: a headless X11 desktop running OBS, streamed to the
# renter's browser over WebRTC with hardware H.264 encoding.
#
# Debian trixie, chosen by measurement rather than preference. The base has to
# satisfy two constraints at once:
#
#   * GStreamer must be new enough that `nvh264enc` — the element Selkies uses
#     for NVIDIA — can open an encode session against driver 610. On 1.24, the
#     newest Ubuntu 24.04 offers and what Selkies' own bundle ships, it fails
#     with "Could not configure supporting library".
#   * Python must be old enough for Selkies, which calls
#     `asyncio.get_event_loop()` with no running loop. That raises on 3.14.
#
# Fedora 44 gives GStreamer 1.28 but Python 3.14. Ubuntu 24.04 gives Python
# 3.12 but GStreamer 1.24. Trixie gives 1.26 and 3.13, and both were verified
# on the reference RTX 3060: nvh264enc encodes 1080p, and get_event_loop
# still works. Using the distribution's GStreamer also drops the 95 MB pinned
# bundle Selkies would otherwise need.
#
# Nothing NVIDIA is baked in. The driver's userspace is injected at runtime by
# nvidia-container-toolkit through CDI, which keeps this image freely
# redistributable and lets one image work across driver versions.
#
# X11 rather than Wayland: Selkies is X11, OBS's capture sources and NVENC are
# mature there, and a headless Wayland compositor is a moving part with no
# payoff for a desktop nobody is sitting in front of.

FROM docker.io/library/rust:1-bookworm AS init-build

WORKDIR /build
COPY init/Cargo.toml init/Cargo.lock ./
COPY init/src ./src
RUN cargo build --release --locked


FROM docker.io/library/debian:trixie

ARG SELKIES_VERSION=1.6.2
ARG SELKIES_RELEASE=https://github.com/selkies-project/selkies/releases/download/v1.6.2

ENV DEBIAN_FRONTEND=noninteractive \
    DISPLAY=:0 \
    PULSE_SERVER=unix:/run/pulse/native \
    GLEEM_RESOLUTION=1920x1080 \
    GLEEM_FRAMERATE=30 \
    GLEEM_ENCODER=nvh264enc

RUN apt-get update && apt-get install -y --no-install-recommends \
        # The display: a virtual X server and just enough window manager that
        # OBS's dialogs behave.
        xvfb x11-utils x11-xserver-utils openbox \
        # Selkies shells out to xsel for clipboard sync in both directions.
        xsel \
        # Audio. OBS refuses to configure an audio source without a sink.
        pulseaudio pulseaudio-utils \
        # OBS itself. obs-websocket has been built in since OBS 28.
        obs-studio \
        # GStreamer 1.26 from the distribution. nvcodec — and therefore a
        # working nvh264enc — is in plugins-bad; webrtcbin needs libnice for
        # ICE, which is packaged separately.
        gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad gstreamer1.0-nice gstreamer1.0-pulseaudio \
        gstreamer1.0-tools \
        # PyGObject plus gst-python's overrides. Both are needed: without the
        # overrides `Gst.Fraction` does not exist and Selkies reports the
        # unhelpful "could not find working GStreamer-Python installation".
        python3 python3-pip python3-venv python3-gi python3-gst-1.0 \
        # GstWebRTC's typelib lives with plugins-bad and is not pulled in by
        # python3-gst-1.0; without it Selkies cannot import GstWebRTC.
        gir1.2-gst-plugins-bad-1.0 \
        dbus-x11 ca-certificates curl xz-utils \
        # Fonts, or OBS renders text as boxes.
        fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*

# The Python component that drives the pipeline and terminates signalling.
# Built in a throwaway toolchain: some of its dependencies are C extensions,
# and a compiler has no business staying in an image a renter gets a desktop
# on.
#
# setuptools is not incidental: one of Selkies' dependencies still imports
# `distutils`, which Python 3.12 removed, and setuptools is what puts the
# shim back.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential python3-dev linux-libc-dev \
    && python3 -m venv --system-site-packages /opt/selkies \
    && /opt/selkies/bin/pip install --no-cache-dir \
        setuptools \
        # GStreamer's `cudaconvert` — which Selkies' NVIDIA pipeline builds —
        # needs NVRTC. That is part of the CUDA toolkit rather than the
        # driver, so neither the base image nor the container runtime's device
        # injection provides it, and without it the element simply does not
        # register and the pipeline dies mid-negotiation. The wheel ships the
        # one library needed, at a fraction of the toolkit's size.
        nvidia-cuda-nvrtc-cu12 \
        "${SELKIES_RELEASE}/selkies_gstreamer-${SELKIES_VERSION}-py3-none-any.whl" \
    # The wheel ships libnvrtc.so.12; GStreamer dlopens the bare SONAME.
    && ln -sf /opt/selkies/lib/python3.13/site-packages/nvidia/cuda_nvrtc/lib/libnvrtc.so.12 \
              /opt/selkies/lib/python3.13/site-packages/nvidia/cuda_nvrtc/lib/libnvrtc.so \
    && apt-get purge -y build-essential python3-dev linux-libc-dev \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

# The web client. Served to the browser by Gleem, not from here — this copy is
# what the version pin is anchored to, so client and server cannot drift.
RUN curl -fsSL "${SELKIES_RELEASE}/selkies-gstreamer-web_v${SELKIES_VERSION}.tar.gz" \
        -o /tmp/selkies-web.tar.gz \
    && mkdir -p /opt/selkies-web \
    && tar -xzf /tmp/selkies-web.tar.gz -C /opt/selkies-web --strip-components=1 \
    && rm /tmp/selkies-web.tar.gz

# Selkies ships neither the Python package nor the web bundle with its licence,
# and MPL-2.0 requires the licence to travel with the code. Every Debian
# package in this image carries its own /usr/share/doc/<pkg>/copyright; these
# two are the only payloads dpkg knows nothing about, so they are the only ones
# that need saying out loud. Pinned to the same tag as the code above, so a
# version bump cannot leave the licence describing a different release.
RUN mkdir -p /usr/share/doc/selkies \
    && curl -fsSL -o /usr/share/doc/selkies/LICENSE \
        "https://raw.githubusercontent.com/selkies-project/selkies/v${SELKIES_VERSION}/LICENSE" \
    && cp /usr/share/doc/selkies/LICENSE /opt/selkies-web/LICENSE

COPY LICENSE NOTICE /usr/share/doc/gleem-obs-runtime/

COPY --from=init-build /build/target/release/runtime-init /usr/local/bin/runtime-init

# Everything a rental may write that outlives it goes here, and this is the
# only path bind-mounted from the host's encrypted workspace.
RUN mkdir -p /workspace /run/pulse \
        /root/.config/obs-studio/basic/scenes /root/.config/obs-studio/basic/profiles \
    && chmod 0777 /workspace

# Loopback only in practice: the agent publishes this on 127.0.0.1 and nothing
# outside the machine can address it.
EXPOSE 8082

ENTRYPOINT ["/usr/local/bin/runtime-init"]
