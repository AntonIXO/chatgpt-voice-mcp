use anyhow::{anyhow, bail, Context, Result};
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    schemars::JsonSchema,
    tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const SERVER_VERSION: &str = "0.1.0";
const DEFAULT_BACKEND: &str = "https://chatgpt.com/backend-api/codex";
const APP_DATA_DIR: &str = "chatgpt-voice-mcp";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("mcp") => {
            ChatGptVoice::default()
                .serve(rmcp::transport::stdio())
                .await?
                .waiting()
                .await?;
            Ok(())
        }
        Some("doctor") => {
            println!("{}", serde_json::to_string_pretty(&doctor())?);
            Ok(())
        }
        Some("login") => login(),
        Some("auth-status") => {
            println!("{}", serde_json::to_string_pretty(&auth_status())?);
            Ok(())
        }
        Some("--help") | Some("-h") | None => {
            println!("chatgpt-voice-mcp\n\nUsage:\n  chatgpt-voice-mcp login\n  chatgpt-voice-mcp auth-status\n  chatgpt-voice-mcp doctor\n  chatgpt-voice-mcp mcp\n\n`login` opens the official Codex device login and stores credentials in the\nMCP-owned CODEX_HOME. The MCP server exposes voice_narrate, voice_doctor, and\nvoice_auth_status.");
            Ok(())
        }
        Some(command) => bail!("unknown command '{command}'"),
    }
}

#[derive(Clone, Default)]
struct ChatGptVoice {
    active: Arc<Mutex<Option<u32>>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NarrateParams {
    /// The script or text to narrate. The model will speak this through ChatGPT Voice.
    text: String,
    /// Optional local video path. Metadata and a matching .srt/.vtt/.txt sidecar are added as context.
    video_path: Option<String>,
    /// Delivery direction, for example: "warm, excited, cinematic, with a short pause before the reveal".
    emotion: Option<String>,
    /// ChatGPT Voice name. v3 currently supports cove, juniper, maple, spruce, ember, vale, breeze, arbor, sol.
    voice: Option<String>,
    /// Save the assistant stream to this WAV path. With a path, playback is
    /// routed only to a private PipeWire null sink; the default speakers and
    /// physical microphone are left untouched.
    output_path: Option<String>,
    /// Maximum seconds to wait for the spoken response.
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct NarrateOutput {
    ok: bool,
    transcript: String,
    output_path: Option<String>,
    voice: String,
    thread_id: Option<String>,
    used_chatgpt_subscription: bool,
    message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct DoctorOutput {
    ok: bool,
    authenticated: bool,
    codex_home: String,
    codex: Option<String>,
    voice_host: Option<String>,
    pipewire_recorder: bool,
    pactl: bool,
    backend: String,
    note: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AuthStatusOutput {
    authenticated: bool,
    codex_home: String,
    auth_file: String,
    permissions_ok: bool,
    detail: String,
}

#[tool_router]
impl ChatGptVoice {
    #[tool(
        name = "voice_doctor",
        description = "Check ChatGPT subscription Voice readiness on Linux. This checks the bundled app-server/voice helper and PipeWire capture tools without starting a call."
    )]
    fn voice_doctor(&self) -> Json<DoctorOutput> {
        Json(doctor())
    }

    #[tool(
        name = "voice_auth_status",
        description = "Report whether this standalone MCP has its own saved ChatGPT/Codex login credentials. It never returns the credential contents."
    )]
    fn voice_auth_status(&self) -> Json<AuthStatusOutput> {
        Json(auth_status())
    }

    #[tool(
        name = "voice_narrate",
        description = "Narrate a script through the authenticated ChatGPT Voice realtime mode. Use this for emotional video narration. It creates a private ephemeral realtime thread, sends the script with delivery instructions, plays the answer, and optionally records assistant audio from a private PipeWire sink to WAV without touching the default speakers or microphone. Do not use the local Read Aloud backend for this task."
    )]
    async fn voice_narrate(
        &self,
        Parameters(params): Parameters<NarrateParams>,
    ) -> Json<NarrateOutput> {
        let active = self.active.clone();
        let output = tokio::task::spawn_blocking(move || run_narration(params, active))
            .await
            .unwrap_or_else(|error| Err(anyhow!("voice worker failed: {error}")));
        Json(match output {
            Ok(value) => value,
            Err(error) => NarrateOutput {
                ok: false,
                transcript: String::new(),
                output_path: None,
                voice: "cove".to_string(),
                thread_id: None,
                used_chatgpt_subscription: false,
                message: format!("ChatGPT Voice narration failed: {error:#}"),
            },
        })
    }
}

