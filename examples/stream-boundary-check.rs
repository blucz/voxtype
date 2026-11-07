//! Local model regression: stream a fixture, then compare the same stream's
//! canonical token decoding with the deltas delivered to Voxtype. No mic/paste.
use tokio::sync::mpsc;
use voxtype::config::Config;
use voxtype::transcribe::{
    parakeet_streaming::ParakeetStreamingTranscriber, StreamingEvent, StreamingTranscriber,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config: Config = toml::from_str(&std::fs::read_to_string(
        std::env::args().nth(1).expect("config path"),
    )?)?;
    let engine =
        ParakeetStreamingTranscriber::new(config.parakeet.as_ref().expect("Parakeet config"))?;
    let mut wav = hound::WavReader::open("tests/fixtures/vad/speech_long.wav")?;
    let samples: Vec<f32> = wav
        .samples::<i16>()
        .map(|s| s.map(|v| f32::from(v) / 32768.0))
        .collect::<Result<_, _>>()?;
    let model_path = std::env::args().nth(2).expect("model directory");
    let handle = parakeet_rs::ParakeetUnifiedHandle::load(model_path, None)?;
    let c = config.parakeet.as_ref().unwrap();
    let mut reference = parakeet_rs::ParakeetUnified::from_shared_with_streaming_config(
        &handle,
        parakeet_rs::UnifiedStreamingConfig {
            chunk_secs: c.streaming_chunk_secs,
            left_context_secs: c.streaming_left_context_secs,
            right_context_secs: c.streaming_right_context_secs,
        },
    )?;
    for chunk in samples.chunks(2560) {
        let _ = reference.transcribe_chunk(chunk)?;
    }
    let _ = reference.flush()?;
    let canonical = reference.get_transcript();
    let (tx, rx) = mpsc::channel(32);
    let mut stream = engine.start_stream(rx)?;
    for chunk in samples.chunks(2560) {
        tx.send(chunk.to_vec()).await?;
    }
    drop(tx);
    let mut text = String::new();
    let mut final_tail = String::new();
    while let Some(event) = stream.events.recv().await {
        match event {
            StreamingEvent::Partial { text: delta, .. } => text.push_str(&delta),
            StreamingEvent::Final { text: delta, .. } => {
                final_tail = delta.clone();
                text.push_str(&delta);
            }
            StreamingEvent::Error(e) => return Err(e.into()),
            StreamingEvent::Ended => break,
            _ => anyhow::bail!("unexpected revision"),
        }
    }
    stream.task.await??;
    assert!(
        final_tail.starts_with(char::is_whitespace),
        "fixture must exercise a word boundary at flush"
    );
    assert_eq!(
        text.trim(),
        canonical.trim(),
        "stream boundaries changed canonical decoded text"
    );
    println!("PASS: streamed fixture matches canonical token decoding ({} characters; final tail starts with whitespace: {})",text.len(),final_tail.starts_with(char::is_whitespace));
    Ok(())
}
