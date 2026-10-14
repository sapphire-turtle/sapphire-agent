# Voice pipeline testing

> **Note (#278):** the `sapphire-agent-cli voice` satellite this guide drives
> was removed when the CLI merged into the single `sapphire-agent` binary; its
> role moves to wearable device firmware. The server side described here
> (`voice/*` on `/rpc`, the STT/TTS providers) is unchanged and is still what
> `sapphire-agent-desktop` uses. To follow the satellite steps as written, use
> the `sapphire-agent-cli-v0.8.0` tag; read `sapphire-agent-server` there as
> `sapphire-agent serve` against a current server.

Two paths for trying the voice pipeline end-to-end:

1. **Mock smoke test** — fastest. Verifies the MCP wire format, SSE
   streaming, and the `sapphire-agent-cli ↔ sapphire-agent` plumbing without
   downloading any models or starting Irodori-TTS. ~2 minutes.
2. **Real test** — SenseVoice STT + Irodori-TTS. ~30 minutes the first
   time (sherpa-onnx C++ build, model downloads, Irodori-TTS setup).

The mock path is the right first move when something's wrong — it
isolates "is the wiring broken" from "is sherpa/Irodori broken".

---

## 1. Mock smoke test

### One-time setup

You need an Anthropic API key. The LLM step runs for real even in mock
mode; the mocks only stub out STT (constant transcript) and TTS (sine
wave).

```sh
export ANTHROPIC_API_KEY=sk-ant-...
```

### Build

```sh
cargo build --bin sapphire-agent-server --bin sapphire-agent-cli
```

No `--features voice-sherpa` needed — the mock providers are part of
the default build.

### Run

Terminal A (server):

```sh
cargo run --bin sapphire-agent-server -- \
    --config test-configs/voice-mock.toml serve
```

Terminal B (satellite, microphone + speaker required):

```sh
cargo run --bin sapphire-agent-cli -- voice --token "<voice_test api_key>"
```

The first satellite run downloads the Silero VAD ONNX (~2 MB) to
`~/.local/share/sapphire-agent-cli/voice-models/`.

### What to expect

1. Speak any utterance.
2. Server log shows
   `voice/pipeline_run: STT via 'mock_stt' (...samples...)` and
   `STT via 'mock_tts' (...chars)`.
3. Satellite stderr prints `> Say hi briefly.` (the mock STT result)
   followed by the LLM's actual reply text.
4. Speaker plays a 400 ms 440 Hz beep (the mock TTS output).

If you reach step 4 the MCP wire format, the SSE stream, the audio
chunk encoding, and the satellite's playback queue are all working.

---

## 2. Real test: SenseVoice STT + Irodori-TTS

### One-time setup

#### a. sherpa-onnx-enabled agent build

Compiles sherpa-onnx + ONNX runtime through CMake. **~10 minutes
cold**, mostly bottlenecked on cmake/onnxruntime, not Rust.

```sh
cargo build --release --features voice-sherpa --bin sapphire-agent
```

#### b. Satellite build

Compiles sherpa-onnx a second time (workspace member). Same ~10 min
cold on the satellite side — distribute via prebuilt binaries in
production.

```sh
cargo build --release --bin sapphire-agent-cli
```

#### c. Irodori-TTS server

In a separate directory:

```sh
git clone https://github.com/Aratako/Irodori-TTS
cd Irodori-TTS
pip install -r requirements.txt
# Run the Gradio entrypoint — check the repo for the exact command;
# it spawns at http://localhost:7860 with a `generate` endpoint.
python app.py
```

Confirm it's up:

```sh
curl http://localhost:7860/info  # should return JSON describing /generate
```

#### d. Anthropic API key

```sh
export ANTHROPIC_API_KEY=sk-ant-...
```

### Run

Terminal A (server):

```sh
cargo run --release --features voice-sherpa --bin sapphire-agent-server -- \
    --config test-configs/voice-irodori.toml serve
```

First boot auto-downloads the SenseVoice bundle (~230 MB) from
sherpa-onnx GitHub releases to
`~/.local/share/sapphire-agent/voice-models/`.

Terminal B (satellite):

```sh
cargo run --release --bin sapphire-agent-cli -- voice \
    --token "<voice_irodori api_key>" --language ja
```

First boot auto-downloads Silero VAD (~2 MB) to
`~/.local/share/sapphire-agent-cli/voice-models/`.

### What to expect

1. Server prints `voice/pipeline_run: STT via 'sense_voice' ...` after
   the first utterance reaches the pipeline.
2. Satellite prints `> <transcribed Japanese text>` once SenseVoice
   finishes (sub-second on Apple Silicon, longer on CPU-only x86).
3. Server prints `TTS via 'irodori' ...`; satellite plays the synth
   reply through the speaker.

### Tuning knobs

- **VAD too sensitive / not sensitive enough** — edit
  `crates/sapphire-agent-cli/src/voice/mod.rs`, the `silero_config` values
  inside `build_vad()`:
    - `threshold` (default 0.5): lower → more permissive
    - `min_silence_duration` (0.25): higher → longer pause needed to
      close an utterance
- **Irodori payload schema drift** — if the upstream Gradio app
  changes its argument count, edit
  `test-configs/voice-irodori.toml`'s `[tts_provider.irodori].payload`
  template. The user message goes into `{{text}}` (JSON-escaped).

---

## 3. Wake-word mode (optional)

Wake words are AI-name-specific (Saphina, Fina, whatever you've
trained), so pre-built KWS vocabularies don't cover them. The only
realistic engine is **openWakeWord** with a classifier you train
externally and drop on the server. Every connected satellite fetches
the same ONNX via `voice/config` and caches it by SHA-256.

### Server config

```toml
[voice]
wake_word_model = "/srv/sapphire/wake-models/saphina-v1.onnx"
```

That's it — global to all room_profiles. Validation catches a typo
in the path at startup; missing files surface as a clean
`validate_profiles` error, not a 500 on the first satellite call.

### Satellite

No extra CLI flags. The satellite fetches the ONNX at startup along
with the openWakeWord shared frontend models (mel + embedding,
~6 MB total, downloaded once from the openWakeWord v0.5.1 GitHub
release to `~/.local/share/sapphire-agent-cli/voice-models/oww/`):

```sh
cargo run --release --bin sapphire-agent-cli -- voice \
    --token "<voice_irodori api_key>"
```

The display label printed on wake-fire is derived from the ONNX
filename (e.g. `saphina-v1.onnx` → `saphina`).

### Training a custom model

```sh
git clone https://github.com/dscripka/openWakeWord
cd openWakeWord
pip install -e . piper-tts

# Generate synthetic samples + train.
# The README's `Custom Models` notebook walks through it in 30–60 min.
# Output: my_wakeword.onnx (~250 KB).
```

Drop the resulting `.onnx` on the server, point
`[voice].wake_word_model` at it, restart sapphire-agent-server. Each
satellite picks up the new model on its next boot (cache hits
re-use existing models; new SHA = re-download once).

### Limitations

- Threshold is fixed at 0.5 — make it config-tunable when the
  trained model needs more sensitivity / less.
- The Rust port of openWakeWord's streaming feature pipeline is
  faithful to
  the Python source but hasn't been validated against the official
  Python implementation's output yet. False positives / negatives
  may need threshold tuning per model.

---

## 4. Persistent satellite config

So you don't have to type `--server …` every boot, drop a TOML
config at `~/.config/sapphire-agent-cli/config.toml` (XDG location;
override with `--config <path>`). See
`crates/sapphire-agent-cli/config.example.toml` for the schema. Minimum:

```toml
[server]
url   = "https://agent.example.com"
token = "sa-call-<long random>"  # must match an api_keys entry on the agent
```

CLI flags override config-file values; `SAPPHIRE_AGENT_TOKEN` works
too. Wake-word fields are deliberately rejected here — server side is
the source of truth (see §3).

---

## 5. Headless Linux deployment (smart-speaker style)

Running the satellite on a server with just a USB speakerphone (Jabra
Speak, AnkerWork, eMeet, etc.) attached:

### Discover devices

```sh
sapphire-agent-cli voice --list-devices
```

prints every input / output device cpal can see, marks the system
defaults with `*`, and exits. The names are exact strings — copy
them verbatim into the selection flags.

### Pin specific devices

```sh
sapphire-agent-cli voice \
    --input-device  "Jabra SPEAK 510 USB"  \
    --output-device "Jabra SPEAK 510 USB"  \
    --language ja --token "<voice_irodori api_key>"
```

The system "default" rarely points at a USB speakerphone — it
usually picks the internal codec instead.

### Make the service survive logout

If you're running `sapphire-agent-cli voice` as a systemd user service
under a non-login account, the PipeWire / PulseAudio user daemons
shut down when no session is active. Enable lingering so the audio
graph (and the satellite) keep running:

```sh
sudo loginctl enable-linger <user>
```

### Echo / feedback

The satellite gates the mic during reply playback, but in wake-word
mode the mic is open while waiting for the trigger — TTS bleed into
the mic can confuse Silero VAD. If you hear the agent talking to
itself, enable PipeWire's `echo-cancel` module
(`pactl load-module module-echo-cancel`) or physically separate the
mic from the speaker.

## 6. Common gotchas

| Symptom | Cause | Fix |
|---|---|---|
| `no default input device` | macOS: app lacks mic permission | System Settings → Privacy & Security → Microphone |
| Silent reply, no errors | Output device muted or output_rate mismatch in the satellite's log | Check `output: <rate> Hz` line; raise system volume |
| `[error: Internal error: Provider error: ...]` from satellite | Anthropic key absent / invalid on the server side | Verify `ANTHROPIC_API_KEY` env var on the **server** terminal |
| `Failed to read voice_pipeline 'irodori'` validation error at server startup | room_profile.voice_pipeline points at undefined block | Match name with `[voice_pipeline.<n>]` |
| `failed to fetch Silero VAD model` | No network / proxy | Pre-download the model manually and place at `~/.local/share/sapphire-agent-cli/voice-models/silero_vad.onnx` |
| First sherpa-onnx build OOM-kills CI | C++ compile is memory-heavy | Use a runner with ≥4 GB / 2 cores or set `CARGO_BUILD_JOBS=1` |

---

## 7. What's still in unit-test territory

The TOML schema, MCP wire format helpers, VAD math, and resampler are
covered by `cargo test --workspace`. The whole MCP-server-to-cpal
round-trip is **not** unit-tested — the integration boundary is the
manual flow above. Bringing that into automated CI would require
spawning a real axum process plus a mock LLM that talks the
JSON-streaming Anthropic protocol, which is more bookkeeping than it
saves at the project's current size.
