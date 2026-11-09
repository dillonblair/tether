# Roadmap to feature parity

Done
- [x] Host agent: screen, input, clipboard, audio, file transfer (voxl)
- [x] Accounts, pairing codes, computer list, online state
- [x] Connect from anywhere through the relay (no port forwarding)
- [x] Web viewer over the tunnel

Fundamentals still open
- [ ] WebRTC transport: P2P UDP, STUN/TURN, relay fallback, end-to-end encryption
- [ ] Hardware encode (VideoToolbox on macOS, Media Foundation on Windows), HEVC, 4:4:4
- [ ] Agent as a login item / launchd service with a menu bar UI and signed installer
- [ ] Windows host installer and service (login screen, UAC prompts)
- [ ] Multi-monitor: choose and switch displays, match viewer resolution
- [ ] Unattended access controls: per-device password, 2FA, session notifications

Clients
- [ ] iOS/iPadOS: gestures, trackpad and mouse, hardware keyboard and shortcuts map, Pencil, external display
- [ ] Android: gestures, overlay keypad, hardware keyboard and mouse
- [ ] Web: installable PWA, touch gestures, keyboard capture, file panel polish
- [ ] macOS/Windows desktop viewers

Parity extras
- [ ] RDP client (NLA/TLS, RD Gateway, audio, drive and printer redirection, dynamic resolution)
- [ ] VNC client (SSH tunnelling, TLS, colour depth modes)
- [ ] Collaborative sessions with a cursor per viewer
- [ ] One-off remote support by shared link
- [ ] Teams: shared computers, roles, audit log, SSO
- [ ] Surround audio, microphone passthrough, device selection
- [ ] Connection sync across devices, live previews, wake-on-LAN
