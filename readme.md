# Rust Voice Assistant: Whisper + Cerebras + Kokoro TTS

A local, voice-driven AI assistant written in Rust. It captures microphone audio, transcribes it locally, queries a remote LLM, and plays back synthesized speech.

### Core Architecture

* **Speech-to-Text (STT):** Local transcription using `kalosm` (Whisper Small).
* **LLM Inference:** Remote processing via the Cerebras API (`llama-4-scout-17b-16e-instruct`).
* **Text-to-Speech (TTS):** Local synthesis using `sherpa_rs` (Kokoro TTS).
* **Audio I/O:** Microphone streaming via `kalosm` and audio playback/file generation via `rodio` and `hound`.

### Prerequisites

1. **Kokoro TTS Models:** You need the `kokoro-multi-lang-v1_0` directory in your project root containing `model.onnx`, `voices.bin`, `tokens.txt`, and the respective dictionaries/lexicons.
2. **Cerebras API Key:** Required for LLM inference.
3. **System Dependencies:** A working microphone and system audio output.

### Voice Commands

The system continuously listens to your microphone and triggers specific pipelines based on spoken prefixes.

**1. File Generation Mode**

* **Triggers:** `please generate...`, `please store...`, `please say...`
* **Action:** Synthesizes the following text into speech and saves it locally as `output.wav`.

**2. AI Conversation Mode**

* **Triggers:** `please ai...`, `colon colon ai...`, `talk to me...`
* **Action:** Strips the trigger phrase, sends the prompt to Cerebras, chunks the text response, and streams the synthesized audio back through your default speakers using `rodio`.

### Setup & Run

```bash
cargo build --release
cargo run --release

```