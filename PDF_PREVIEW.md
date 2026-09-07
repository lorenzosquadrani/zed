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
successfully. All nine PDF tests pass, including a 20-seed sweep of the GPUI
tests. They cover rasterized content and
color order, invalid input, bitmap bounds, visible-page caching, direct-file
workspace opening, navigation, reload recovery, and Ctrl + wheel zoom behavior.
Formatting and launcher syntax checks pass. The included two-page fixture was
also opened and its rendered text inspected in the native Wayland app on this
host during initial local-preview validation.

The remote integration test uses separate client/server fake filesystems and
the actual headless-server request handler. It checks remote loading (even with
a conflicting local filename), automatic reload after atomic file replacement,
error recovery, manual reload,
and rejected oversized, missing, or directory inputs. Bounded local reads are
also tested. A connection to your real SSH host still needs to be verified after
restarting the development editor.

```sh
./script/pdf-dev run crates/pdf_viewer/fixtures/preview.pdf
./script/pdf-dev env ITERATIONS=20 cargo test -p pdf_viewer
```

This is focused feature validation, not a run of Zed's complete test suite or a
PDF compatibility corpus. The GPUI test skill guided deterministic scheduler
and input-event coverage.

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
Parsing and rasterization run on the background executor, with one raster job
in flight per view. Only visible page bitmaps are retained; GPU images are
released when pages leave the viewport, the document changes, or the tab closes.
Output bitmaps are capped at 2048 pixels per side and the visible bitmap budget
is 128 MiB per pane, excluding renderer working memory and GPU copies. High zoom
levels can therefore show reduced sharpness. Input files are limited to 128 MiB.

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
