# ChatGPT Voice MCP

Standalone Linux MCP server for emotional narration through the signed-in
ChatGPT Voice realtime session. It uses the ChatGPT subscription session through
Codex's authenticated app-server/WebRTC protocol and can capture assistant
playout into a WAV without sending it to the default speakers or microphone.

The repository is deliberately separate from `codex-desktop-linux`. It owns its
login directory, its MCP process, and its PipeWire session state.

## What this is and is not

- It uses ChatGPT subscription Voice limits, not an OpenAI API key.
- It does not use CDP or the local Read Aloud/Kokoro backend.
- The official signed `codex` app-server and matching `codex-voice-host` are
  still required for the internal Voice transport. The MCP does not pretend
  that this private protocol is a stable public SDK.
- `video_path` contributes local metadata and a nearby subtitle/text sidecar;
  raw video frames are not uploaded to the realtime model automatically.
- Voice can improvise wording even when the prompt asks for verbatim delivery.
  The returned transcript is the model's observed speech, not a proof of exact
  script fidelity.

## Architecture

```text
MCP client
   │ stdio JSON-RPC
   ▼
chatgpt-voice-mcp
   ├── private CODEX_HOME
   │     └── auth.json (created by `login`, mode 0600)
   ├── signed codex app-server --stdio
   │     └── chatgpt.com/backend-api/codex
   ├── matching codex-voice-host
   │     └── WebRTC SDP + realtime audio
   └── optional recording route
         ├── PipeWire module-null-sink, owned by this request
         ├── CPAL playback and capture both target that node
         └── pw-record targets the numeric sink node
```

The login process is intentionally separate from the MCP stdio process. Run
`chatgpt-voice-mcp login` in a terminal; it starts the official Codex device
login with an isolated `CODEX_HOME` and never accepts a token as an MCP
argument. `voice_auth_status` reports only metadata and never returns the
credential contents.

## Build and login

```bash
cargo build --release
./target/release/chatgpt-voice-mcp login
./target/release/chatgpt-voice-mcp auth-status
./target/release/chatgpt-voice-mcp doctor
```

Tagged releases publish a Linux x86_64 archive with a SHA-256 sidecar. Download
the latest archive from [GitHub Releases](https://github.com/AntonIXO/chatgpt-voice-mcp/releases), verify it, and put the binary on your `PATH`:

```bash
tar -xzf chatgpt-voice-mcp-<version>-linux-x86_64.tar.gz
sha256sum -c chatgpt-voice-mcp-<version>-linux-x86_64.tar.gz.sha256
install -Dm755 chatgpt-voice-mcp-<version>-linux-x86_64/chatgpt-voice-mcp ~/.local/bin/chatgpt-voice-mcp
```

Pushing a tag matching `v*` runs the release workflow. It repeats formatting,
tests, Clippy, and a locked release build before publishing the archive.

By default credentials are stored under:

```text
$XDG_DATA_HOME/chatgpt-voice-mcp/codex-home/auth.json
```

When `XDG_DATA_HOME` is unset, the fallback is
`~/.local/share/chatgpt-voice-mcp/codex-home`. Override the location explicitly
with `CHATGPT_VOICE_CODEX_HOME`. The directory is mode `0700`; `auth.json` is
required to be private (`0600` on Unix).

The signed runtime can be supplied in either of two ways:

1. Put a matching release under
   `$CHATGPT_VOICE_CODEX_HOME/packages/app-server-daemon/releases/<release>/`
   with `bin/codex` and `codex-resources/voice/bin/codex-voice-host`.
2. Set both `CODEX_CHATGPT_VOICE_CODEX` and `CODEX_CHATGPT_VOICE_HOST` to an
   already validated matching pair. This does not copy or read credentials from
   another Codex home.

For device login, set `CHATGPT_VOICE_CODEX_LOGIN` if the login executable is
not available as `codex` on `PATH`.

## MCP configuration

```json
{
  "mcpServers": {
    "chatgpt-voice": {
      "command": "/absolute/path/chatgpt-voice-mcp/target/release/chatgpt-voice-mcp",
      "args": ["mcp"]
    }
  }
}
```

Tools:

- `voice_auth_status` checks the standalone credential store without exposing
  secrets.
- `voice_doctor` checks credentials, the signed binary pair, `pactl`, and
  `pw-record` without starting a call.
- `voice_narrate` accepts `text`, `emotion`, `voice`, optional `video_path`,
  `output_path`, and `timeout_seconds`.

## Recording route

When `output_path` is supplied, the server:

1. creates a request-owned PipeWire null sink;
2. obtains its numeric node ID with `pactl`;
3. sets `PIPEWIRE_NODE` for both CPAL directions so the helper does not open a
   physical microphone or default speaker;
4. starts `pw-record --target <numeric-node-id>` against that sink;
5. stops the realtime session before stopping the recorder;
6. removes the sink and temporary ALSA configuration even on normal cleanup.

It never calls `set-default-sink` and never creates a loopback. The server also
fails closed when the helper's speaker meter is zero, so an empty WAV is not
reported as success.

## Failure modes and lessons learned

### A sink name is not a recorder target

The first implementation passed `codex_voice_capture_<pid>.monitor` to
`pw-record`. On this PipeWire/PulseAudio compatibility layer that name was not
resolved; the recorder silently attached to the default microphone. The fix is
to resolve the sink's numeric node ID first and pass that ID to `pw-record`.

### A matching helper pair matters

The app-server and `codex-voice-host` exchange an exact build commit during the
framed handshake. Mixing the desktop app's embedded CLI with a newer helper can
produce transcript events while dropping the incoming audio track. Discovery
therefore accepts only an explicit pair or one release directory containing both
files.

### Opening devices is not the same as enabling audio

The signed helper opens local streams muted and with playout suppressed. The
MCP must send the control transition that unmutes the speaker direction after
transport and devices are ready. The speaker peak is checked before success.

### Muting a microphone is weaker than isolating it

The helper still opens a CPAL capture stream. Routing both CPAL directions to
the request-owned null sink keeps that stream off the physical source, and the
runtime control keeps microphone upload muted.

### Logical output audio is not guaranteed by the pinned WebRTC build

The current Codex protocol models `thread/realtime/outputAudio/delta`, but the
signed WebRTC runtime used for development did not emit those notifications to
this client. PipeWire capture is therefore the compatibility backend. A future
helper fork can add a bounded post-decoder remote-track tap and make that the
primary recording path without changing the MCP surface.

### Credentials must not be smuggled through MCP arguments

The login command delegates the official device flow to Codex and stores the
result in a private, MCP-owned home. Tokens are never accepted in tool
arguments, printed in status output, or copied from the user's normal Codex
home unless the user explicitly points `CHATGPT_VOICE_CODEX_HOME` there.

## Verification

```bash
cargo fmt --all -- --check
cargo test
cargo build --release
./target/release/chatgpt-voice-mcp auth-status
./target/release/chatgpt-voice-mcp doctor
```

The live validation used the real signed Voice session and checked:

- non-zero helper speaker peak;
- a non-silent 48 kHz stereo 16-bit WAV;
- recorder and helper source-outputs targeting the private sink node;
- no physical sink-inputs or source-outputs after cleanup;
- unchanged default PipeWire sink.

## License

MIT. See [LICENSE](LICENSE).
