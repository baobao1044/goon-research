// goon desktop front-end: streams the CLI's own NDJSON events into a chat transcript.
// No framework, no build step: the binary serves this file.

const transcript = document.getElementById("transcript");
const empty = document.getElementById("empty");
const form = document.getElementById("form");
const promptBox = document.getElementById("prompt");
const sendButton = document.getElementById("send");
const counter = document.getElementById("counter");
const statusChip = document.getElementById("status");
const cwdChip = document.getElementById("cwd");
const metaBox = document.getElementById("meta");
const sessionsBox = document.getElementById("sessions");

let running = false;
let assistantBubble = null;
let assistantText = "";

function escapeText(value) {
  return String(value ?? "");
}

function addBubble(kind, text) {
  if (empty) empty.style.display = "none";
  const node = document.createElement("div");
  node.className = `bubble ${kind}`;
  node.textContent = escapeText(text);
  transcript.appendChild(node);
  transcript.scrollTop = transcript.scrollHeight;
  return node;
}

function addEvent(parts) {
  if (empty) empty.style.display = "none";
  const row = document.createElement("div");
  row.className = "event";
  for (const part of parts) {
    const span = document.createElement("span");
    if (typeof part === "string") {
      span.textContent = part;
    } else {
      span.className = part.className || "";
      span.textContent = part.text;
    }
    row.appendChild(span);
  }
  transcript.appendChild(row);
  transcript.scrollTop = transcript.scrollHeight;
}

function addPanel(summary, body) {
  const details = document.createElement("details");
  details.className = "panel";
  const head = document.createElement("summary");
  head.textContent = summary;
  const pre = document.createElement("pre");
  pre.textContent = body;
  details.append(head, pre);
  transcript.appendChild(details);
  transcript.scrollTop = transcript.scrollHeight;
}

function setRunning(value) {
  running = value;
  sendButton.disabled = value;
  statusChip.textContent = value ? "running" : "idle";
  statusChip.className = value ? "status running" : "status";
}

function assistantAppend(chunk) {
  if (!assistantBubble) {
    assistantBubble = addBubble("assistant", "");
    assistantText = "";
  }
  assistantText += chunk;
  assistantBubble.textContent = assistantText;
  transcript.scrollTop = transcript.scrollHeight;
}

function finishAssistant() {
  assistantBubble = null;
  assistantText = "";
}

function summarizeTool(event) {
  const meta = event.meta || {};
  const label = meta.summary || meta.kind || "";
  const state = event.state || "";
  return { name: event.name || "tool", state, label };
}

function renderEvent(event) {
  const type = event.type;
  if (type === "text") {
    assistantAppend(event.text || "");
    return;
  }
  if (type === "reasoning") {
    addPanel("thinking", event.text || "");
    return;
  }
  if (type === "tool") {
    const { name, state, label } = summarizeTool(event);
    if (state === "start") {
      addEvent([{ text: `${name} `, className: "tool" }, label]);
    } else if (state === "error") {
      addEvent([{ text: `${name} `, className: "tool" }, { text: label || "failed", className: "bad" }]);
    } else {
      addEvent([{ text: `${name} `, className: "tool" }, { text: label, className: "ok" }]);
    }
    if (event.meta && event.meta.diff) addPanel(`${name} diff`, event.meta.diff);
    if (event.meta && event.meta.outputPreview) addPanel(`${name} output`, (event.meta.outputPreview || []).join("\n"));
    return;
  }
  if (type === "usage") {
    const total = event.total ?? 0;
    if (total) addEvent([`tokens ${total}`]);
    return;
  }
  if (type === "verify_start") {
    addEvent([{ text: "verify ", className: "tool" }, (event.commands || []).join(" · ")]);
    return;
  }
  if (type === "verify_result") {
    const bad = event.status === "fail";
    addEvent([{ text: "verify ", className: "tool" }, { text: event.status || "", className: bad ? "bad" : "ok" }, event.reason ? ` ${event.reason}` : ""]);
    return;
  }
  if (type === "verify_rollback") {
    addEvent([{ text: "rollback ", className: "warn" }, event.reason || ""]);
    return;
  }
  if (type === "compaction") {
    addEvent([`compaction #${event.count ?? 0}`]);
    return;
  }
  if (type === "retry") {
    addEvent([{ text: "retry ", className: "warn" }, `attempt ${event.attempt ?? 0}${event.message ? ` · ${event.message}` : ""}`]);
    return;
  }
  if (type === "error") {
    addBubble("error", event.text || event.message || "error");
    return;
  }
  if (type === "done") {
    if (event.text && !assistantText) assistantAppend(event.text);
    return;
  }
  if (type === "stopped") {
    addEvent([{ text: "stopped ", className: "warn" }, event.reason || ""]);
    return;
  }
  if (type === "harness" || type === "task_start" || type === "jit_repaired" || type === "harness_hook") {
    return;
  }
  // Anything else is kept visible but compact, so new CLI events are never silently dropped.
  addEvent([type || "event"]);
}

