# Selkies 1.6.2, vendored

Three files, all MPL-2.0 and all unmodified:

| File | SHA-256 |
|---|---|
| `selkies_gstreamer-1.6.2-py3-none-any.whl` | `682e88b9f463bb48826b855ebf9ba25822b83385ea41cbff1710a1190d31b185` |
| `selkies-gstreamer-web_v1.6.2.tar.gz` | `117ed26db23f8db7c6f66d840ed11e1c1d3bfbf57c1b430391663600241e82f2` |
| `LICENSE` | Mozilla Public License 2.0, as shipped by upstream |

## Why they are here

The Containerfile used to download the wheel and the web bundle from the
`v1.6.2` GitHub release of `selkies-project/selkies`. On 2026-09-23 upstream
published 2.0.0 and deleted every 1.x release and every 1.x tag. The files
are not on PyPI and not in the Wayback Machine. Bumping to 2.0.0 is not a
drop-in change: the signalling protocol between Selkies and the browser moves
between releases, and the agent relays it verbatim.

## Where they came from

Recovered on 2026-09-28 from `localhost/gleem-obs-runtime:licencetest`, an
image built from this repository on 2026-09-12 while the upstream release
still existed. Its Selkies package and web bundle hash identically to the
2026-09-08 `dev` build the runtime was verified with on the reference
RTX 3060.

* **Wheel.** Rebuilt with `python -m wheel pack` from the installed
  `selkies_gstreamer/` package and its `.dist-info` (`METADATA`, `WHEEL`,
  `entry_points.txt`, `top_level.txt`), after dropping pip's installer
  bookkeeping (`INSTALLER`, `REQUESTED`, `direct_url.json`, `RECORD`) and
  `__pycache__`. The archive is therefore not byte-identical to upstream's
  wheel, but every file inside it is; the package is pure Python and carries
  its MPL headers.
* **Web bundle.** `/opt/selkies-web` from the same image, re-tarred with a
  single top-level directory the way upstream's tarball was laid out. The
  `LICENSE` copied into it by the Containerfile was excluded so the tarball
  holds only upstream's files.
* **LICENSE.** The file the Containerfile fetched from upstream's `v1.6.2`
  tag at build time.

## Bumping

Replace all three files together and change `ARG SELKIES_VERSION` in the
Containerfile, then re-verify NVENC encoding and the browser session end to
end before publishing.
