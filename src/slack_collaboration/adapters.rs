use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::coder::CoderKind;
use crate::content::ContentResolver;

use super::{
    ConversationProvider, InboundEnvelope, OutboundDelivery, ProviderScope, SlackTransport,
};

pub struct SocketModeTransport {
    web: SlackWebApiTransport,
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
}

impl SocketModeTransport {
    fn open_socket(
        api_base: &str,
        app_token: &str,
    ) -> Result<tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>>
    {
        let response = ureq::post(&format!(
            "{}/apps.connections.open",
            api_base.trim_end_matches('/')
        ))
        .set("Authorization", &format!("Bearer {app_token}"))
        .send_json(serde_json::json!({}))?;
        let value: serde_json::Value = response.into_json()?;
        let url = value["url"].as_str().context("Slack Socket Mode URL")?;
        Ok(tungstenite::connect(url)?.0)
    }

    pub fn connect(
        api_base: impl Into<String>,
        app_token: String,
        bot_token: String,
    ) -> Result<Self> {
        let api_base = api_base.into();
        let socket = Self::open_socket(&api_base, &app_token)?;
        Ok(Self {
            web: SlackWebApiTransport::new(api_base, app_token, bot_token),
            socket,
        })
    }
}

pub fn decode_socket_mode_envelope(text: &str) -> Result<InboundEnvelope> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let event = &value["payload"]["event"];
    Ok(InboundEnvelope {
        id: value["payload"]["event_id"]
            .as_str()
            .context("Slack event id")?
            .into(),
        acknowledgement_id: value["envelope_id"]
            .as_str()
            .context("Slack envelope id")?
            .into(),
        channel: event["channel"].as_str().context("Slack channel")?.into(),
        timestamp: event["ts"].as_str().context("Slack timestamp")?.into(),
        thread_timestamp: event["thread_ts"].as_str().map(str::to_owned),
        user: event["user"].as_str().context("Slack user")?.into(),
        text: event["text"].as_str().unwrap_or_default().into(),
    })
}

impl SlackTransport for SocketModeTransport {
    fn acknowledge(&mut self, event_id: &str) -> Result<()> {
        self.socket.send(tungstenite::Message::Text(
            serde_json::json!({"envelope_id": event_id}).to_string(),
        ))?;
        Ok(())
    }

    fn post(&mut self, delivery: &OutboundDelivery) -> Result<()> {
        self.web.post(delivery)
    }

    fn check(&self) -> Result<()> {
        self.web.check()
    }

    fn receive(&mut self) -> Result<InboundEnvelope> {
        let message = self.socket.read()?;
        decode_socket_mode_envelope(&message.into_text()?)
    }

    fn reconnect(&mut self) -> Result<()> {
        self.socket = Self::open_socket(&self.web.api_base, &self.web.app_token)?;
        Ok(())
    }

    fn bounded_history(&mut self, threads: &[(String, String)]) -> Result<Vec<InboundEnvelope>> {
        let mut events = Vec::new();
        for (channel, thread_timestamp) in threads {
            let value = self.web.call_value(
                "conversations.replies",
                &self.web.bot_token,
                serde_json::json!({"channel": channel, "ts": thread_timestamp, "limit": 100}),
            )?;
            for message in value["messages"].as_array().into_iter().flatten() {
                let Some(timestamp) = message["ts"].as_str() else {
                    continue;
                };
                if timestamp == thread_timestamp {
                    continue;
                }
                events.push(InboundEnvelope {
                    id: format!("history:{channel}:{timestamp}"),
                    acknowledgement_id: String::new(),
                    channel: channel.clone(),
                    timestamp: timestamp.into(),
                    thread_timestamp: Some(thread_timestamp.clone()),
                    user: message["user"].as_str().unwrap_or_default().into(),
                    text: message["text"].as_str().unwrap_or_default().into(),
                });
            }
        }
        Ok(events)
    }
}

pub struct SlackWebApiTransport {
    api_base: String,
    bot_token: String,
    app_token: String,
}

