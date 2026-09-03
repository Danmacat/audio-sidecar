# mediaremote-adapter packaging assets

macOS media-session support reads the system `MediaRemote` private framework,
which Apple restricts to Apple-signed platform binaries since macOS 15.4
(PORTING.md §4b, probe #2). These assets are the community workaround
([ungive/mediaremote-adapter](https://github.com/ungive/mediaremote-adapter),
BSD-3-Clause, see `LICENSE`):

- `mediaremote-adapter.pl` — driver script; the sidecar spawns
  `/usr/bin/perl <script> <framework> stream --micros` and consumes its
  NDJSON frames.
- `MediaRemoteAdapter.framework/` — the helper framework the perl script
  loads inside the Apple-entitled perl process. Ad-hoc signed, arm64.

At runtime the sidecar looks for this pair at (in order):

1. `$AUDIO_SIDECAR_MEDIAREMOTE_DIR`
2. `<sidecar binary dir>/mediaremote-adapter/`
3. `<sidecar binary dir>/`

`cargo build` copies this folder next to the binary automatically (see
`build.rs`). Packagers must ship the folder next to the sidecar binary the
same way. If the assets are missing, `hello.capabilities.mediaSessions` and
`mediaArtwork` report `false` and media RPCs return errors — capture is
unaffected.

## Rebuilding the framework

From a checkout of <https://github.com/ungive/mediaremote-adapter>:

```sh
cmake -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build
cp -R build/MediaRemoteAdapter.framework <this directory>/
```

Apple may break this trick in any macOS release; retest with
`/usr/bin/perl mediaremote-adapter.pl <abs framework path> get` (expected:
`null` or a JSON object; `Failed to load framework` means the route is dead).
