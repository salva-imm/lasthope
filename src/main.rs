use std::time::Duration;
use tokio::sync::mpsc;
use std::collections::BTreeMap;

use std::error::Error;

use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, Sink};
use std::path::Path;
use sherpa_rs::tts::{KokoroTts, KokoroTtsConfig};
use serde::{Deserialize, Serialize};
use serde::de::StdError;
use tokio::fs;

#[derive(Serialize)]
struct CerebrasRequest {
    model: String,
    stream: bool,
    messages: Vec<Message>,
    temperature: f32,
    max_tokens: i32,
    seed: i32,
    top_p: f32,
}

#[derive(Serialize)]
struct Message {
    content: String,
    role: String,
}

#[derive(Deserialize)]
struct CerebrasResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: MessageResponse,
}

#[derive(Deserialize)]
struct MessageResponse {
    content: String,
}

fn split_into_chunks(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks = vec![];
    let mut current = String::new();

    for sentence in text.split_terminator('.') {
        let sentence = sentence.trim();
        if sentence.is_empty() { continue; }
        if current.len() + sentence.len() + 1 > max_len {
            chunks.push(current.trim().to_string());
            current.clear();
        }
        current.push_str(sentence);
        current.push('.');
    }
    if !current.is_empty() {
        chunks.push(current.trim().to_string());
    }
    chunks
}

async fn ask_cerebras(prompt: &str, api_key: &str) -> Result<String, Box<dyn Error>> {
    let client = reqwest::Client::new();

    let system_message = Message {
        role: "system".into(),
        content: "Please do not include any asterisks (*) in your replies. Skip or remove them entirely.".into(),
    };

    let user_message = Message {
        role: "user".into(),
        content: prompt.to_string(),
    };

    let body = CerebrasRequest {
        model: "llama-4-scout-17b-16e-instruct".into(),
        stream: false,
        messages: vec![system_message, user_message],
        temperature: 0.0,
        max_tokens: -1,
        seed: 0,
        top_p: 1.0,
    };

    let res = client
        .post("https://api.cerebras.ai/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&body)
        .send()
        .await?;

    let text = res.text().await?;
    // println!("Raw response: {}", text);

    let parsed: CerebrasResponse = serde_json::from_str(&text)?;

    Ok(parsed.choices[0].message.content.clone())
}

async fn tts_to_audio(tts_engine: &mut KokoroTts, text: &str) -> Result<(Vec<f32>, u32), Box<dyn StdError + Send + Sync>> {
    let audio = tts_engine.create(text, 2, 1.0)?;
    Ok((audio.samples, audio.sample_rate))
}

async fn tts_to_file(
    tts_engine: &mut KokoroTts,
    text: &str,
    filename: &str,
) -> Result<(), Box<dyn StdError + Send + Sync>> {
    let (samples, sample_rate) = tts_to_audio(tts_engine, text).await?;
    // Convert f32 samples to i16 WAV format
    let mut wav_writer = hound::WavWriter::create(filename, hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    })?;
    for sample in samples {
        let s = (sample * i16::MAX as f32) as i16;
        wav_writer.write_sample(s)?;
    }
    wav_writer.finalize()?;
    Ok(())
}

fn normalize_command(text: &str) -> String {
    let lowered = text.to_lowercase();
    lowered
        .replace("colon colon ai", "::ai")
        .replace("colon colon say", "::say")
}

#[tokio::main]
async fn main() {
    use kalosm::sound::*;
    let cerebras_api_key = "YOUR_KEY";
    let whisper = Whisper::builder()
        .with_source(WhisperSource::Small)
        .build()
        .await
        .expect("Failed to load Whisper");

    let config = KokoroTtsConfig {
        model: "./kokoro-multi-lang-v1_0/model.onnx".to_string(),
        voices: "./kokoro-multi-lang-v1_0/voices.bin".into(),
        tokens: "./kokoro-multi-lang-v1_0/tokens.txt".into(),
        data_dir: "./kokoro-multi-lang-v1_0/espeak-ng-data".into(),
        dict_dir: "./kokoro-multi-lang-v1_0/dict".into(),
        lexicon: "./kokoro-multi-lang-v1_0/lexicon-us-en.txt,./kokoro-multi-lang-v1_0/lexicon-zh.txt".into(),
        length_scale: 1.0,
        ..Default::default()
    };

    let mut tts_engine = KokoroTts::new(config);

    loop {
        let mic = MicInput::default();
        let stream = mic.stream();
        let mut transcribe = stream.transcribe(whisper.clone());
        let mut text = String::new();
        println!("Listening ...");
        while let Some(segment) = transcribe.next().await {
            text = segment.text().parse().unwrap();
            text = text.trim().to_lowercase();
            if !text.trim().is_empty() {
                println!("Transcribed: {}", text);
                break;
            }
        }
        drop(transcribe);


        if text.starts_with("please generate") || text.starts_with("please store") || text.starts_with("please say") {
            println!("INSIDE VOICE GENERATION BLOCK");

            // remove the command
            let content = text
                .trim_start_matches("please generate")
                .trim_start_matches("please store")
                .trim_start_matches("please say")
                .trim();
            if !content.is_empty() {
                println!("Simple TTS: {}", content);
                if let Err(e) = tts_to_file(&mut tts_engine, content, "output.wav").await {
                    eprintln!("Failed to save TTS file: {}", e);
                } else {
                    println!("Saved speech to output.wav");
                }
            }
        } else if text.starts_with("please ai") || text.starts_with("colon colon ai") || text.starts_with("talk to me") {
            println!("INSIDE AI BLOCK");
            // Normal AI conversation
            let prompt = text
                .trim_start_matches("please ai")
                .trim_start_matches("colon colon ai")
                .trim_start_matches("talk to me")
                .trim();
            match ask_cerebras(prompt, &cerebras_api_key).await {
                Ok(reply) => {
                    println!("AI: {}", reply);
                    let chunks: Vec<_> = split_into_chunks(&reply, 80)
                        .into_iter()
                        .filter(|c| !c.trim().is_empty())
                        .collect();

                    let (_stream, handle) = rodio::OutputStream::try_default()
                        .expect("failed to open audio output");
                    let sink = rodio::Sink::try_new(&handle)
                        .expect("failed to create sink");

                    for chunk in chunks {
                        match tts_to_audio(&mut tts_engine, &chunk).await {
                            Ok((samples, sample_rate)) => {
                                let src = rodio::buffer::SamplesBuffer::new(1, sample_rate, samples);
                                sink.append(src);
                            }
                            Err(e) => eprintln!("TTS failed: {}", e),
                        }
                    }
                    sink.sleep_until_end();
                }
                Err(e) => eprintln!("AI request failed: {}", e),
            }
        }
    }
}
