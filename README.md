# Gleem OBS Runtime

The container a rental runs in: a headless X11 desktop with OBS on it,
streamed to the renter's browser over WebRTC with hardware H.264 encoding.

One rental, one container. It is started by the machine agent, given the GPU
through CDI, attached to a network that cannot reach the host's LAN, and
handed a `/workspace` bind-mounted from an encrypted volume whose key the
machine never stores.

## What is inside

| | |
|---|---|
| Display | `Xvfb` at the rental's resolution, `openbox` so OBS's dialogs behave |
| Audio | PulseAudio null sink — OBS refuses to configure audio without one |
| Application | OBS Studio, with `obs-websocket` on `127.0.0.1:4455` |
| Streaming | [Selkies](https://github.com/selkies-project/selkies) v1.6.2 on the distribution's GStreamer 1.26, `nvh264enc` |
| PID 1 | `runtime-init`, a small Rust binary in `init/` |

**Debian trixie, and the base was chosen by measurement.** Two constraints
have to hold at once:

* GStreamer must be new enough that `nvh264enc` — the element Selkies uses for
  NVIDIA — can open an encode session against driver 610. On 1.24, the newest
  Ubuntu 24.04 offers and what Selkies' own bundle ships, it fails with "Could
  not configure supporting library".
* Python must be old enough for Selkies, which calls
  `asyncio.get_event_loop()` with no running loop. That raises on 3.14.

Fedora 44 gives GStreamer 1.28 but Python 3.14. Ubuntu 24.04 gives Python 3.12
but GStreamer 1.24. Trixie gives 1.26 and 3.13, and both were verified on the
reference RTX 3060. Using the distribution's GStreamer also drops the 95 MB
pinned bundle Selkies would otherwise need.

Nothing NVIDIA is baked in. The driver's userspace is injected at runtime by
`nvidia-container-toolkit` through CDI, which is what keeps this image freely
redistributable and lets one image work across driver versions.

`runtime-init` exists so the image needs neither supervisord nor a Python
runtime to sequence five processes. It waits for the X server to actually
accept connections rather than sleeping a guess, reaps the orphans that PID 1
inherits, and — deliberately — **exits if any component dies**. A rental with
a dead streamer is not a degraded rental, it is a black screen the renter is
being charged for; failing loudly lets the agent report it.

## Licence

Apache-2.0. See `LICENSE`, and `NOTICE` for the attributions it requires.

That covers Gleem's own contributions — the Containerfile and `runtime-init`.
The programs the image assembles keep their own licences; the image is an
aggregation of separately licensed software, not a combined work, and none of
their copyleft reaches this repository.

## Third-party code and licensing

Read this before shipping the image or vendoring the client.

**Selkies is MPL-2.0**, not Apache-2.0. Mozilla Public License 2.0 is
file-level copyleft: the rest of Gleem is unaffected, but any modification to
a Selkies file has to stay MPL-2.0 and be made available. Vendoring the files
unmodified, with their headers intact, keeps that obligation trivial. Modifying
them in place does not.

**Selkies' `_gpl_` GStreamer bundle is no longer used.** The image takes
GStreamer from Debian instead, which avoids that bundle's GPL obligations.
Debian's `gstreamer1.0-plugins-bad` still carries its own licence mix — check
it before publishing the image, but it is a smaller and better-documented set
than the vendored bundle was.

**`guacamole-keyboard-selkies.js`** in the web client is Apache-2.0, from
Apache Guacamole, carrying a noted local modification.

**OBS Studio is GPL-2.0**, but it is executed rather than linked, which is the
ordinary aggregation case for a container image.

None of this blocks anything, and as of now none of it is outstanding: the
answer is written down in `NOTICE`, which ships inside the image at
`/usr/share/doc/gleem-obs-runtime/NOTICE`.

What was actually missing was smaller and more specific than the list above
suggests. Every Debian package in the image already carries its own
`/usr/share/doc/<package>/copyright` — 573 of them, obs-studio included — so
the GPL and LGPL licences travel with the image without anyone doing anything.
The two payloads dpkg knows nothing about, `/opt/selkies` and
`/opt/selkies-web`, shipped with **no licence file at all**, which MPL-2.0
does not allow. Both now carry it, fetched at the pinned `SELKIES_VERSION` so
a bump cannot leave the licence describing a different release.

Source for the GPL and LGPL components is Debian's, unmodified; `NOTICE`
records that and carries the written offer.

## Building

```sh
podman build -t localhost/gleem-obs-runtime:dev -f Containerfile .
```

The Selkies version is pinned by `ARG SELKIES_VERSION`. Its signalling shape
moves between releases, so bumping it is a deliberate, tested action — the
agent relays those messages verbatim and will happily forward a protocol the
browser no longer understands.

## Running it by hand

```sh
podman run --rm -it \
  --device nvidia.com/gpu=all \
  -p 127.0.0.1:8082:8082 \
  -e GLEEM_RESOLUTION=1920x1080 -e GLEEM_FRAMERATE=30 \
  localhost/gleem-obs-runtime:dev
```

Without `--device nvidia.com/gpu=all` the pipeline has no encoder and the
container will fail on startup rather than silently falling back to software
encoding — which would cook a host's CPU and look, to the renter, exactly like
it was working.
