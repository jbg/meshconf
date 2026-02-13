# meshconf

Peer-to-peer video conferencing in the terminal.

`meshconf` is a small group video conferencing tool that runs in your terminal. It uses [iroh](https://iroh.computer/) for peer-to-peer connectivity with built-in encryption, hardware-accelerated [HEVC](https://en.wikipedia.org/wiki/High_Efficiency_Video_Coding) (via macOS VideoToolbox) for video, and [Opus](https://opus-codec.org/) for audio. Video can be rendered directly in a compatible terminal using the [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/), or displayed in a native window (the default).

There is no host/joiner distinction — every participant is equal. Each participant's ticket is displayed below the video gallery, and anyone can share theirs to let new people join. Any participant can leave without disrupting the call. All participants connect directly to each other in a full mesh, making this suitable for small calls.

## Usage

### Starting a call

```
meshconf
```

A ticket string is printed (and displayed in the TUI). Share it with another participant.

### Joining a call

```
meshconf <ticket>
```

Paste a ticket from any participant in the call. Once connected, your own ticket is displayed too — anyone can use any participant's ticket to join.

### Options

| Flag | Description |
|---|---|
| `--test-pattern` | Send animated colour bars instead of the webcam |
| `--kitty` | Display video in the terminal using the Kitty graphics protocol instead of a native window |
| `--relay <url>` | Use a custom relay server URL (e.g. `http://localhost:3340` for local dev); omit to use iroh's default production relays |
| `--no-relay` | Disable relay servers entirely (direct connections only) |

### Quitting

Press `q` or `Ctrl-C`. The call continues for the remaining participants.

## How it works

Every participant creates an iroh endpoint and displays a ticket. When a new participant joins using someone's ticket, they connect and exchange a lightweight JSON control protocol over a bidirectional QUIC stream: the new peer sends a Hello (containing its own address) and receives back a PeerList of everyone currently in the call. The new participant then establishes direct connections to the other peers — the peer with the lexicographically lower public key initiates each connection.

There is no central coordinator. Every participant accepts incoming connections and can serve as the entry point for new joiners. Each peer periodically broadcasts its full peer list to all its control channels (every 5 seconds, plus immediately on any membership change), so all participants converge on the same view of the call even if a peer that introduced two others disconnects before the introduction is complete. Peer disconnections are detected locally via QUIC connection close — no explicit leave messages are needed.

Media is sent over unidirectional QUIC streams using the [MoQ Transport](https://datatracker.ietf.org/doc/draft-ietf-moq-transport/) (SUBGROUP_HEADER) framing with [LOC](https://datatracker.ietf.org/doc/draft-mzanaty-moq-loc/) (Low Overhead Container) object headers. Each audio or video frame opens a fresh stream with a subgroup header and a single object carrying a LOC Capture Timestamp extension (wall-clock microseconds since epoch). Audio is prioritised over video at both the QUIC and application layers.

## Requirements

- **macOS** (audio uses AVAudioEngine, video uses VideoToolbox — both are macOS-only)
- A terminal supporting the **Kitty graphics protocol** for inline video (e.g. Kitty, Ghostty, WezTerm) when using `--kitty`, otherwise video is displayed in a native window
- A webcam and microphone (or use `--test-pattern` for video)

## Building

```
cargo build --release
```

## Limitations

- Currently macOS-only
- Uses the default camera and default audio input device — there is no device selection
- The full-mesh topology means every participant sends to every other participant, so it is practical only for small groups
- Video uses adaptive bitrate with a maximum of 640×480 @ 15 fps / 500 kbps

## License

MIT OR Apache-2.0