impl SlackWebApiTransport {
    pub fn new(api_base: impl Into<String>, app_token: String, bot_token: String) -> Self {
        Self {
            api_base: api_base.into().trim_end_matches('/').into(),
            app_token,
            bot_token,
        }
    }

    fn call_value(
        &self,
        method: &str,
        token: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let response = ureq::post(&format!("{}/{method}", self.api_base))
            .set("Authorization", &format!("Bearer {token}"))
            .send_json(body)?;
        let value: serde_json::Value = response.into_json()?;
        if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            bail!("Slack {method} failed")
        }
        Ok(value)
    }

    fn call(&self, method: &str, token: &str, body: serde_json::Value) -> Result<()> {
        self.call_value(method, token, body).map(|_| ())
    }
}

impl SlackTransport for SlackWebApiTransport {
    fn acknowledge(&mut self, event_id: &str) -> Result<()> {
        self.call(
            "apps.connections.ack",
            &self.app_token,
            serde_json::json!({"envelope_id": event_id}),
        )
    }

    fn post(&mut self, delivery: &OutboundDelivery) -> Result<()> {
        self.call("chat.postMessage", &self.bot_token, serde_json::json!({
            "channel": delivery.channel, "thread_ts": delivery.thread_timestamp, "text": delivery.text
        }))
    }

    fn check(&self) -> Result<()> {
        self.call("auth.test", &self.bot_token, serde_json::json!({}))
    }
}

pub struct CodexAppServerProvider {
    executable: PathBuf,
    runner_home: Option<PathBuf>,
}

impl CodexAppServerProvider {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            runner_home: None,
        }
    }

    pub fn new_scoped(executable: PathBuf, runner_home: PathBuf) -> Self {
        Self {
            executable,
            runner_home: Some(runner_home),
        }
    }

    fn turn(
        &self,
        action_id: &str,
        session: Option<&str>,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
    ) -> Result<(String, String)> {
        fs::create_dir_all(&scope.slice_draft)?;
        let profile = if let Some(runner_home) = &self.runner_home {
            let codex_home = scope.slice_draft.join(".codex-home");
            fs::create_dir_all(&codex_home)?;
            let resolver = ContentResolver::new(None);
            let profile = crate::os::render_profile_for_access_for_coder_with_codex_home(
                &resolver,
                &runner_home.display().to_string(),
                std::slice::from_ref(&scope.slice_draft),
                std::slice::from_ref(&scope.checkout),
                CoderKind::Codex,
                Some(&codex_home),
            )?;
            crate::os::preflight_profile(&profile)?;
            Some(profile)
        } else {
            None
        };
        let mut command = if let Some(profile) = &profile {
            crate::os::command_under_profile(profile, &self.executable)
        } else {
            Command::new(&self.executable)
        };
        command.arg("app-server");
        if self.runner_home.is_some() {
            command.env("CODEX_HOME", scope.slice_draft.join(".codex-home"));
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let request = serde_json::json!({"action_id": action_id, "session": session, "checkout": scope.checkout, "slice_draft": scope.slice_draft, "contribution": contribution, "actor": actor});
        child
            .stdin
            .as_mut()
            .context("open Codex App Server stdin")?
            .write_all(serde_json::to_string(&request)?.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!("Codex App Server failed")
        }
        let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        Ok((
            response["session"]
                .as_str()
                .context("Codex response session")?
                .into(),
            response["reply"]
                .as_str()
                .context("Codex response reply")?
                .into(),
        ))
    }
}

impl ConversationProvider for CodexAppServerProvider {
    fn start(
        &mut self,
        action_id: &str,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
    ) -> Result<(String, String)> {
        self.turn(action_id, None, scope, contribution, actor)
    }

    fn resume(
        &mut self,
        action_id: &str,
        session: &str,
        scope: &ProviderScope,
        contribution: &str,
        actor: &str,
    ) -> Result<String> {
        Ok(self
            .turn(action_id, Some(session), scope, contribution, actor)?
            .1)
    }

    fn ready(&self) -> Result<()> {
        Command::new(&self.executable)
            .arg("app-server")
            .arg("--help")
            .status()?
            .success()
            .then_some(())
            .context("Codex App Server is unavailable")
    }
}
