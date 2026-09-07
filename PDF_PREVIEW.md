# Local PDF preview build

This fork opens local and SSH-remote `.pdf` files in native Zed tabs. It supports continuous
scrolling, previous/next/first/last page navigation, zoom, fit to width, split
panes, restoring PDF tabs on restart, and automatic reload after file changes.

## Run on Fedora Atomic

The development environment is a rootless Fedora Toolbox named `zed-pdf-dev`.
Build dependencies are installed inside that container. Rust and Cargo's
downloads are under `target/local-tools` in this repository.

```sh
./script/pdf-dev build
./script/pdf-dev run /absolute/path/to/document.pdf
```

`run` launches `target/debug/zed` directly, with separate settings, cache, and
application data under `target/pdf-preview`. Your existing `zed` command still
launches your independently installed editor. No host package layering or
replacement of that installation is needed.

```sh
./script/pdf-dev check
./script/pdf-dev test
./script/pdf-dev fmt -- --check
```

The launcher defaults to four compiler jobs and disables development debug
symbols to keep the first build manageable on a 32 GiB machine. Override
`CARGO_BUILD_JOBS` if needed. The first build downloads Zed's full dependency
graph and takes substantially longer than incremental builds.

The Toolbox also includes `fish`, matching this host's login shell. Restart the
development app if a window from before that installation still shows an
environment-loading warning.

## Remote folders

Quit the development Zed and restart it with `./script/pdf-dev run`, then use
Zed's usual SSH connection UI and open a PDF from the remote project panel.
The restart is required after rebuilding the editor or changing launcher
environment variables; existing windows keep running the old binary.

Development builds compile a matching remote server locally and upload it on
connection. The Toolbox now includes `musl-gcc`, and the launcher sets
`CC_x86_64_unknown_linux_musl=musl-gcc`. This fixes the missing
`x86_64-linux-musl-gcc` error without adding host packages. To build the server
for an x86-64 Linux remote explicitly:

```sh
./script/pdf-dev server
./script/pdf-dev server-smoke
```

Its output is `target/remote_server/x86_64-unknown-linux-musl/debug/remote_server`.
This statically linked server avoids depending on the remote host's glibc
version. Other remote architectures may require Zed's additional cross-compiling
tools; they have not been configured by this setup.

The remote server's `debug-embed` feature must enable `util/debug-embed` as well
as `rust-embed`: Zed's `fs_embed!` otherwise looks for a source checkout at
runtime and panics while loading default settings. The feature wiring is fixed
in this fork. `server-smoke` copies the binary outside the checkout, uses
isolated XDG directories, and checks that the server stays running and accepts
connections. The old binary fails this check; the corrected binary passes.
The corrected binary was also checksum-verified and startup-tested on the
Ubuntu 24.04 remote host before replacing the development server. The previous
binary is retained there as `.zed_server/zed-remote-server-dev-build.before-asset-fix`.

Development builds normally upload their server on every connection. To
refresh the remote server with compression, completely quit development Zed
and run:

```sh
./script/pdf-dev env ZED_BUILD_REMOTE_SERVER=1 ./script/pdf-dev run
```

After the current server has been successfully installed, skip rebuilding and
uploading it on subsequent connections with:

```sh
./script/pdf-dev env ZED_BUILD_REMOTE_SERVER=never ./script/pdf-dev run
```

Do not use `never` before installation, or after changing the server/protocol
without uploading a matching build. Toolbox does not forward arbitrary host
environment variables, so these commands set them inside the container.

PDF bytes are fetched through the existing remote connection and rendered on
the client. The server enforces the 128 MiB file limit before reading and also
bounds the read itself. Opening and each reload transfer the complete file;
page rasterization still happens only for visible pages. Remote worktree
notifications trigger the same debounced reload as local files. The remote host
does not need Hayro, PDFium, or a graphical environment.

## Validation

The full Zed development binary and the x86-64 musl remote server build
successfully. All 15 PDF tests pass, including a 20-seed sweep of the GPUI
tests. They cover rasterized content and
color order, invalid input, bitmap bounds, visible-page caching, direct-file
workspace opening, navigation, reload recovery, and Ctrl + wheel zoom behavior.
Regression tests also cover scroll-back image identity, zero raster requests on
zoom-out, coalesced zoom refinement with the old image still visible, stale
reload results, LRU eviction, entry/pixel budgets, capped-resolution reuse,
indexed visibility, and worker shutdown.
Formatting and launcher syntax checks pass. The included two-page fixture was
also opened and its rendered text inspected in the native Wayland app on this
host during initial local-preview validation.