#[tool_handler(
    name = "chatgpt-voice-mcp",
    version = "0.1.0",
    instructions = "Use voice_narrate for emotional narration through ChatGPT Voice subscription limits. The standalone server uses credentials created by `chatgpt-voice-mcp login` in its own CODEX_HOME and the authenticated Codex app-server realtime WebRTC path, not the local Read Aloud/Kokoro backend. Pass the full script in text and a concise delivery direction in emotion. A video_path contributes local metadata and an adjacent subtitle/text sidecar; the realtime model does not automatically see raw video frames. Set output_path when a WAV file is required. Call voice_auth_status and voice_doctor if setup is uncertain."
)]
impl ServerHandler for ChatGptVoice {}

fn doctor() -> DoctorOutput {
    let bundle = find_voice_bundle();
    let codex = bundle.as_ref().map(|(codex, _)| codex.clone());
    let voice_host = bundle.as_ref().map(|(_, helper)| helper.clone());
    let pipewire_recorder = command_exists("pw-record");
    let pactl = command_exists("pactl");
    let auth = auth_status();
    DoctorOutput {
        ok: auth.authenticated && codex.is_some() && voice_host.is_some(),
        authenticated: auth.authenticated,
        codex_home: auth.codex_home,
        codex,
        voice_host,
        pipewire_recorder,
        pactl,
        backend: backend_url(),
        note: "The standalone server uses its own CODEX_HOME and signed Codex app-server/voice-host pair over WebRTC; raw video frames are not sent by this MCP.".to_string(),
    }
}