async function loadMeta() {
  try {
    const response = await fetch("/api/meta");
    const meta = await response.json();
    cwdChip.textContent = meta.cwd || "—";
    metaBox.textContent = `${meta.provider || "provider"} · ${meta.model || "no model"}\nv${meta.version || "?"}`;
  } catch (error) {
    metaBox.textContent = "CLI not reachable";
  }
}

async function loadSessions() {
  try {
    const response = await fetch("/api/sessions");
    const list = await response.json();
    sessionsBox.textContent = "";
    if (!Array.isArray(list) || list.length === 0) {
      const none = document.createElement("div");
      none.className = "session";
      none.textContent = "no sessions yet";
      sessionsBox.appendChild(none);
      return;
    }
    for (const session of list.slice(0, 25)) {
      const row = document.createElement("div");
      row.className = "session";
      const title = document.createElement("span");
      title.textContent = (session.title || session.cwd || "session").slice(0, 22);
      const id = document.createElement("span");
      id.className = "id";
      id.textContent = String(session.id || "").slice(0, 8);
      row.append(title, id);
      sessionsBox.appendChild(row);
    }
  } catch (error) {
    sessionsBox.textContent = "";
  }
}

async function runPrompt(text) {
  addBubble("user", text);
  setRunning(true);
  finishAssistant();
  try {
    const response = await fetch("/api/prompt", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ prompt: text }),
    });
    const contentType = response.headers.get("content-type") || "";
    if (!contentType.includes("text/event-stream")) {
      const payload = await response.json().catch(() => ({}));
      addBubble("error", payload.error || "the run could not start");
      return;
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let boundary = buffer.indexOf("\n\n");
      while (boundary !== -1) {
        const chunk = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        for (const line of chunk.split("\n")) {
          if (!line.startsWith("data: ")) continue;
          const raw = line.slice(6);
          let event;
          try {
            event = JSON.parse(raw);
          } catch (error) {
            continue;
          }
          if (event.type === "exit") {
            if (event.stderr) addPanel("stderr", event.stderr);
            if (event.code && event.code !== 0) addEvent([{ text: "exit ", className: "bad" }, String(event.code)]);
          } else {
            renderEvent(event);
          }
        }
        boundary = buffer.indexOf("\n\n");
      }
    }
    finishAssistant();
  } catch (error) {
    addBubble("error", String(error));
  } finally {
    setRunning(false);
    loadSessions();
  }
}

form.addEventListener("submit", event => {
  event.preventDefault();
  const text = promptBox.value.trim();
  if (!text || running) return;
  promptBox.value = "";
  counter.textContent = "0";
  runPrompt(text);
});

promptBox.addEventListener("input", () => {
  counter.textContent = String(promptBox.value.length);
});

promptBox.addEventListener("keydown", event => {
  if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
    event.preventDefault();
    form.requestSubmit();
  }
});

for (const chip of document.querySelectorAll(".chip")) {
  chip.addEventListener("click", () => {
    promptBox.value = chip.dataset.prompt || "";
    counter.textContent = String(promptBox.value.length);
    promptBox.focus();
  });
}

loadMeta();
loadSessions();
promptBox.focus();