The remote integration test uses separate client/server fake filesystems and
the actual headless-server request handler. It checks remote loading (even with
a conflicting local filename), automatic reload after atomic file replacement,
error recovery, manual reload,
and rejected oversized, missing, or directory inputs. Bounded local reads are
also tested. The user has confirmed that the corrected development build connects
to the real SSH host and previews remote PDFs successfully.

```sh
./script/pdf-dev run crates/pdf_viewer/fixtures/preview.pdf
./script/pdf-dev env ITERATIONS=20 cargo test -p pdf_viewer
```

This is focused feature validation, not a run of Zed's complete test suite or a
PDF compatibility corpus. The GPUI test skill guided deterministic scheduler
and input-event coverage.

## Performance checks

The usable local/remote preview checkpoint is commit `b00d76d7c4` on
`pdf-preview`. Performance changes build on that checkpoint without changing
the remote protocol. The existing remote development server can still be reused
with `ZED_BUILD_REMOTE_SERVER=never`.

The isolated `pdf_viewer_benchmarks` package compiles the production raster code
without GPUI or any `test-support` dependencies. The wrapper checks the feature
graph and bounds each benchmark invocation to five minutes. Build first, then
run smoke, quick, and measured modes:

```sh
./script/pdf-dev cargo bench -p pdf_viewer_benchmarks --bench raster --profile release-fast --no-run
./script/pdf-dev bench --test
./script/pdf-dev bench --quick
./script/pdf-dev bench
```

