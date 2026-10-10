// src/app/agents.rs
//
// Requests from AI agents (`cyberterm +mcp`), recognised by the `agent`
// name the bridge adds to their control-socket params. They go through
// the user:
// - reading a pane (`get_text`, `blocks`) needs a grant for that pane,
//   asked for the first time ([agents] read);
// - typing (`send_text`) and running (`agent_run`) ask every time unless
//   allowed for that pane / agent ([agents] write);
// - panes an agent created with `agent_run` are readable by it;
// - a badge marks panes an agent works with, the context menu revokes
//   its grants, and every request lands in the audit log.
//
// A request that needs consent waits in a queue (its reply channel kept)
// while a prompt shows at the bottom of the focused pane.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;

use serde_json::{json, Value};

use super::*;
use crate::config::AgentPolicy;
use crate::control::{Call, Request, Response, RpcError, CONSENT_TTL};

/// How long a pane keeps its badge after an agent last touched it.
const BADGE_FOR: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Read,
    Type,
    Run,
}

struct Consent {
    call: Call,
    agent: String,
    kind: Kind,
    pane: Option<PaneId>,
    /// What the agent wants to type or run.
    detail: String,
    asked: Instant,
}

#[derive(Default)]
pub(super) struct AgentState {
    queue: VecDeque<Consent>,
    /// (agent, pane) allowed to read.
    read: HashSet<(String, PaneId)>,
    /// (agent, pane) allowed to type without asking.
    write: HashSet<(String, PaneId)>,
    /// Agents allowed to run commands without asking.
    run: HashSet<String>,
    /// Pane -> (agent, last activity), for the badge.
    seen: HashMap<PaneId, (String, Instant)>,
}

impl AgentState {
    fn has_grants(&self, pane: PaneId) -> bool {
        self.read
            .iter()
            .chain(self.write.iter())
            .any(|(_, p)| *p == pane)
    }
}

fn audit_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("cyberterm").join("agents.log")
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

/// Text as it will be shown in a prompt: control characters made visible
/// so an agent can't hide an Enter (or an escape sequence) in it.
fn visible(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\r' => "⏎".to_string(),
            '\n' => "↵".to_string(),
            '\t' => "⇥".to_string(),
            c if c.is_control() => format!("^{}", ((c as u8) ^ 0x40) as char),
            c => c.to_string(),
        })
        .collect()
}

impl App {
    /// Routes a control-socket call: agents' through the consent rules,
    /// everyone else's straight through.
    pub(super) fn on_control_call(&mut self, call: Call) {
        let Some(agent) = call
            .request
            .params
            .get("agent")
            .and_then(Value::as_str)
            .map(|a| clip(a, 40))
        else {
            let id = call.request.id.clone();
            let outcome = self.on_control(call.request);
            let _ = call.reply.send(Response::new(id, outcome));
            return;
        };
        self.agent_call(call, agent);
    }

