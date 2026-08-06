//! Edge TTS provider — uses Microsoft Edge's free Read Aloud service.
//!
//! Calls the `edge-tts` Python CLI as a subprocess. No API key required.
//! The model ID is used as the voice name (e.g. "zh-CN-XiaoxiaoNeural").

use std::process::Command;

use crate::providers::tts::{AudioData, AudioResponse, TtsProvider, TtsRequest, TtsVoice};

const DEFAULT_VOICE: &str = "zh-CN-XiaoxiaoNeural";

pub struct EdgeTtsProvider {
    /// Edge TTS binary path (default: "edge-tts" resolved from PATH).
    binary: String,
}

impl EdgeTtsProvider {
    pub fn new() -> Self {
        Self {
            binary: "edge-tts".to_string(),
        }
    }

    pub fn with_binary(binary: String) -> Self {
        Self { binary }
    }
}

impl Default for EdgeTtsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl TtsProvider for EdgeTtsProvider {
    fn synthesize(&self, req: TtsRequest) -> anyhow::Result<AudioResponse> {
        let voice = match &req.voice {
            TtsVoice::Id(id) if !id.is_empty() => id.clone(),
            _ => req.model.clone(),
        };
        let voice = if voice.is_empty() {
            DEFAULT_VOICE.to_string()
        } else {
            voice
        };

        // Use a temp file for the audio output.
        let temp_dir = tempfile::tempdir()?;
        let ext = match req.response_format {
            Some(crate::providers::TtsFormat::Wav) => "wav",
            _ => "mp3",
        };
        let output_path = temp_dir.path().join(format!("tts.{ext}"));

        let mut cmd = Command::new(&self.binary);
        cmd.args([
            "--voice",
            &voice,
            "--text",
            &req.input,
            "--write-media",
            output_path.to_str().unwrap(),
        ]);

        // Speed: edge-tts accepts rate as percentage like "+50%" or "-20%".
        if let Some(speed) = req.speed {
            if speed > 0.0 {
                let pct = ((speed - 1.0) * 100.0).round() as i32;
                let rate = format!("{pct:+d}%");
                cmd.args(["--rate", &rate]);
            }
        }

        let output = cmd
            .output()
            .map_err(|e| anyhow::anyhow!("failed to spawn edge-tts: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("edge-tts failed: {stderr}");
        }

        let bytes = std::fs::read(&output_path)?;
        let mime_type = match ext {
            "wav" => "audio/wav",
            _ => "audio/mp3",
        };

        Ok(AudioResponse {
            audio: AudioData {
                bytes,
                mime_type: mime_type.to_string(),
            },
            usage: None,
        })
    }
}