The fixed generated PDFs contain 1, 100, or 1000 pages with text and vector
plots. Each measurement renders the last page, alternating 100% and 125% zoom.
It compares reconstructing the document/cache on each render (the checkpoint's
strategy) with retaining them (the worker's strategy), using identical raster
code. Before timing it asserts identical pixels and dimensions in both paths.
Fixture construction and those assertions are outside timing.

This is a CPU raster microbenchmark, not a full historical-checkout comparison
or a frame-latency benchmark. It excludes SSH, worker scheduling, BGRA conversion,
GPUI layout, GPU uploads, and display presentation. The deterministic GPUI tests
separately check raster-request counts and image reuse through zoom/scroll/reload.
Neither test scheduler timings nor these raster numbers measure real display
frame rates. Benchmark results are stored locally under `target/criterion`.

For the actual development profile, set `PDF_BENCH_PROFILE=dev` inside Toolbox:

```sh
./script/pdf-dev env PDF_BENCH_PROFILE=dev ./script/pdf-dev bench --quick
```

Set `PDF_BENCH_UNOPTIMIZED=1` alongside `PDF_BENCH_PROFILE=dev` to override the
new dependency optimization levels back to zero for comparison, without editing
the workspace or changing the application build defaults. Use separate Criterion
baseline names when comparing profiles, and do not run measured benchmarks
alongside a build or another benchmark.

Targeted development-profile overrides optimize the PDF rasterizer, font/path
libraries, and viewer code without building all of Zed in release mode. Shared
library changes can require a larger one-time incremental rebuild.

Measured on this Linux host on 2026-09-07, using Rust 1.97.1, the same benchmark
source/lockfile, the `dev` profile, the 100-page fixture, 10 samples, 500 ms warmup,
and a 2 s measurement target (Criterion extended the slow cases to collect ten
samples). The baseline uses `PDF_BENCH_UNOPTIMIZED=1`; the candidate uses the
default dependency overrides. Runs were sequential after the editor build, not
concurrent with compilation. Values are Criterion's reported estimate intervals:

| Raster strategy | Unoptimized dependencies | Optimized dependencies |
| --- | --- | --- |
| Recreate document/cache each render | 631.59–667.07 ms | 22.704–24.079 ms |
| Retain document/cache | 662.51–735.66 ms | 23.004–25.069 ms |

For the old-versus-new strategy this is roughly 647 ms versus 24 ms per raster,
about 96% less time on this fixture. Cache reuse alone did not consistently
reduce raster time here; eliminating redundant raster requests and optimizing
the CPU libraries are the main wins. Earlier quick runs ranged around 300 ms
versus 11 ms, so absolute timings clearly vary with host conditions. This is
not a guarantee for arbitrary PDFs or a measurement of end-to-end UI latency.

The final measured baselines are `unoptimized-dev-full` and
`optimized-dev-full`. The GPUI benchmark skill guided feature isolation and
sequential measurements; the GPUI test skill guided fake-time zoom/reload tests
and 20-seed scheduling checks rather than wall-clock assertions.

## Controls

Open a PDF from the project panel, file finder, or the command line. Use the
toolbar for page navigation, zoom, fit to width, and reload. Keyboard controls
apply while the PDF pane has focus:

| Action | Linux/Windows | macOS |
| --- | --- | --- |
| Zoom in/out | Ctrl + / Ctrl - | Cmd + / Cmd - |
| Actual size | Ctrl 0 | Cmd 0 |
| Fit width | Ctrl Shift 0 | Cmd Shift 0 |
| Next/previous page | Page Down / Page Up | Page Down / Page Up |
| First/last page | Home / End | Home / End |
| Reload | Ctrl R | Cmd R |

Ctrl + mouse wheel also zooms. Ordinary scrolling pans the document. The
`pdf viewer` actions are available in the command palette.

## Scope and rendering

The renderer is [Hayro](https://github.com/LaurenzV/hayro), a Rust PDF rasterizer
with embedded fallback fonts. It does not need a separate PDFium installation.
Each open document has a dedicated background worker that owns its parsed PDF
and Hayro rendering cache. Split panes share this worker. A bounded request
channel and one outstanding request per view limit queued work; the worker does
not block the foreground executor or shared background threads while idle.

Each pane retains recently viewed bitmaps in a least-recently-used cache. Zoom
scales the previous bitmap immediately; higher-resolution refinement waits for
a 100 ms pause in zoom events. Zooming out reuses a higher-resolution image.
Cache matching uses effective pixel dimensions, so zooming beyond the raster
cap does not repeatedly render identical images. Document reloads invalidate
both parsed state and bitmaps, and discard results from the old document.

Output bitmaps are capped at 2048 pixels per side. The retained bitmap budget
is 128 MiB per pane, excluding renderer working memory, GPU copies, and one
in-flight output bitmap. The cache also limits entries to 128, or the visible
page count if larger. Images are released on eviction, reload, or tab closure.
Scrolling back can still need rendering after eviction; this is intentional
to bound memory. High zoom levels can show reduced sharpness. Input files are
limited to 128 MiB. Hayro's parsed structures and font/image caches have no
separate byte budget and are released when their document worker closes.

Page positions are indexed once per document. Visibility and current-page
lookup use binary search, navigation uses indexed offsets, and only visible
page elements are laid out, preserving the full document scroll extent.

This is a visual preview. Text selection, search, annotations, SyncTeX, password
entry, and collaborative-session PDF sharing are not implemented. Hayro is still developing
PDF compatibility; some documents may render differently from established
viewers. Parsing errors and reload failures are shown in the UI.

## Existing efforts reviewed

- [PR #51040, Add PDF viewer](https://github.com/zed-industries/zed/pull/51040):
  Hayro rendering with a proposed text-extraction API. Closed without merging;
  maintainers preferred opening a system viewer. Useful precedent for a
  separate `pdf_viewer` workspace item.
- [PR #51870, lightweight PDF viewer](https://github.com/zed-industries/zed/pull/51870):
  Hayro rendering and a second text backend. Also closed without merging. Its
  implementation eagerly scheduled all pages and used manual unsafe threading
  assertions, so it was not imported.
- [PR #63603, continuous PDF viewer](https://github.com/zed-industries/zed/pull/63603):
  PDFium integration. Closed without merging. Review identified eager page
  rasterization, disconnected caching code, and a synthetic placeholder when
  PDFium was unavailable.
- [Discussion #45819](https://github.com/zed-industries/zed/discussions/45819):
  an earlier PDFium prototype with single, dual, and continuous page modes.

This implementation follows Zed's current image-viewer integration conventions
and uses the released Hayro API. It does not import the closed PRs wholesale.
Upstream acceptance is a separate question; the local fork can be used regardless.