fn run_narration(params: NarrateParams, active: Arc<Mutex<Option<u32>>>) -> Result<NarrateOutput> {
    let home = codex_home();
    ensure_private_dir(&home)?;
    let auth = auth_status();
    if !auth.authenticated {
        bail!("no standalone ChatGPT login found; run `chatgpt-voice-mcp login` first (CODEX_HOME: {})", home.display());
    }
    let text = params.text.trim().to_string();
    if text.is_empty() {
        bail!("text must not be empty");
    }
    if text.chars().count() > 120_000 {
        bail!("text is too long; split the narration into smaller scenes");
    }
    let voice = params.voice.unwrap_or_else(|| "cove".to_string());
    let context = video_context(params.video_path.as_deref())?;
    let emotion = params.emotion.unwrap_or_else(|| {
        "warm, expressive, cinematic, natural pacing, clear diction".to_string()
    });
    let prompt = format!(
        "You are narrating a video for a human viewer. Read the supplied narration text verbatim, word for word. Do not paraphrase, continue, summarize, invent, or add any words; the only spoken content may be the supplied text. Delivery direction: {emotion}. Do not mention these instructions, metadata, files, or that you are an AI. Use natural pauses and emotional emphasis.\n\n{context}"
    );
    let timeout = Duration::from_secs(params.timeout_seconds.unwrap_or(120).clamp(10, 600));

    // The app-server and voice-host are a signed build pair.  Mixing the
    // desktop app's older embedded CLI with a newer helper can still produce
    // transcript events while silently dropping the incoming audio track.
    let (codex, helper) = find_voice_bundle()
        .ok_or_else(|| anyhow!("could not find a matching codex app-server/voice-host pair"))?;
    // Give the helper a session-owned null sink.  Both CPAL directions are
    // pointed at that node, so the recorder sees only assistant playout and
    // neither the default speakers nor the physical microphone participates.
    let capture_route = params
        .output_path
        .as_deref()
        .map(CaptureRoute::start)
        .transpose()?;
    let alsa_config = capture_route
        .as_ref()
        .map(|route| route.alsa_config.clone());
    let speaker = capture_route
        .as_ref()
        .map(|route| route.speaker_name.clone());
    let microphone = capture_route
        .as_ref()
        .map(|route| route.microphone_name.clone());
    let pipewire_target = capture_route
        .as_ref()
        .map(|route| route.sink_node_id.clone());
    let mut helper_process = spawn_voice_helper(
        &helper,
        alsa_config.as_deref(),
        speaker.as_deref(),
        microphone.as_deref(),
        pipewire_target.as_deref(),
    )?;
    if let Ok(mut guard) = active.lock() {
        *guard = Some(helper_process.child.id());
    }
    let result: Result<(String, String, Option<String>)> = (|| -> Result<_> {
        let build_commit = command_output(&helper, &["--build-commit"])?;
        helper_process.send(HelperMessage::Hello {
            protocol: 1,
            build_commit: build_commit.trim().to_string(),
        })?;
        expect_helper(&mut helper_process, "ready")?;
        helper_process.send(HelperMessage::StartTransport {})?;
        let offer = expect_helper(&mut helper_process, "offer")?;
        let offer_sdp = offer.sdp().context("voice helper returned no SDP offer")?;

        let mut app = AppServer::spawn(&codex, &home)?;
        app.send(json!({
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "codex-chatgpt-voice-linux", "version": SERVER_VERSION}, "capabilities": {"experimentalApi": true}}
        }))?;
        app.wait_for_id(1, timeout)?;
        app.send(json!({"method":"initialized","params":{}}))?;
        app.send(json!({"id":2,"method":"thread/start","params":{"cwd":env::current_dir()?.to_string_lossy(),"ephemeral":true,"approvalPolicy":"never","sandbox":"read-only"}}))?;
        let thread_response = app.wait_for_id(2, timeout)?;
        let thread_id = thread_response["result"]["thread"]["id"]
            .as_str()
            .ok_or_else(|| anyhow!("app-server did not return a thread id"))?
            .to_string();

        let record = Recorder::start(params.output_path.as_deref(), capture_route)?;
        app.send(json!({"id":3,"method":"thread/realtime/start","params":{"threadId":thread_id,"outputModality":"audio","transport":{"type":"webrtc","sdp":offer_sdp},"version":"v3","voice":voice,"includeStartupContext":false,"prompt":prompt}}))?;
        app.wait_for_id(3, timeout)?;
        let answer = app.wait_for_event_field("thread/realtime/sdp", "sdp", timeout)?;
        helper_process.send(HelperMessage::ApplyAnswer { sdp: answer })?;
        expect_helper(&mut helper_process, "transportReady")?;
        helper_process.send(HelperMessage::InitializeRuntime {})?;
        expect_helper(&mut helper_process, "runtimeReady")?;
        helper_process.send(HelperMessage::OpenDevices {
            selection: DeviceSelection {
                microphone: microphone
                    .clone()
                    .or_else(|| env::var("CODEX_CHATGPT_VOICE_MICROPHONE").ok()),
                speaker: speaker
                    .clone()
                    .or_else(|| env::var("CODEX_CHATGPT_VOICE_SPEAKER").ok()),
                channel: None,
            },
        })?;
        expect_helper(&mut helper_process, "devicesOpened")?;
        // The signed helper starts both local directions muted.  Opening the
        // CPAL streams is not enough to create its playout sink; the control
        // transition below both unmutes the speaker and attaches the incoming
        // WebRTC track to it.
        helper_process.send(HelperMessage::SetAudioControls {
            controls: AudioControls {
                microphone_muted: true,
                speaker_suppressed: false,
            },
        })?;
        expect_helper(&mut helper_process, "audioControlsApplied")?;

        // `appendSpeech` is the app-server control for speakable text.  The
        // similarly named `appendText` creates a realtime text item and may
        // emit a transcript without asking the audio modality to render it.
        let chunks = speech_chunks(&text);
        let mut transcript_parts = Vec::with_capacity(chunks.len());
        for (index, chunk) in chunks.iter().enumerate() {
            let request_id = 4 + index as u64;
            app.send(json!({"id":request_id,"method":"thread/realtime/appendSpeech","params":{"threadId":thread_id,"text":chunk}}))?;
            app.wait_for_id(request_id, timeout)?;
            transcript_parts.push(app.wait_for_transcript(&thread_id, timeout)?);
            // Keep adjacent speech turns from being coalesced by the realtime
            // service while retaining one continuous local recording.
            std::thread::sleep(Duration::from_millis(120));
        }
        let transcript = transcript_parts.join(" ");
        std::thread::sleep(Duration::from_millis(700));
        // Ask the signed helper for its level meter before stopping.  This is
        // a useful distinction between an empty WebRTC playout stream and a
        // recorder routed to the wrong local sink.
        helper_process.send(HelperMessage::InspectAudio {})?;
        let audio_state = expect_audio_state(&mut helper_process)?;
        eprintln!(
            "voice helper audio state: microphonePeak={} speakerPeak={}",
            audio_state.microphone_peak, audio_state.speaker_peak
        );
        let stop_id = 4 + chunks.len() as u64;
        app.send(
            json!({"id":stop_id,"method":"thread/realtime/stop","params":{"threadId":thread_id}}),
        )?;
        let _ = app.wait_for_id(stop_id, Duration::from_secs(10));
        let output_path = record.finish()?;
        if audio_state.speaker_peak == 0 {
            bail!("ChatGPT Voice returned no decoded speaker audio; refusing to report an empty recording");
        }
        Ok((thread_id, transcript, output_path))
    })();
    let _ = helper_process.send(HelperMessage::Close {});
    let _ = helper_process.child.kill();
    if let Ok(mut guard) = active.lock() {
        *guard = None;
    }
    let (thread_id, transcript, output_path) = result?;
    Ok(NarrateOutput {
        ok: true,
        transcript,
        output_path,
        voice,
        thread_id: Some(thread_id),
        used_chatgpt_subscription: true,
        message:
            "ChatGPT Voice realtime narration completed through the signed-in subscription session."
                .to_string(),
    })
}

