Bro's optional Windows remote desktop package, based on Voxl.

Native mobile viewers can use WebRTC H.264 video over UDP, with an ordered input data channel. TURN supports connections that need a relay. The existing WebSocket transport remains available over TCP.

The Windows host's WebRTC TURN connection uses UDP. Networks that block host UDP use the WebSocket transport over TCP.

Bro downloads this package only after remote control is enabled in Settings → Mobile Companion. Paired clients connect through Bro; the package's listener stays on loopback. Viewing and control permissions are enforced separately for each connection.

The executable links x264 and is distributed under GPL-2.0. The ZIP includes license notices, and `bro-rmm-source.zip` contains the corresponding source and build instructions. This preview runs in the logged-in Windows user's session; it cannot control protected UAC or lock screens.
