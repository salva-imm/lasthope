# lasthope

A personal voice companion written in Rust. You speak, she listens, thinks and answers aloud.
Hearing and speaking run locally; only the language model call goes to the cloud.

> Work in progress. Phases 1-3 of 5 are done: the foundation, her character and
> conversation, memory and sessions. The focus timer's voice and the web UI come next.

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

Your turn ends after 1.2 s of silence (`END_OF_TURN_MS`); shorter thinking pauses stay
in the same turn, and the whole turn is transcribed at once.

**Bluetooth headphones:** while a Bluetooth headset's own mic is open, macOS switches it
to phone-call quality (16 kHz, both directions), which makes her sound like a phone call,
causes dropouts each time the mic opens or closes, and hurts recognition. So by default
she listens through another mic (e.g. the MacBook's) when the default mic belongs to your
headphones. `lasthope --mics` shows the choice; `MIC_DEVICE` overrides it.

### Code layout

```
src/
  main.rs        startup, --text flag, wiring
  config.rs      settings from env / .env
  core_loop.rs   the state machine: Inputs and Commands in, Events out
  audio_in.rs    mic -> VAD -> Whisper (and the mic gate)
  speech_out.rs  text -> phonemes -> Kokoro -> speaker, sentence chunking
  llm.rs         OpenAI-compatible client
  persona.rs     loads persona.md, builds each request and its memory context, cleans replies
  rite.rs        the shape of a session: greeting, oath verdict, oath, close (pure state machine)
  streak.rs      days and the chain of seals (pure logic)
  memory.rs      SQLite: sessions, messages (+ full-text search), facts, mission, streak
  timer.rs       the vigil (focus timer)
  text_io.rs     stdin/stdout mode, slash-commands, event printer
```

The core owns all state. Anything that drives it (voice, `--text`, later the web UI)
sends `Command`s and watches `Event`s:

- Commands: `mute`, `stop_speaking`, `sleep`, `end_session`, `oath_verdict`, `set_mission`,
  `forget_fact`, `timer_start`, `timer_pause`, `timer_reset`
- Events: `session_started`, `state_changed`, `user_spoke`, `she_spoke`, `timer_tick`,
  `memory_changed`, `notice`, `error`, `session_ended`

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
   Optional: `LLM_TEMPERATURE` (default 0.8), `PERSONA_PATH` (default `./persona.md`),
   `MIC_DEVICE`, `END_OF_TURN_MS` (see `.env.example`).

## Run

```bash
cargo run --release            # voice: speak into the mic
cargo run --release -- --text  # type instead: stdin replaces the mic, stdout the speaker
cargo run --release -- --mics  # list microphones and show which one she will use
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

### Memory and the rite

Everything she knows lives in `lasthope.db`, a local SQLite file (gitignored, `DB_PATH` to move it).
The Rust code owns it and writes what is relevant into every request; the model is never trusted
to remember on its own.

- **sessions**: start, end, summary, the oath sworn, its verdict, focused minutes
- **messages**: every line, saved the moment it is said (a crash loses nothing), with FTS5
  full-text search; lines from past sessions that match what you just said are recalled into the prompt
- **facts**: what she has learned about you, each deletable
- **state**: the great work (your mission) and the chain of seals (the streak)

A session follows the rite:

1. **She speaks first**: greets you using memory, asks what you did since last time and, if an oath
   is due, whether you kept it or broke it.
2. **Your verdict** ("kept" / "broken", or `/kept` `/broken`) is recorded, and the chain of seals is
   updated from that answer and real dates only: consecutive kept days grow it, a broken oath or a
   missed day resets it. Days run 4 am to 4 am, so a late-night session belongs to the evening before.
3. You talk.
4. **"Close the rite"**: she asks for tomorrow's oath, you swear it, she seals it. Saying it again
   closes without an oath. Ctrl-C closes at once.
5. **On close**, one LLM call writes a short summary and extracts new durable facts.

A session left open by a crash is sealed (and summarized) on the next start; a summary that
failed (e.g. rate limit) is retried then too. On first run the great work is: *Master Rust, EVM
internals and networking, with at least twenty-five minutes of focused work every day.*

### Commands

Type these in the terminal, in either mode:

```
/mute  /unmute               deafen her (closes the mic) / let her hear again
/stop                        silence her: cut off the current reply
/sleep /wake                 pause / resume listening
/timer start|pause|reset     the vigil (25 min)
/kept  /broken               your verdict on the last oath
/status                      the great work, the chain of seals, the oaths
/facts   /forget N           what she knows of you; strike fact N out
/mission [text]              show or rename the great work
/search words                search everything ever said
/end                         close the rite (asks tomorrow's oath; /end again skips it)
```

Ctrl-C closes the session (summary included); a second Ctrl-C quits at once.

## Tests

```bash
cargo test --release                    # unit tests
cargo test --release -- --ignored       # + Kokoro -> Whisper round trip (needs the models)
```
