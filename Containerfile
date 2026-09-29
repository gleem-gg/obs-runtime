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


# The vendored Selkies files, staged as an image layer so the build can mount
# them without copying them into the final image. A bind mount straight from
# the build context would do the same, but under rootless podman on an SELinux
# host it arrives unreadable, and the option that fixes that is podman-only.
FROM scratch AS selkies
COPY vendor/selkies /


# OBS Studio, built from source rather than taken from Debian. Debian builds
# OBS without the browser source, because that needs the Chromium Embedded
# Framework and Debian does not package CEF; a browser source is how most
# streamers put alerts, chat and overlays on screen, so a rental without one
# is not much of a streaming machine.
#
# Built against the CEF build OBS itself pins in CMakePresets.json for this
# release, from OBS's CDN, checked against OBS's own hash. Pinned by commit,
# like the plugin below; bump the version, the commit and the CEF values
# together, copying the CEF ones from that release's CMakePresets.json
# (dependencies → cef, ubuntu-x86_64).
FROM docker.io/library/debian:trixie AS obs

ARG OBS_VERSION=32.2.2
ARG OBS_COMMIT=ba2f32bdf791005443988a4955e963663e16b1ed
ARG CEF_VERSION=6533
ARG CEF_REVISION=6
ARG CEF_SHA256=7963335519a19ccdc5233f7334c5ab023026e2f3e9a0cc417007c09d86608146

ENV DEBIAN_FRONTEND=noninteractive