/// Realtime speech appends have a much smaller practical payload window than
/// the app-server text limit.  Keep each spoken turn short and drop bracketed
/// stage directions so the model cannot accidentally read them aloud.
fn speech_chunks(text: &str) -> Vec<String> {
    const MAX_CHARS: usize = 180;
    let mut chunks = Vec::new();
    let mut current = String::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || (line.starts_with('[') && line.ends_with(']')) {
            continue;
        }
        for word in line.split_whitespace() {
            let extra = usize::from(!current.is_empty());
            if current.chars().count() + extra + word.chars().count() > MAX_CHARS
                && !current.is_empty()
            {
                chunks.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum HelperMessage {
    Hello { protocol: u32, build_commit: String },
    Ready {},
    InitializeRuntime {},
    RuntimeReady {},
    StartTransport {},
    Offer { sdp: String },
    ApplyAnswer { sdp: String },
    TransportReady {},
    TransportTimedOut {},
    OpenDevices { selection: DeviceSelection },
    DevicesOpened {},
    SetAudioControls { controls: AudioControls },
    AudioControlsApplied {},
    InspectAudio {},
    AudioState { state: AudioState },
    Close {},
    Closed {},
}

impl HelperMessage {
    fn kind(&self) -> &'static str {
        match self {
            Self::Ready {} => "ready",
            Self::Offer { .. } => "offer",
            Self::TransportReady {} => "transportReady",
            Self::RuntimeReady {} => "runtimeReady",
            Self::DevicesOpened {} => "devicesOpened",
            Self::AudioControlsApplied {} => "audioControlsApplied",
            Self::AudioState { .. } => "audioState",
            Self::Closed {} => "closed",
            _ => "other",
        }
    }
    fn sdp(&self) -> Option<String> {
        match self {
            Self::Offer { sdp } => Some(sdp.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AudioState {
    microphone_peak: u16,
    speaker_peak: u16,
}

fn expect_audio_state(helper: &mut VoiceHelper) -> Result<AudioState> {
    match helper.receive()? {
        HelperMessage::AudioState { state } => Ok(state),
        other => bail!("voice helper expected audioState, got {}", other.kind()),
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceSelection {
    microphone: Option<String>,
    speaker: Option<String>,
    channel: Option<Vec<u16>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AudioControls {
    microphone_muted: bool,
    speaker_suppressed: bool,
}

struct VoiceHelper {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
}

fn spawn_voice_helper(
    path: &str,
    alsa_config: Option<&str>,
    speaker: Option<&str>,
    microphone: Option<&str>,
    pipewire_target: Option<&str>,
) -> Result<VoiceHelper> {
    let mut command = Command::new(path);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in [
        ("GST_PLUGIN_PATH", ""),
        ("GST_PLUGIN_PATH_1_0", ""),
        ("GST_PLUGIN_SYSTEM_PATH", ""),
        ("GST_PLUGIN_SYSTEM_PATH_1_0", ""),
        ("GST_REGISTRY", "/dev/null"),
        ("GST_REGISTRY_UPDATE", "no"),
        ("GST_REGISTRY_FORK", "no"),
    ] {
        command.env(key, value);
    }
    if let Some(config) = alsa_config {
        command.env("ALSA_CONFIG_PATH", config);
    }
    if let Some(speaker) = speaker {
        command.env("CODEX_CHATGPT_VOICE_SPEAKER", speaker);
    }
    if let Some(microphone) = microphone {
        command.env("CODEX_CHATGPT_VOICE_MICROPHONE", microphone);
    }
    if let Some(target) = pipewire_target {
        // The PipeWire ALSA plugin applies PIPEWIRE_NODE to both the playback
        // and capture streams.  Pointing both directions at this session's
        // null sink prevents CPAL from opening the physical microphone while
        // keeping the assistant playout on the same isolated sink.
        command.env("PIPEWIRE_NODE", target);
    }
    // The signed helper carries its own `$ORIGIN` loader paths. Keep the
    // parent environment untouched until the helper explicitly initializes
    // its private GStreamer runtime.
    let mut child = command
        .spawn()
        .context("failed to start codex-voice-host")?;
    let input = child
        .stdin
        .take()
        .context("voice helper stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("voice helper stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("voice helper stderr unavailable")?;
    Ok(VoiceHelper {
        child,
        input,
        output: BufReader::new(stdout),
        stderr: BufReader::new(stderr),
    })
}

impl VoiceHelper {
    fn send(&mut self, message: HelperMessage) -> Result<()> {
        let payload = serde_json::to_vec(&message)?;
        let length = u32::try_from(payload.len()).context("voice helper frame too large")?;
        self.input.write_all(&length.to_be_bytes())?;
        self.input.write_all(&payload)?;
        self.input.flush()?;
        Ok(())
    }
    fn receive(&mut self) -> Result<HelperMessage> {
        let mut length = [0u8; 4];
        self.output
            .read_exact(&mut length)
            .context("voice helper closed while reading frame header")?;
        let length = u32::from_be_bytes(length) as usize;
        if length > 128 * 1024 {
            bail!("voice helper frame exceeds limit");
        }
        let mut payload = vec![0u8; length];
        self.output
            .read_exact(&mut payload)
            .context("voice helper closed while reading frame payload")?;
        Ok(serde_json::from_slice(&payload)?)
    }
}

fn expect_helper(helper: &mut VoiceHelper, expected: &str) -> Result<HelperMessage> {
    let message = match helper.receive() {
        Ok(message) => message,
        Err(error) => {
            let mut stderr = String::new();
            let _ = helper.stderr.read_to_string(&mut stderr);
            if stderr.trim().is_empty() {
                return Err(error)
                    .with_context(|| format!("voice helper closed while waiting for {expected}"));
            }
            bail!(
                "voice helper closed while waiting for {expected}: {error}; stderr: {}",
                stderr.trim()
            );
        }
    };
    if message.kind() != expected {
        bail!("voice helper expected {expected}, got {}", message.kind());
    }
    Ok(message)
}

struct AppServer {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Drop for AppServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl AppServer {
    fn spawn(codex: &str, codex_home: &Path) -> Result<Self> {
        let mut child = Command::new(codex);
        child.args([
            "-c",
            &format!(
                "experimental_realtime_webrtc_call_base_url=\"{}\"",
                backend_url()
            ),
            "app-server",
            "--stdio",
        ]);
        child
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        child.env("CODEX_HOME", codex_home);
        let mut child = child.spawn().context("failed to start codex app-server")?;
        let input = child.stdin.take().context("app-server stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("app-server stdout unavailable")?;
        Ok(Self {
            child,
            input,
            output: BufReader::new(stdout),
        })
    }
    fn send(&mut self, value: Value) -> Result<()> {
        serde_json::to_writer(&mut self.input, &value)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        Ok(())
    }
    fn next(&mut self, _timeout: Duration) -> Result<Value> {
        #[cfg(unix)]
        if self.output.buffer().is_empty() {
            let timeout_ms = _timeout.as_millis().min(i32::MAX as u128) as i32;
            let mut pollfd = libc::pollfd {
                fd: self.output.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if ready == 0 {
                bail!("timed out waiting for app-server");
            }
            if ready < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        let mut line = String::new();
        self.output
            .read_line(&mut line)
            .context("failed to read app-server")?;
        if line.trim().is_empty() {
            bail!("app-server closed stdout");
        }
        Ok(serde_json::from_str(line.trim())?)
    }
    fn wait_for_id(&mut self, id: u64, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let value = self.next(remaining)?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if value.get("error").is_some() {
                    bail!("app-server request {id} failed: {}", value);
                }
                return Ok(value);
            }
        }
    }
    fn wait_for_event_field(
        &mut self,
        method: &str,
        field: &str,
        timeout: Duration,
    ) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let value = self.next(deadline.saturating_duration_since(Instant::now()))?;
            if value.get("method").and_then(Value::as_str) == Some(method) {
                return value["params"][field]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("event {method} did not include {field}"));
            }
        }
    }
    fn wait_for_transcript(&mut self, thread_id: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let value = self.next(deadline.saturating_duration_since(Instant::now()))?;
            if value.get("method").and_then(Value::as_str)
                == Some("thread/realtime/transcript/done")
                && value["params"]["threadId"] == thread_id
                && value["params"]["role"] == "assistant"
            {
                return Ok(value["params"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string());
            }
            if value.get("method").and_then(Value::as_str) == Some("thread/realtime/error") {
                bail!("realtime error: {}", value["params"]["message"]);
            }
        }
    }
}

struct CaptureRoute {
    null_module: u32,
    sink_node_id: String,
    alsa_config: String,
    speaker_name: String,
    microphone_name: String,
    active: bool,
}

impl CaptureRoute {
    fn start(_output_path: &str) -> Result<Self> {
        let sink_name = format!("codex_voice_capture_{}", std::process::id());
        let null_module = pactl_load_module(&[
            "module-null-sink",
            &format!("sink_name={sink_name}"),
            "sink_properties=device.description=CodexVoiceCapture",
        ])?;
        let sink_node_id = match pactl_sink_node_id(&sink_name) {
            Ok(id) => id,
            Err(error) => {
                let _ = pactl_unload_module(null_module);
                return Err(error);
            }
        };
        let root = PathBuf::from(
            env::var("XDG_CACHE_HOME")
                .unwrap_or_else(|_| format!("{}/.cache", env::var("HOME").unwrap_or_default())),
        )
        .join("codex-desktop-dev/voice-mcp");
        if let Err(error) = fs::create_dir_all(&root) {
            let _ = pactl_unload_module(null_module);
            return Err(error.into());
        }
        let path = root.join(format!("asound-capture-{}.conf", std::process::id()));
        let base_config = match fs::read_to_string("/usr/share/alsa/alsa.conf") {
            Ok(config) => config,
            Err(error) => {
                let _ = pactl_unload_module(null_module);
                return Err(error).context("could not read the system ALSA base configuration");
            }
        };
        let config = format!(
            "{}\npcm.voice_play {{ type pipewire playback_node \"{sink_name}\" capture_node \"-1\" rate 48000 format S32_LE channels 2 hint {{ description \"Codex Voice Capture\" show on }} }}\npcm.voice_capture {{ type pipewire playback_node \"-1\" capture_node \"-1\" hint {{ description \"Codex Voice Microphone\" show on }} }}\npcm.voice_default {{ type asym playback.pcm \"voice_play\" capture.pcm \"voice_capture\" }}\npcm.!default voice_default\npcm.!sysdefault voice_default\nctl.!default {{ type pipewire }}\n",
            base_config,
        );
        if let Err(error) = fs::write(&path, config) {
            let _ = pactl_unload_module(null_module);
            return Err(error.into());
        }
        Ok(Self {
            null_module,
            sink_node_id,
            alsa_config: path.to_string_lossy().into_owned(),
            speaker_name: "Codex Voice Capture".to_string(),
            microphone_name: "Codex Voice Microphone".to_string(),
            active: true,
        })
    }

    fn finish(&mut self) {
        if !self.active {
            return;
        }
        let _ = pactl_unload_module(self.null_module);
        let _ = fs::remove_file(&self.alsa_config);
        self.active = false;
    }
}

impl Drop for CaptureRoute {
    fn drop(&mut self) {
        self.finish();
    }
}

struct Recorder {
    child: Option<Child>,
    path: Option<String>,
    route: Option<CaptureRoute>,
}
impl Recorder {
    fn start(path: Option<&str>, route: Option<CaptureRoute>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self {
                child: None,
                path: None,
                route,
            });
        };
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let target = route
            .as_ref()
            .map(|route| route.sink_node_id.as_str())
            .ok_or_else(|| anyhow!("an output capture route was not prepared"))?;
        let mut command = Command::new("pw-record");
        command.args([
            "--media-category",
            "Capture",
            "--target",
            target,
            "--rate",
            "48000",
            "--channels",
            "2",
            path.to_string_lossy().as_ref(),
        ]);
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start the private PipeWire recorder")?;
        Ok(Self {
            child: Some(child),
            path: Some(path.to_string_lossy().to_string()),
            route,
        })
    }
    fn finish(mut self) -> Result<Option<String>> {
        if let Some(mut child) = self.child.take() {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGINT);
            }
            let _ = child.wait();
        }
        if let Some(route) = self.route.as_mut() {
            route.finish();
        }
        Ok(self.path)
    }
}

fn pactl_load_module(args: &[&str]) -> Result<u32> {
    let output = Command::new("pactl")
        .args(["load-module"])
        .args(args)
        .output()
        .context("failed to invoke pactl load-module")?;
    if !output.status.success() {
        bail!(
            "pactl load-module failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("pactl returned an invalid module id")
}

fn pactl_sink_node_id(name: &str) -> Result<String> {
    for _ in 0..40 {
        if let Ok(output) = Command::new("pactl")
            .args(["list", "short", "sinks"])
            .output()
        {
            let listing = String::from_utf8_lossy(&output.stdout);
            if let Some(id) = listing.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                let id = fields.next()?;
                let candidate = fields.next()?;
                (candidate == name).then(|| id.to_string())
            }) {
                return Ok(id);
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!("PipeWire did not expose the isolated sink node: {name}")
}

fn pactl_unload_module(module: u32) -> Result<()> {
    let output = Command::new("pactl")
        .args(["unload-module", &module.to_string()])
        .output()
        .context("failed to invoke pactl unload-module")?;
    if !output.status.success() {
        bail!(
            "pactl unload-module failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn video_context(path: Option<&str>) -> Result<String> {
    let Some(path) = path else {
        return Ok("No local video file was supplied.".to_string());
    };
    let path = Path::new(path);
    if !path.is_file() {
        bail!("video_path does not exist: {}", path.display());
    }
    let mut context = format!(
        "Local video metadata (the realtime model does not receive raw frames): {}",
        path.display()
    );
    if command_exists("ffprobe") {
        if let Ok(output) = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration:stream=codec_name,width,height,r_frame_rate",
                "-of",
                "default=noprint_wrappers=1",
                &path.to_string_lossy(),
            ])
            .output()
        {
            context.push('\n');
            context.push_str(&String::from_utf8_lossy(&output.stdout));
        }
    }
    for extension in ["srt", "vtt", "txt", "md"] {
        let sidecar = path.with_extension(extension);
        if sidecar.is_file() {
            let text = fs::read_to_string(&sidecar).unwrap_or_default();
            if !text.trim().is_empty() {
                context.push_str(&format!(
                    "\nSidecar {}:\n{}",
                    sidecar.display(),
                    text.chars().take(30_000).collect::<String>()
                ));
            }
            break;
        }
    }
    Ok(context)
}

fn backend_url() -> String {
    env::var("CODEX_CHATGPT_VOICE_BACKEND_URL").unwrap_or_else(|_| DEFAULT_BACKEND.to_string())
}

fn data_home() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DATA_DIR)
}

fn codex_home() -> PathBuf {
    env::var_os("CHATGPT_VOICE_CODEX_HOME")
        .or_else(|| env::var_os("CODEX_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| data_home().join("codex-home"))
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn permissions_are_private(path: &Path) -> bool {
    #[cfg(unix)]
    {
        fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o077 == 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

fn auth_status() -> AuthStatusOutput {
    let home = codex_home();
    let auth_file = home.join("auth.json");
    let exists = auth_file.is_file();
    let permissions_ok = !exists || permissions_are_private(&auth_file);
    let authenticated = exists
        && permissions_ok
        && fs::metadata(&auth_file)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false);
    let detail = if authenticated {
        "standalone credentials are present; their contents are never returned".to_string()
    } else if exists && !permissions_ok {
        "auth.json exists but is group/world readable; run chmod 600 or login again".to_string()
    } else {
        "no standalone credentials; run `chatgpt-voice-mcp login`".to_string()
    };
    AuthStatusOutput {
        authenticated,
        codex_home: home.to_string_lossy().into_owned(),
        auth_file: auth_file.to_string_lossy().into_owned(),
        permissions_ok,
        detail,
    }
}

fn login() -> Result<()> {
    let home = codex_home();
    ensure_private_dir(&home)?;
    let codex = find_login_codex().ok_or_else(|| {
        anyhow!("could not find a Codex executable; set CHATGPT_VOICE_CODEX_LOGIN or install codex")
    })?;
    eprintln!(
        "Starting official device login with isolated CODEX_HOME={}.",
        home.display()
    );
    let status = Command::new(&codex)
        .args(["login", "--device-auth"])
        .env("CODEX_HOME", &home)
        .status()
        .with_context(|| format!("failed to start {codex} login"))?;
    if !status.success() {
        bail!("Codex device login exited with {status}");
    }
    #[cfg(unix)]
    if let Ok(auth_file) = fs::canonicalize(home.join("auth.json")) {
        fs::set_permissions(auth_file, fs::Permissions::from_mode(0o600))?;
    }
    let status = auth_status();
    if !status.authenticated {
        bail!(
            "login completed but no private auth.json was created: {}",
            status.detail
        );
    }
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

fn command_exists(command: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {command} >/dev/null 2>&1")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
fn command_output(command: &str, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(
        Command::new(command).args(args).output()?.stdout,
    )?)
}
fn find_codex() -> Option<String> {
    if let Ok(path) = env::var("CODEX_CHATGPT_VOICE_CODEX") {
        if Path::new(&path).is_file() {
            return Some(path);
        }
    }
    if let Ok(path) = env::var("CODEX_CLI_PATH") {
        if Path::new(&path).is_file() {
            return Some(path);
        }
    }
    for path in [
        "/usr/lib/chatgpt/resources/codex",
        "/usr/bin/codex",
        "/usr/local/bin/codex",
    ] {
        if Path::new(path).is_file() {
            return Some(path.to_string());
        }
    }
    command_exists("codex").then(|| "codex".to_string())
}
fn find_login_codex() -> Option<String> {
    if let Ok(path) = env::var("CHATGPT_VOICE_CODEX_LOGIN") {
        if Path::new(&path).is_file() {
            return Some(path);
        }
    }
    find_codex()
}
fn find_voice_host() -> Option<String> {
    if let Ok(path) = env::var("CODEX_CHATGPT_VOICE_HOST") {
        if Path::new(&path).is_file() {
            return Some(path);
        }
    }
    let root = codex_home().join("packages/app-server-daemon/releases");
    let mut found = Vec::new();
    for entry in fs::read_dir(root).ok()? {
        let path = entry
            .ok()?
            .path()
            .join("codex-resources/voice/bin/codex-voice-host");
        if path.is_file() {
            found.push(path);
        }
    }
    found.sort();
    found.pop().map(|path| path.to_string_lossy().to_string())
}

fn find_voice_bundle() -> Option<(String, String)> {
    if let (Ok(codex), Ok(helper)) = (
        env::var("CODEX_CHATGPT_VOICE_CODEX"),
        env::var("CODEX_CHATGPT_VOICE_HOST"),
    ) {
        if Path::new(&codex).is_file() && Path::new(&helper).is_file() {
            return Some((codex, helper));
        }
    }
    let root = codex_home().join("packages/app-server-daemon/releases");
    let mut pairs = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            let codex = dir.join("bin/codex");
            let helper = dir.join("codex-resources/voice/bin/codex-voice-host");
            if codex.is_file() && helper.is_file() {
                pairs.push((dir, codex, helper));
            }
        }
    }
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    if let Some((_, codex, helper)) = pairs.pop() {
        return Some((
            codex.to_string_lossy().into_owned(),
            helper.to_string_lossy().into_owned(),
        ));
    }
    // Development fallback for systems where the daemon package is absent.
    let codex = find_codex()?;
    let helper = find_voice_host()?;
    Some((codex, helper))
}

#[cfg(test)]
mod tests {
    use super::speech_chunks;

    #[test]
    fn speech_chunks_removes_stage_directions_and_preserves_words() {
        assert_eq!(
            speech_chunks("[Verse]\nHello, world!\n\n[Outro]\nGoodbye."),
            vec!["Hello, world! Goodbye.".to_string()]
        );
    }

    #[test]
    fn speech_chunks_limits_long_turns() {
        let chunks = speech_chunks(&"word ".repeat(100));
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 180));
    }
}
