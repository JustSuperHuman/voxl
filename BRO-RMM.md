# Bro remote desktop

This fork of [Voxl](https://github.com/tembo/voxl) is Bro's optional remote desktop package. Bro downloads it when you enable remote control in **Settings → Mobile Companion**. The package has its own source and release cycle; Bro's desktop download contains no Voxl binary.

The Windows host captures the selected display with DXGI and encodes H.264. Native mobile viewers use WebRTC video over UDP and an ordered data channel for input. TURN can relay the connection when direct ICE candidates fail. Voxl's existing WebSocket video and input protocol remains available over TCP through Bro's authenticated bridge.

The host's WebRTC connection to TURN uses UDP. A native viewer can use TURN over TCP or TLS on its own connection. When the host cannot use UDP, the mobile viewer uses the WebSocket connection over TCP.

The host runs in your Windows login session. It can control ordinary desktop applications, but Windows protects the lock screen and elevated UAC prompts from an ordinary user process. It does not install a system service.

Windows streams fit within 2560×1440 by default. A 5120×1440 ultrawide display streams at 2560×720, so mobile hardware decoders can open it. This leaves the computer's display mode unchanged. Pointer input, damage and cursor position use the scaled stream's coordinates. `--max-capture-width`, `--max-capture-height` and `--max-capture-pixels` set the limits; zero disables an individual limit.

## Host contract, version 1

Start `voxl.exe` on loopback with an unpredictable `VOXL_PASSWORD`. Keep `--stripes=1` for native WebRTC. Bro authenticates requests to the package with `Authorization: Bearer`; paired mobile clients authenticate to Bro, which holds the package password.

`POST /rtc/offer` accepts a fully gathered SDP offer:

```json
{"type":"offer","sdp":"…","iceServers":[],"readOnly":false}
```

It returns `{"type":"answer","sdp":"…","id":"…"}` with gathered ICE candidates. `iceServers` accepts the browser's `urls`, `username`, and `credential` fields; URLs can be a string or an array. Gathering has a 12-second limit. `DELETE /rtc/{id}` closes the peer. `GET /rtc` lists active peers as `{"peers":[{"id":"…","state":"connected","readOnly":false}]}` so the host can release a dead viewer's lease. Four peers may be active at once. A peer closes after 35 seconds without control messages or ping replies.

Create one receive-only video transceiver and an ordered data channel named `control` before generating the offer. The channel uses Voxl's text commands (`web/src/protocol.ts`); send `v,1` to start video and answer each `{"t":"ping","s":123}` with `po,123`. `hello` includes the display size and `readOnly`. Windows also sends `{"t":"pointer","x":10,"y":20,"visible":true}` when the system pointer moves. Clipboard text and small cursor bitmaps use the existing protocol.

`readOnly` is enforced for each WebRTC peer. TCP viewers can use `/ws?readOnly=true`. A read-only viewer cannot inject input or write the clipboard. Bro reserves control separately and overwrites these flags at its gateway.

Video queues are bounded. Overflow requests a new H.264 keyframe. WebRTC picture-loss feedback also requests a keyframe, and receiver-report loss feeds the encoder's bitrate controller. Video capture and encoding sleep when no client requests video.

`POST /shutdown` stops new peers and input, returns `{"ok":true}`, and releases held keys/buttons on the native input thread before the process exits. It gives the HTTP response at least 150 ms to flush. Bro uses this endpoint before terminating a managed host.

## Windows builds and releases

Build dependencies are Rust, LLVM, esbuild and vcpkg's `x264`, `libyuv`, and `opus` packages using `x64-windows-static-md`. Run `packaging/build-windows.ps1` with `VCPKG_ROOT` set. `packaging/package.ps1` creates the package ZIP, its SHA-256 checksum and a versioned manifest.

The `Bro RMM Windows preview` workflow tests and publishes a new version, then signs its package catalog with the repository's `BRO_RMM_SIGNING_KEY` secret. Bro verifies the Ed25519 signature before accepting the catalog from `bro-rmm-channel-v1/catalog.json`. Versioned package assets remain immutable; updating the signed catalog lets this package ship fixes without a Bro release. `packaging/create-signing-key.mjs` creates a protected local key for a new channel; keep that private key outside this repository.

The Rust source retains Voxl's MIT license. The executable links x264 and is distributed under GPL-2.0; the release includes the applicable notices and a separate corresponding-source archive. Build instructions and dependency versions travel with that archive. Bro communicates with this executable as a separate process.

For a live Windows transport check, run `cargo run --example transport-probe -- http://127.0.0.1:6901` with `VOXL_PASSWORD` set to the host's password. It verifies authenticated TCP video, native WebRTC H.264 delivery, and peer teardown.