# OBS's own Ubuntu CI list, less what the build below switches off.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake ninja-build git ca-certificates curl xz-utils pkg-config \
        extra-cmake-modules libglib2.0-dev libcurl4-openssl-dev \
        libavcodec-dev libavdevice-dev libavfilter-dev libavformat-dev libavutil-dev \
        libswresample-dev libswscale-dev libjansson-dev libx264-dev libmbedtls-dev \
        libgl1-mesa-dev libgles2-mesa-dev libglvnd-dev libpulse-dev uthash-dev libsimde-dev \
        libluajit-5.1-dev python3-dev swig \
        libx11-dev libx11-xcb-dev libxcb-randr0-dev libxcb-shm0-dev libxcb-xinerama0-dev \
        libxcb-composite0-dev libxinerama-dev libxcb1-dev libxcb-xfixes0-dev libxss-dev \
        libxkbcommon-dev libatk1.0-dev libatk-bridge2.0-dev libxcomposite-dev libxdamage-dev \
        libasound2-dev libfontconfig-dev libfreetype6-dev libspeexdsp-dev libudev-dev \
        libv4l-dev libva-dev libpci-dev libdrm-dev \
        nlohmann-json3-dev libwebsocketpp-dev libasio-dev libqrcodegencpp-dev \
        libffmpeg-nvenc-dev librist-dev libsrt-openssl-dev \
        qt6-base-dev qt6-base-private-dev qt6-svg-dev libnss3-dev libgbm-dev \
    && rm -rf /var/lib/apt/lists/*

RUN git clone --quiet --filter=tree:0 https://github.com/obsproject/obs-studio.git /src \
    && git -C /src checkout --quiet --detach "$OBS_COMMIT" \
    && git -C /src submodule update --quiet --init --recursive --depth 1

# CEF arrives with its debug symbols, which make libcef.so 1.9 GB on its own.
# Stripped here, before the build copies it around, so no layer ever holds
# the unstripped file.
RUN curl -fsSLo /tmp/cef.tar.xz \
        "https://cdn-fastly.obsproject.com/downloads/cef_binary_${CEF_VERSION}_linux_x86_64_v${CEF_REVISION}.tar.xz" \
    && echo "$CEF_SHA256  /tmp/cef.tar.xz" | sha256sum -c - \
    && mkdir /cef \
    && tar --strip-components 1 -xJf /tmp/cef.tar.xz -C /cef \
    && rm /tmp/cef.tar.xz \
    && strip --strip-unneeded /cef/Release/*.so*

# Switched off: capture hardware a rental never has (AJA, DeckLink), Intel's
# encoder on NVIDIA machines, desktop plumbing this X11 box does not run
# (Wayland, PipeWire, JACK, sndio), VLC, WebRTC output, and the What's New
# dialog, which would greet every renter with OBS's release notes.
#
# Installed twice: into /usr so OBS IRL Control can build against it, and
# into /out for the runtime image. chrome-sandbox, Chromium's setuid sandbox
# helper, is dropped from the latter: obs-browser runs CEF with no_sandbox,
# and the image strips setuid bits anyway, so it would only be a dead file.
# The build copies it by name, so it cannot go any earlier.
RUN cmake -S /src -B /build -G Ninja \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr \
        -DCMAKE_INSTALL_LIBDIR=lib/x86_64-linux-gnu \
        -DOBS_VERSION_OVERRIDE="$OBS_VERSION" \
        -DENABLE_BROWSER=ON -DCEF_ROOT_DIR=/cef \
        -DENABLE_WHATSNEW=OFF \
        -DENABLE_AJA=OFF -DENABLE_DECKLINK=OFF -DENABLE_QSV11=OFF \
        -DENABLE_WAYLAND=OFF -DENABLE_PIPEWIRE=OFF -DENABLE_JACK=OFF -DENABLE_SNDIO=OFF \
        -DENABLE_VLC=OFF -DENABLE_WEBRTC=OFF \
    && cmake --build /build \
    && cmake --install /build \
    && DESTDIR=/out cmake --install /build \
    && rm -rf /build /out/usr/lib/x86_64-linux-gnu/obs-plugins/chrome-sandbox \
    && find /out -type f \( -name '*.so' -o -name '*.so.*' -o -path '*/bin/*' -o -name obs-browser-page \) \
        -exec strip --strip-unneeded {} + \
    && install -Dm644 /src/COPYING /out/usr/share/doc/obs-studio/COPYING \
    && install -Dm644 /cef/LICENSE.txt /out/usr/share/doc/obs-studio/cef/LICENSE.txt \
    && install -Dm644 /cef/README.txt /out/usr/share/doc/obs-studio/cef/README.txt

# The Debian packages that provide every library OBS links against, for the
# runtime stage to install. Derived rather than listed by hand, so a version
# bump cannot leave a library out; a library nothing provides fails the build
# here instead of failing a rental.
RUN find /out -type f \( -name '*.so' -o -name '*.so.*' -o -path '*/bin/*' -o -name obs-browser-page \) \
        | xargs ldd 2>/dev/null > /tmp/ldd \
    ; if grep -q 'not found' /tmp/ldd; then grep 'not found' /tmp/ldd | sort -u; exit 1; fi \
    && awk '$2 == "=>" && $3 ~ /^\// { print $3 }' /tmp/ldd | sort -u \
        | while read -r lib; do dpkg -S "$(realpath "$lib")" 2>/dev/null || dpkg -S "$lib" 2>/dev/null || true; done \
        | cut -d: -f1 | sort -u > /obs-packages \
    && test -s /obs-packages


# OBS IRL Control, Gleem's own plugin (GPL-2.0-or-later), built here against
# the very libobs the image ships: a plugin built against a different OBS
# release can load and then crash, and a crash takes the rental down. Building
# on top of the OBS stage makes that true by construction.
#
# Pinned by commit rather than tag, so a moved tag cannot change what runs on
# somebody else's hardware. The commit is the one the signed release tag
# points at; bump the version and the commit together.
FROM obs AS irl-control

ARG IRL_CONTROL_VERSION=1.2.0
ARG IRL_CONTROL_COMMIT=b0cf2beae828c84cccbc38c233b6cbe1fd41c360

RUN git clone --quiet https://github.com/gleem-gg/obs-irl-control.git /irl \
    && git -C /irl checkout --quiet --detach "$IRL_CONTROL_COMMIT" \
    && grep -q "VERSION $IRL_CONTROL_VERSION " /irl/CMakeLists.txt \
    && cmake -S /irl -B /irl-build -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr \
    && cmake --build /irl-build \
    && DESTDIR=/irl-out cmake --install /irl-build \
    && install -Dm644 /irl/LICENSE /irl-out/usr/share/doc/obs-irl-control/LICENSE


FROM docker.io/library/debian:trixie AS rootfs

# Selkies comes from vendor/selkies/, not from upstream. On 2026-09-23 the
# Selkies project deleted every 1.x release and tag, so the wheel and web
# bundle this image was verified with no longer exist anywhere but here. The
# wheel is pure Python, which makes the vendored copy the source as well; see
# vendor/selkies/README.md for where it came from.
ARG SELKIES_VERSION=1.6.2

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        # The display: a virtual X server and just enough window manager that
        # OBS's dialogs behave.
        xvfb x11-utils x11-xserver-utils openbox \
        # Paints the wallpaper onto the root window. Openbox draws no
        # background of its own, so without it the desktop behind OBS is black.
        hsetroot \
        # Selkies shells out to xsel for clipboard sync in both directions.
        xsel \
        # Audio. OBS refuses to configure an audio source without a sink.
        pulseaudio pulseaudio-utils \
        # Qt's SVG image and icon plugins. OBS's themes draw checkbox ticks
        # and the arrows on combo and spin boxes from SVGs; without these they
        # render blank, so a checkbox cannot be seen or ticked. Only a
        # Recommends of the Qt SVG library, so --no-install-recommends drops
        # them.
        qt6-svg-plugins \
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
RUN --mount=type=bind,from=selkies,target=/tmp/selkies \
    apt-get update \
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
        "/tmp/selkies/selkies_gstreamer-${SELKIES_VERSION}-py3-none-any.whl" \
    # The wheel ships libnvrtc.so.12; GStreamer dlopens the bare SONAME.
    && ln -sf /opt/selkies/lib/python3.13/site-packages/nvidia/cuda_nvrtc/lib/libnvrtc.so.12 \
              /opt/selkies/lib/python3.13/site-packages/nvidia/cuda_nvrtc/lib/libnvrtc.so \
    && apt-get purge -y build-essential python3-dev linux-libc-dev \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

# The web client. Served to the browser by Gleem, not from here — this copy is
# what the version pin is anchored to, so client and server cannot drift.
RUN --mount=type=bind,from=selkies,target=/tmp/selkies \
    mkdir -p /opt/selkies-web \
    && tar -xzf "/tmp/selkies/selkies-gstreamer-web_v${SELKIES_VERSION}.tar.gz" \
        -C /opt/selkies-web --strip-components=1

# Selkies ships neither the Python package nor the web bundle with its licence,
# and MPL-2.0 requires the licence to travel with the code. Every Debian
# package in this image carries its own /usr/share/doc/<pkg>/copyright; these
# two are the only payloads dpkg knows nothing about, so they are the only ones
# that need saying out loud. It lives next to the code it covers in
# vendor/selkies/, so a version bump replaces all three files together and
# cannot leave the licence describing a different release.
RUN --mount=type=bind,from=selkies,target=/tmp/selkies \
    mkdir -p /usr/share/doc/selkies \
    && cp /tmp/selkies/LICENSE /usr/share/doc/selkies/LICENSE \
    && cp /tmp/selkies/LICENSE /opt/selkies-web/LICENSE

COPY LICENSE NOTICE /usr/share/doc/gleem-obs-runtime/

COPY --from=init-build /build/target/release/runtime-init /usr/local/bin/runtime-init

# OBS itself, from the stage above, and the Debian packages its libraries come
# from. obs-websocket has been built in since OBS 28.
COPY --from=obs /out/ /
RUN --mount=type=bind,from=obs,source=/obs-packages,target=/tmp/obs-packages \
    apt-get update \
    && xargs -a /tmp/obs-packages apt-get install -y --no-install-recommends \
    && rm -rf /var/lib/apt/lists/* \
    && ldconfig \
    && ! ldd /usr/bin/obs /usr/lib/x86_64-linux-gnu/obs-plugins/*.so 2>/dev/null | grep 'not found'

COPY --from=irl-control /irl-out/ /

# Everything a rental may write that outlives it goes here, and this is the
# only path bind-mounted from the host's encrypted workspace. OBS keeps its
# configuration under /workspace/config and scenes use media and fonts from
# /workspace/media; runtime-init creates both at start, since the mount hides
# anything created here.
RUN mkdir -p /workspace /run/pulse \
    && chmod 0777 /workspace

# Configuration files, such as fontconfig picking up the workspace's fonts.
COPY rootfs/ /

# Openbox's stock rc.xml carries the key and mouse bindings that make windows
# movable at all, so the Gleem rules are added to it rather than replacing it.
RUN sed -i '/<applications>/r /etc/gleem/openbox-applications.xml' /etc/xdg/openbox/rc.xml \
    && grep -q 'title="OBS \*"' /etc/xdg/openbox/rc.xml

# No setuid or setgid binaries. Nothing in this image runs as one user and
# needs to become another: su, passwd, mount and the rest arrive with the base
# system and would only ever serve a renter poking at the desktop. Stripping
# them is also what lets the agent pull the image at all. Its service unit
# sets RestrictSUIDSGID, so the kernel refuses to create such a file while
# podman unpacks a layer:
#
#   unpacking failed (error: exit status 1; output:
#   open /usr/bin/chage: operation not permitted)
#
# The bits have to go from every layer, not just the last, which is why the
# whole tree is flattened into the final stage below.
RUN find / -xdev -type f -perm /6000 -exec chmod a-s {} +


# The image hosts pull: one layer holding the finished tree, so the base
# image's setuid files never appear in any layer's tar. Metadata does not
# survive a `FROM scratch`, so everything a container needs at runtime is
# declared here rather than in the build stage.
FROM scratch

COPY --from=rootfs / /

ENV DISPLAY=:0 \
    PULSE_SERVER=unix:/run/pulse/native \
    GLEEM_RESOLUTION=1920x1080 \
    GLEEM_FRAMERATE=30 \
    GLEEM_ENCODER=nvh264enc

# Loopback only in practice: the agent publishes this on 127.0.0.1 and nothing
# outside the machine can address it.
EXPOSE 8082

ENTRYPOINT ["/usr/local/bin/runtime-init"]
