# lasthope

A personal voice companion written in Rust. You speak, she listens, thinks and answers aloud.
Hearing and speaking run locally; only the language model call goes to the cloud.

> All five phases are built: the foundation, her character and conversation, memory and
> sessions, the vigil, and the web UI; plus the grimoire, reading your PDFs with her. Still to come: an LLM fallback chain, all-day
> listening, and answering only your voice.

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

**Stopping mid-sentence:** if the words end where a sentence cannot (on "and", "because",
"the", "my", "um", a comma or "…"), she keeps listening three seconds longer; go on and
the whole sentence is heard as one. If you stay silent she asks for the rest ("Go on.",
"Finish the thought.": fixed lines, no model call, not saved), and what you say next is
joined to what you said before (a spoken command still stands alone). This reads the
transcript, so it can miss a cut-off that Whisper rounds into a finished sentence.

**Bluetooth headphones:** while a Bluetooth headset's own mic is open, macOS switches it
to phone-call quality (16 kHz, both directions), which makes her sound like a phone call,
causes dropouts each time the mic opens or closes, and hurts recognition. So by default
she listens through another mic (e.g. the MacBook's) when the default mic belongs to your
headphones.

**Choosing the mic:** pick it live in the Chamber (*Her nature → Microphone*) or with
`/mic` (list, then `/mic N`, `/mic auto`, `/mic default`, or part of a name). The change
applies at once, even while she listens, and is kept across restarts. `MIC_DEVICE` is only
the starting choice when none was saved. A chosen mic that is unplugged stays chosen:
she listens through the automatic choice until it returns. `lasthope --mics` lists the
mics with `MIC_DEVICE` only (it does not read the saved choice).

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
  timer.rs       the vigil (focus timer) and the spoken words for it
  grimoire.rs    the books: PDF indexing, choosing a page, the lesson and judging prompts
  text_io.rs     stdin/stdout mode, slash-commands, event printer
  web.rs         the web UI server: files, JSON API, WebSocket, who may come in
ui/
  index.html     the Chamber   grimoire.html  the Grimoire   archive.html  the Archive
  app.css        the look (colours, fonts, the seal's motion)
  common.js      connection and helpers; chamber.js, grimoire.js, archive.js: the pages
  pdfjs/         pdf.js 6.5.438 (Mozilla, Apache-2.0), served from this machine
  fonts/         Cinzel, IM Fell English, JetBrains Mono (SIL OFL, see OFL.txt)
  assets/        seal.png, your artwork (gitignored; a dark circle stands in without it)
```

The core owns all state. Anything that drives it (voice, `--text`, later the web UI)
sends `Command`s and watches `Event`s:

- Commands: `mute`, `stop_speaking`, `sleep`, `end_session`, `open_rite`, `oath_verdict`,
  `set_mission`, `forget_fact`, `timer_start` (optional `minutes`), `timer_pause`, `timer_reset`,
  `log_vigil` (`minutes`, optional `yesterday`), `set_voice` (`voice`, `pace`), `set_speaks_first`,
  `read_aloud` (`session`), `burn_session` (`session`), `read` (`from`: `next`, `oath`,
  `random`), `read_page` (`book`, `page`), `leave_page`, `set_book` (`book`), `reindex`
- Events: `session_started`, `state_changed`, `user_spoke`, `she_spoke`, `timer_tick`,
  `memory_changed`, `notice`, `error`, `session_ended`, `reading_changed`, `shelf_changed`,
  `goodbye`

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
   `MIC_DEVICE`, `END_OF_TURN_MS`, `BOOKS_DIR` (see `.env.example`).

## Run

```bash
cargo run --release            # voice: speak into the mic; the web UI at http://localhost:7700
cargo run --release -- --text  # type instead: stdin replaces the mic, stdout the speaker
cargo run --release -- --no-ui # no web UI: the program ends when the rite closes
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
These spoken phrases are commands instead:

- **"close the rite"** or **"end the session"** (on its own): ends the session.
- **"begin the vigil"** / **"pause the vigil"** / **"reset the vigil"** (on their own; "start",
  "resume", "hold", "timer" work too): see [The vigil](#the-vigil).
- **"read the next page"** / **"read a random page"** / **"read a page for my oath"** /
  **"leave the page"**: see [The grimoire](#the-grimoire).
- **"please say …"** (or "please generate …", "please store …"): the rest is
  spoken into `output.wav` instead of being answered (voice mode only).

### Her character

She lives in [`persona.md`](persona.md): a calm, dark, commanding mentor who
speaks in the language of ritual (the great work, oaths, vigils, seals) and
pushes you toward your work, the gym and other people rather than toward
talking to her. You can still talk to her of anything; she follows you to other
subjects and turns you back to the work only when talk is standing in for it. She also speaks
of money: learning as the investment that compounds into better pay, freedom and an easier
life. She gives no investment advice. The file is re-read on every reply, so edits apply
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
- **vigils**: focused work by day, timed or reported by you
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

When the LLM provider pushes back (429 rate limit, 5xx, timeout or dropped connection) the
request is retried up to four times, and she says so once per reply: *"Retrying, the API hit
the limit."* or *"Retrying, the API is not answering."* If every try fails she says that too
(*"…Give it a minute, then speak again."*). These status lines are never saved to memory.

### The vigil

A focus timer, twenty-five minutes unless you say otherwise ("begin a fifty minute vigil",
`/timer start 50`). It keeps running while you talk to her, sleep her or go quiet; a paused
vigil resumes where it stopped.

- When it ends she announces it aloud (right after her current sentence, if she is speaking),
  and its minutes are recorded on the session and the day.
- Only completed vigils count. A vigil paused, reset or left running when the rite closes is
  not counted.
- **Kept one while she was not running?** Just tell her: "I completed a vigil", "I did two
  vigils yesterday", "I did a forty minute vigil this morning", "I did an hour of focused
  work". The Rust code reads it from your words (plain past-tense statements only; questions,
  plans and "I didn't…" are not reports), records it for today or yesterday, and she
  acknowledges it. Without a length, a vigil counts as twenty-five minutes. Saying "I
  completed it" just after the timer ended does not count that vigil twice. `/vigil 40` and
  `/vigil yesterday 40` do the same by command.
- She knows today's total and the running vigil, so she can hold you to the mission.

### The grimoire

Point `BOOKS_DIR` at a folder of PDFs (default `./books`, gitignored). She reads them with
you, one page at a time.

- **Indexing**: at startup, and whenever you press *Read the shelf again* in the Grimoire (or
  type `/reindex`), every PDF in the folder (and its subfolders) is hashed. Only new or
  changed files are read; each page's text goes into SQLite (`books`, `pages`, with FTS5
  over the text). A book with no text to extract (a scan) is flagged, not an error; a file
  that will not open is marked unreadable; a file that left the folder is marked gone, and
  its pages are kept so old questions on it can still be judged. Your place in each book
  (the last page read) is kept.
- **Choosing a page**: always the Rust code, never the model. By default the next unread
  page of the book you read (pages with too little text, like titles and figures, are
  skipped); on request the page that best matches your oath (full-text search over every
  book; the newest oath for today or tomorrow, or the great work if none is sworn; a page
  taught in the last 14 days is passed over if another matches); a random page; or the page
  you are looking at in the Grimoire. The next unread page and a page you chose move your
  place; the oath and random pages do not.
- **The lesson**: the Grimoire shows the real page (pdf.js, served locally). The model gets
  only that page's text, explains its core idea in three spoken sentences, then asks two
  questions, one at a time. Your spoken (or typed) answer is judged against the page text.
  Every question, your answer and whether it was right are stored (`questions`).
- **Wrong answers come back**: at the start of a later session, once you have spoken and
  she has answered, she asks again up to two old questions you got wrong (never ones asked
  in the last twelve hours) and shows their pages. A question counts as learned once you
  answer it right.
- While a question waits, the judging call first decides whether what you said answers it.
  Anything else (another subject, a question of your own) gets an ordinary reply, is not
  graded, and the question stays open for later. Spoken commands still work, and "leave the
  page" closes the book. Only PDFs for now (no EPUB).

### The web UI

Open **http://localhost:7700** while she runs. Plain HTML, CSS and JavaScript served from
`ui/` (no framework, no build step: edit a file and reload).

- **Chamber**: her seal (it breathes while she listens, quickens when she speaks, dims while
  she sleeps), her last words and the live transcript, Deafen / Silence / Slumber /
  Close (or Open) the rite, the vigil with its length, a field to speak to her in writing,
  the great work (renamable), the seven seals of the chain, the due oath with Kept / Broken,
  what she knows of you (strike out a fact with two presses), and her nature: voice
  (Kokoro's English voices), pace of speech, the microphone (automatic, the system
  default, or one by name, with the one she listens through), whether she speaks first.
- **Grimoire**: the shelf (each book with its pages and your place, scans and unreadable
  files marked; *Read this book*; *Read the shelf again* after adding or changing PDFs), the
  page viewer (back, onward, go to a page), *Read with her*: this page, the next unread, for
  my oath, a random page; her question with a field to answer in writing, and every
  question asked with your answer and her verdict. The Chamber shows a card while a book
  is open.
- **Archive**: every night (NOX I, II, …) with its account, what was sworn, what she learned
  and every word; full-text search over all of it; *Hear it spoken* (she reads the account
  aloud), *Copy it out*, and *Burn this night* (two presses: deletes its words and the facts
  learned in it; the minutes of focused work and the chain of seals are kept).
- While the UI is up, closing the rite does not end the program: the Chamber offers
  **Open the rite** for the next session (or type `/open`). Ctrl-C quits.
- "She answers only my voice" is shown but not built yet.

**Your phone:** set `UI_BIND=0.0.0.0`. At startup she prints a second link,
`http://<your-mac's-ip>:7700/?key=…`; open it once on the phone (the key is then kept in a
cookie; bookmark the page). Without the key other devices are refused. The key is made once
and kept in the database. Whatever the setting, other websites open in your browser cannot
reach her: requests with a foreign origin, or a foreign host name, are refused.

### Commands

Type these in the terminal, in either mode:

```
/mute  /unmute               deafen her (closes the mic) / let her hear again
/stop                        silence her: cut off the current reply
/sleep /wake                 pause / resume listening
/timer start [min]|pause|reset   the vigil (25 min unless told)
/vigil [yesterday] [min]     record a vigil you kept while she was not running (25 min)
/kept  /broken               your verdict on the last oath
/status                      the great work, the chain of seals, the oaths, today's focus
/facts   /forget N           what she knows of you; strike fact N out
/mission [text]              show or rename the great work
/search words                search everything ever said
/end                         close the rite (asks tomorrow's oath; /end again skips it)
/open                        open the next rite (while the web UI keeps her running)
/read [next|oath|random]     read a page with her (the next unread page by default)
/page BOOK PAGE              read the page you choose;  /leave  closes the book
/books   /book N             the shelf; make book N the one you read
/reindex                     look through BOOKS_DIR again for new and changed PDFs
/mic [N|auto|default|name]   list the microphones; choose one (at once, kept across restarts)
```

Ctrl-C closes the session (summary included); a second Ctrl-C quits at once.

## Tests

```bash
cargo test --release                    # unit tests
cargo test --release -- --ignored       # + Kokoro -> Whisper round trip (needs the models)
```
