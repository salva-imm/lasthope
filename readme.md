# lasthope

A personal voice companion written in Rust. You speak, she listens, thinks and answers aloud.
Hearing and speaking run locally; only the language model call goes to the cloud.

> Work in progress. Phases 1-2 of 5 are done: the foundation, her character and
> conversation. Persistent memory, the focus timer's voice and the web UI come next.

## How it works

| Part | What | Where |
|---|---|---|
| Ears | Silero VAD waits for you to finish a sentence, Whisper small.en transcribes it | local, [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) |
| Mind | Any OpenAI-compatible chat API (Groq, Cerebras, OpenAI, ollama, …) | cloud (or local server) |
| Voice | [loqui-g2p](https://crates.io/crates/loqui-g2p) turns text into phonemes, Kokoro speaks them | local, sherpa-onnx |
| Audio I/O | Microphone and speaker | [rodio](https://crates.io/crates/rodio) |

sherpa-onnx ships prebuilt static libraries (onnxruntime included), so there is no cmake,
no C++ toolchain setup and no dylib to manage: `cargo build` is enough.

The microphone is only open while she is listening. It is closed while she thinks or
speaks, so she never hears herself.

### Code layout

```
src/
  main.rs        startup, --text flag, wiring
  config.rs      settings from env / .env
  core_loop.rs   the state machine: Inputs and Commands in, Events out
  audio_in.rs    mic -> VAD -> Whisper (and the mic gate)
  speech_out.rs  text -> phonemes -> Kokoro -> speaker, sentence chunking
  llm.rs         OpenAI-compatible client
  persona.rs     loads persona.md, builds each request, cleans replies
  memory.rs      session transcript and recent history (SQLite in phase 3)
  timer.rs       the vigil (focus timer)
  text_io.rs     stdin/stdout mode, slash-commands, event printer
```

The core owns all state. Anything that drives it (voice, `--text`, later the web UI)
sends `Command`s and watches `Event`s:

- Commands: `mute`, `stop_speaking`, `sleep`, `end_session`, `timer_start`, `timer_pause`, `timer_reset`
- Events: `state_changed`, `user_spoke`, `she_spoke`, `timer_tick`, `memory_changed`, `notice`, `error`, `session_ended`

## Setup

1. **Rust** 1.87 or newer.
2. **Speech models** (~450 MB download):
   ```bash
   ./scripts/fetch-models.sh
   ```
   This puts Kokoro in `./kokoro-multi-lang-v1_0` and Whisper + VAD in `./models` (both gitignored).
3. **LLM settings**: copy `.env.example` to `.env` and fill in the three values.
   ```bash
   LLM_BASE_URL=https://api.groq.com/openai/v1
   LLM_API_KEY=...
   LLM_MODEL=...        # check your provider's current model list
   ```
   Real environment variables override `.env`. To switch providers, change these three.
   Optional: `LLM_TEMPERATURE` (default 0.8), `PERSONA_PATH` (default `./persona.md`).

## Run

```bash
cargo run --release            # voice: speak into the mic
cargo run --release -- --text  # type instead: stdin replaces the mic, stdout the speaker
```

`--text` loads no speech models, so it starts instantly and works without audio hardware.
Typed lines wait until she has finished replying, so you can also pipe a script:

```bash
printf 'What is the great work?\nAnd what is a vigil?\n' | cargo run --release -- --text
```

### Talking to her

Just talk. Everything you say goes to her, with the recent conversation
(the last 16 lines, up to about 4000 characters) so she follows the thread.
Two spoken phrases are commands instead:

- **"close the rite"** or **"end the session"** (on its own): ends the session.
- **"please say …"** (or "please generate …", "please store …"): the rest is
  spoken into `output.wav` instead of being answered (voice mode only).

### Her character

She lives in [`persona.md`](persona.md): a calm, dark, commanding mentor who
speaks in the language of ritual (the great work, oaths, vigils, seals) and
pushes you toward your work, the gym and other people rather than toward
talking to her. The file is re-read on every reply, so edits apply
immediately. HTML comments in it are notes and are not sent.

Whatever the model returns, the code keeps her spoken: reasoning blocks,
markdown and list markup are stripped, and a reply is cut to four sentences.

### Commands

Type these in the terminal, in either mode:

```
/mute  /unmute               deafen her (closes the mic) / let her hear again
/stop                        silence her: cut off the current reply
/sleep /wake                 pause / resume listening
/timer start|pause|reset     the vigil (25 min)
/end                         end the session (also Ctrl-C)
```

## Tests

```bash
cargo test --release                    # unit tests
cargo test --release -- --ignored       # + Kokoro -> Whisper round trip (needs the models)
```
