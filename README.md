# Tether

Remote desktop you host yourself: control a computer from a browser anywhere, with no
router setup. Working name; an independent project, not affiliated with any other
remote desktop product.

## Pieces

| Piece | What it does | Status |
| --- | --- | --- |
| `agent/` (`tether-agent`) | Runs on the computer to control. Streams the screen with [voxl](https://github.com/tembo/voxl) (H.264, input, clipboard, audio, file transfer) on loopback only and dials out to the relay. | Working (macOS, Windows, Linux/X11 via voxl) |
| `relay/` (`tether-relay`) | Accounts, computer list, pairing codes, and the tunnel between viewers and agents. Serves the web portal and viewer. | Working |
| WebRTC transport | Peer-to-peer UDP video with the relay as fallback, end-to-end encrypted. | Next |
| iOS app | Native viewer (VideoToolbox decode, touch/trackpad/keyboard). | Planned |
| Android app | Native viewer (MediaCodec). | Planned |
| Web app | The portal and viewer here, extended to parity. | Started |

## Run the relay

```sh
cargo build --release -p tether-relay
TETHER_ADDR=0.0.0.0:8080 TETHER_DB=tether.db TETHER_PUBLIC_URL=https://tether.example.com \
  ./target/release/tether-relay
```

Put it behind TLS (for example Caddy: `tether.example.com { reverse_proxy localhost:8080 }`).
The viewer needs HTTPS (or localhost) because browsers only decode video with WebCodecs
on secure pages.

## Add a Mac

```sh
brew install x264 opus pkgconf esbuild
cargo build --release -p tether-agent
```

Open the portal, create an account, press **Get code**, then on the Mac:

```sh
./target/release/tether-agent enroll --relay https://tether.example.com --code <CODE>
./target/release/tether-agent run
```

macOS asks for Screen Recording and Accessibility permission for the agent (or the
terminal that starts it) the first time. Flags after `run` go to voxl, e.g.
`--framerate=60 --max-kbps=20000 --audio-enabled=false`.

## How a connection works

1. The agent keeps a WebSocket to the relay, authenticated with its device token.
2. A signed-in viewer opens `/d/<device>/`. The relay checks the viewer owns the device.
3. The relay asks the agent for a tunnel connection; the agent dials back and pipes it to
   voxl on `127.0.0.1`. The relay forwards the request, WebSocket upgrades included.

## Security notes

- Passwords are Argon2id; session, device and pairing tokens are stored as SHA-256 hashes.
- The agent's streaming server listens on loopback with a password made up each run.
- In this version the relay can see the stream (TLS ends there). End-to-end encryption
  arrives with the WebRTC transport.
- Not done yet: 2FA, email verification, per-device subdomains, rate limits beyond login.

## Licence

MIT for this code. The agent links x264 through voxl, which is GPL: binaries you
distribute that include it fall under the GPL until the encoder is swapped for a
hardware one (VideoToolbox / Media Foundation), which is on the roadmap.