    fn agent_call(&mut self, mut call: Call, agent: String) {
        let method = call.request.method.replace('-', "_");
        let reply = |call: Call, outcome: Result<Value, RpcError>| {
            let _ = call
                .reply
                .send(Response::new(call.request.id.clone(), outcome));
        };
        if !self.config.agents.enabled {
            self.audit(&agent, &method, None, "denied", "agents disabled");
            return reply(
                call,
                Err(RpcError::failed(
                    "agent access is turned off in this terminal ([agents] enabled = false)",
                )),
            );
        }
        let kind = match method.as_str() {
            "get_text" | "blocks" => Kind::Read,
            "send_text" => Kind::Type,
            "agent_run" => Kind::Run,
            "list_panes" | "list_tabs" | "history" | "ping" => {
                self.audit(&agent, &method, None, "allowed", "");
                let outcome = self.on_control(call.request.clone_request());
                return reply(call, outcome);
            }
            other => {
                self.audit(&agent, other, None, "denied", "not available to agents");
                return reply(
                    call,
                    Err(RpcError::failed(format!(
                        "`{other}` isn't available to agents"
                    ))),
                );
            }
        };
        // Pin the pane now, so focus moving before the user answers can't
        // redirect the request.
        let pane = if kind == Kind::Run {
            None
        } else {
            match self.agent_target(&call.request.params) {
                Ok(id) => {
                    call.request.params["pane"] = json!(id);
                    Some(id)
                }
                Err(e) => {
                    self.audit(&agent, &method, None, "failed", &e.message);
                    return reply(call, Err(e));
                }
            }
        };
        let detail = match kind {
            Kind::Read => String::new(),
            Kind::Type => call.request.params["text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            Kind::Run => call.request.params["command"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        };
        let a = &self.agents;
        let key = |p: PaneId| (agent.clone(), p);
        let (policy, granted) = match kind {
            Kind::Read => (
                self.config.agents.read,
                pane.is_some_and(|p| a.read.contains(&key(p))),
            ),
            Kind::Type => (
                self.config.agents.write,
                pane.is_some_and(|p| a.write.contains(&key(p))),
            ),
            Kind::Run => (self.config.agents.write, a.run.contains(&agent)),
        };
        if granted || policy == AgentPolicy::Allow {
            self.agent_execute(call, &agent, kind, pane, &detail, "allowed");
            return;
        }
        if policy == AgentPolicy::Deny {
            self.audit(&agent, &method, pane, "denied", &detail);
            let what = if kind == Kind::Read { "read" } else { "write" };
            return reply(
                call,
                Err(RpcError::failed(format!(
                    "the user doesn't allow agents to {what} here ([agents] {what} = \"deny\")"
                ))),
            );
        }
        self.agents.queue.push_back(Consent {
            call,
            agent,
            kind,
            pane,
            detail,
            asked: Instant::now(),
        });
        if let Some(gpu) = &self.gpu {
            if !self.window_focused {
                gpu.window
                    .request_user_attention(Some(winit::window::UserAttentionType::Informational));
            }
        }
        self.request_redraw();
    }

    fn agent_target(&self, params: &Value) -> Result<PaneId, RpcError> {
        match params.get("pane").and_then(Value::as_u64) {
            Some(id) => {
                let id = id as PaneId;
                self.pane(id)
                    .map(|p| p.id)
                    .ok_or_else(|| RpcError::invalid_params(format!("no pane {id}")))
            }
            None => self
                .focused_pane()
                .map(|p| p.id)
                .ok_or_else(|| RpcError::failed("no panes")),
        }
    }

    fn agent_execute(
        &mut self,
        call: Call,
        agent: &str,
        kind: Kind,
        pane: Option<PaneId>,
        detail: &str,
        decision: &str,
    ) {
        let method = call.request.method.replace('-', "_");
        let call_poll = call.request.params["poll"].as_bool() == Some(true);
        if let Some(p) = pane {
            if self.pane(p).is_none() {
                self.audit(agent, &method, pane, "failed", "pane closed");
                let _ = call.reply.send(Response::new(
                    call.request.id.clone(),
                    Err(RpcError::failed(format!("pane {p} was closed"))),
                ));
                return;
            }
        }
        let id = call.request.id.clone();
        let outcome = self.on_control(call.request.clone_request());
        let mut touched = pane;
        if let (Kind::Run, Ok(result)) = (kind, &outcome) {
            if let Some(new) = result["pane"].as_u64() {
                let new = new as PaneId;
                self.agents.read.insert((agent.to_string(), new));
                touched = Some(new);
            }
        }
        if let Some(p) = touched {
            self.agents
                .seen
                .insert(p, (agent.to_string(), Instant::now()));
        }
        let decision = if outcome.is_ok() { decision } else { "failed" };
        // wait_for_command's repeat polls would flood the log.
        if !call_poll {
            self.audit(agent, &method, touched, decision, detail);
        }
        let _ = call.reply.send(Response::new(id, outcome));
        self.request_redraw();
    }

    pub(super) fn consent_pending(&self) -> bool {
        !self.agents.queue.is_empty()
    }

    /// Keys while a consent prompt is showing: Enter/y allow once, a allow
    /// from now on, n/Esc deny. Everything else is swallowed so a keystroke
    /// meant for the shell can't answer by accident.
    pub(super) fn consent_key(&mut self, event: &KeyEvent) {
        if event.state != ElementState::Pressed || event.repeat {
            return;
        }
        let choice = match &event.logical_key {
            Key::Named(NamedKey::Enter) => Some(false),
            Key::Named(NamedKey::Escape) => None,
            Key::Character(c) => match c.to_lowercase().as_str() {
                "y" => Some(false),
                "a" => Some(true),
                "n" => None,
                _ => return,
            },
            _ => return,
        };
        let Some(consent) = self.agents.queue.pop_front() else {
            return;
        };
        let Consent {
            call,
            agent,
            kind,
            pane,
            detail,
            asked,
        } = consent;
        let method = call.request.method.replace('-', "_");
        if asked.elapsed() > CONSENT_TTL {
            self.audit(&agent, &method, pane, "expired", &detail);
        } else if let Some(always) = choice {
            if always {
                match (kind, pane) {
                    (Kind::Read, Some(p)) => {
                        self.agents.read.insert((agent.clone(), p));
                    }
                    (Kind::Type, Some(p)) => {
                        self.agents.write.insert((agent.clone(), p));
                        self.agents.read.insert((agent.clone(), p));
                    }
                    (Kind::Run, _) => {
                        self.agents.run.insert(agent.clone());
                    }
                    _ => {}
                }
            }
            let decision = if always {
                "allowed always"
            } else {
                "allowed once"
            };
            self.agent_execute(call, &agent, kind, pane, &detail, decision);
        } else {
            self.audit(&agent, &method, pane, "denied", &detail);
            let _ = call.reply.send(Response::new(
                call.request.id.clone(),
                Err(RpcError::failed("the user declined this request")),
            ));
        }
        self.request_redraw();
    }

    /// Answers requests whose caller has stopped waiting.
    pub(super) fn expire_consents(&mut self) {
        while self
            .agents
            .queue
            .front()
            .is_some_and(|c| c.asked.elapsed() > CONSENT_TTL)
        {
            let c = self.agents.queue.pop_front().unwrap();
            let method = c.call.request.method.replace('-', "_");
            self.audit(&c.agent, &method, c.pane, "expired", &c.detail);
            let _ = c.call.reply.send(Response::new(
                c.call.request.id.clone(),
                Err(RpcError::failed("the user didn't answer in time")),
            ));
        }
    }

    /// Withdraws every agent's grants for a pane (context menu).
    pub(super) fn revoke_agents(&mut self, pane: PaneId) {
        let a = &mut self.agents;
        a.read.retain(|(_, p)| *p != pane);
        a.write.retain(|(_, p)| *p != pane);
        a.seen.remove(&pane);
        self.request_redraw();
    }

    pub(super) fn pane_has_agent(&self, pane: PaneId) -> bool {
        self.agents.has_grants(pane) || self.agents.seen.contains_key(&pane)
    }

    pub(super) fn forget_pane_agents(&mut self, pane: PaneId) {
        self.revoke_agents(pane);
    }

    fn audit(&self, agent: &str, method: &str, pane: Option<PaneId>, decision: &str, detail: &str) {
        if !self.config.agents.audit_log {
            return;
        }
        let path = audit_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let line = json!({
            "time_ms": crate::shell::tap::now_ms(),
            "window": std::process::id(),
            "agent": agent,
            "method": method,
            "pane": pane,
            "decision": decision,
            "detail": clip(detail, 2000),
        });
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);
        if let Ok(mut f) = file {
            let _ = writeln!(f, "{line}");
        }
    }

    // ------------------------------------------------------------------
    // Drawing
    // ------------------------------------------------------------------

    /// The badge in a pane's bottom-right corner while an agent works with
    /// it (recent activity or a standing grant).
    /// `right` columns at the right edge are taken (the danger badge).
    pub(super) fn draw_agent_badge(&self, pane: PaneId, frame: &mut Frame, right: usize) -> usize {
        let a = &self.agents;
        let recent = a
            .seen
            .get(&pane)
            .filter(|(_, at)| at.elapsed() < BADGE_FOR)
            .map(|(name, _)| name.clone());
        let granted = a
            .read
            .iter()
            .chain(a.write.iter())
            .find(|(_, p)| *p == pane)
            .map(|(name, _)| name.clone());
        let Some(name) = recent.or(granted) else {
            return 0;
        };
        let text = format!(" ◆ {} ", clip(&name, 20));
        let width = text.chars().count();
        if frame.rows == 0 || frame.cols < width + right + 2 {
            return 0;
        }
        let bg = frame::hex_to_rgb(self.palette.ansi[5]);
        let fg = frame::hex_to_rgb(self.palette.bg);
        // Bottom-right: the top-right corner is where block badges sit.
        frame.put(frame.rows - 1, frame.cols - width - right, &text, fg, bg);
        width
    }

    /// The prompt for the oldest pending request, over the bottom rows of
    /// the focused pane.
    pub(super) fn draw_consent(&self, frame: &mut Frame) {
        let Some(c) = self.agents.queue.front() else {
            return;
        };
        let fg = frame::hex_to_rgb(self.palette.fg);
        let bg = frame::hex_to_rgb(self.palette.bg);
        let accent = frame::hex_to_rgb(self.palette.ansi[5]);
        let cols = frame.cols;

        let target = |p: Option<PaneId>| -> String {
            let Some(id) = p else { return String::new() };
            let Some(pane) = self.pane(id) else {
                return format!("pane {id}");
            };
            let mut s = format!("pane {id}");
            if id == self.focused {
                s.push_str(" (this one)");
            }
            if !pane.title.is_empty() {
                s.push_str(&format!(" · {}", clip(&pane.title, 40)));
            }
            if let Some(reason) = &pane.danger {
                s.push_str(&format!(" · ⚠ DANGEROUS ({reason})"));
            }
            s
        };
        let (headline, always) = match c.kind {
            Kind::Read => (
                format!("{} wants to read {}", c.agent, target(c.pane)),
                "a allow for this pane",
            ),
            Kind::Type => (
                format!("{} wants to type into {}:", c.agent, target(c.pane)),
                "a allow for this pane",
            ),
            Kind::Run => (
                format!("{} wants to run a command in a new pane:", c.agent),
                "a allow its commands from now on",
            ),
        };
        // Up to three rows of what will be typed or run.
        let shown = visible(&c.detail);
        let width = cols.saturating_sub(4).max(1);
        let chars: Vec<char> = shown.chars().collect();
        let mut detail: Vec<String> = chars.chunks(width).map(|w| w.iter().collect()).collect();
        if detail.len() > 3 {
            detail.truncate(3);
            if let Some(last) = detail.last_mut() {
                last.pop();
                last.push('…');
            }
        }
        let queued = self.agents.queue.len() - 1;
        let height = 2 + detail.len() + 1;
        if frame.rows < height {
            return;
        }
        let mut row = frame.rows - height;
        frame.fill(row, 0, bg, accent);
        let col = frame.put(row, 1, "◆ ", bg, accent);
        frame.put(
            row,
            col,
            &clip(&headline, cols.saturating_sub(4)),
            bg,
            accent,
        );
        row += 1;
        for line in &detail {
            frame.fill(row, 0, fg, bg);
            frame.put(row, 2, line, fg, bg);
            row += 1;
        }
        frame.fill(row, 0, fg, bg);
        row += 1;
        let more = if queued > 0 {
            format!("   ({queued} more waiting)")
        } else {
            String::new()
        };
        let keys = format!("Enter allow once · {always} · Esc deny{more}");
        frame.fill(row, 0, fg, bg);
        frame.put(row, 1, &clip(&keys, cols.saturating_sub(2)), fg, bg);
        frame.cursor = None;
    }
}

trait CloneRequest {
    fn clone_request(&self) -> Request;
}

impl CloneRequest for Request {
    fn clone_request(&self) -> Request {
        Request {
            id: self.id.clone(),
            method: self.method.clone(),
            params: self.params.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_keys_are_made_visible() {
        assert_eq!(visible("ls\r"), "ls⏎");
        assert_eq!(visible("a\x1b[2J"), "a^[[2J");
        assert_eq!(visible("x\ny\tz"), "x↵y⇥z");
    }

    #[test]
    fn clip_marks_cut_text() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("hello world", 5), "hello…");
    }
}
