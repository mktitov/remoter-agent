#!/usr/bin/env python3
"""Fake ACP agent for the remoter-agent `kimi-acp` driver tests.

Speaks newline-delimited JSON-RPC over stdio, just enough ACP for the driver:
initialize, session/new, session/resume (only "known-session" succeeds),
session/set_config_option, session/prompt with streamed agent_message_chunk
updates, one session/request_permission and one elicitation/create round-trip
mid-turn, and an end-turn response carrying token usage.

Env knobs:
  FAKE_AGENT_CAPTURE  — path; every notable event is appended as a JSON line.
  FAKE_AGENT_PIDFILE  — path; the agent writes its pid there at startup.
  FAKE_AGENT_HANG=1   — never answer session/prompt (kill-on-drop tests).
  FAKE_AGENT_EXIT_ON_PROMPT=1 — exit the process on session/prompt without
                        answering, leaving a grandchild holding the stdout pipe
                        open (died-mid-turn detection tests: no pipe EOF for
                        the connection to notice).
  FAKE_AGENT_EMPTY=1  — end the turn with no streamed text (zero-output tests).
  FAKE_AGENT_NO_ALLOW=1 — offer only a reject permission option (the driver
                        must cancel the request, not pick the reject).
  FAKE_AGENT_FAIL_PROMPT_ON_SESSION — comma-separated session ids whose
                        session/prompt gets a JSON-RPC error ("total message
                        size exceeds limit", the provider context-limit
                        wording) instead of a turn (resume→fresh fallback tests).
"""

import json
import os
import subprocess
import sys
import time

capture_path = os.environ.get("FAKE_AGENT_CAPTURE")
hang = os.environ.get("FAKE_AGENT_HANG") == "1"
exit_on_prompt = os.environ.get("FAKE_AGENT_EXIT_ON_PROMPT") == "1"
empty = os.environ.get("FAKE_AGENT_EMPTY") == "1"
no_allow = os.environ.get("FAKE_AGENT_NO_ALLOW") == "1"
fail_prompt_on_session = [
    s for s in os.environ.get("FAKE_AGENT_FAIL_PROMPT_ON_SESSION", "").split(",") if s
]

pidfile = os.environ.get("FAKE_AGENT_PIDFILE")
if pidfile:
    with open(pidfile, "w") as f:
        f.write(str(os.getpid()))

# Config options advertised by this fake agent. The driver is expected to call
# `session/set_config_option` for any per-kind model/thinking configuration.
# Like the real kimi CLI, the `thinking` choices depend on the current model:
# `kimi-code/kimi-for-coding` accepts only "on" (console: On / Off unsupported).
current_model = "default-model"


def config_options():
    thinking = [
        {"value": "low", "name": "Low"},
        {"value": "high", "name": "High"},
        {"value": "max", "name": "Max"},
    ]
    if current_model == "kimi-code/kimi-for-coding":
        thinking = [{"value": "on", "name": "On"}]
    return [
        {
            "id": "model",
            "name": "Model",
            "type": "select",
            "currentValue": current_model,
            "options": [
                {"value": "kimi-code/k3", "name": "K3"},
                {"value": "kimi-code/kimi-for-coding", "name": "K2.7 Coding"},
                {"value": "default-model", "name": "Default"},
            ],
        },
        {
            "id": "thinking",
            "name": "Thinking",
            "type": "select",
            "currentValue": thinking[0]["value"],
            "options": thinking,
        },
    ]


def record(entry):
    if capture_path:
        with open(capture_path, "a") as f:
            f.write(json.dumps(entry) + "\n")


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def chunk(session_id, text):
    send({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
                "messageId": "m1",
            },
        },
    })


def finish_turn(prompt_msg):
    sid = prompt_msg["params"]["sessionId"]
    if not empty:
        chunk(sid, "world")
    send({
        "jsonrpc": "2.0",
        "id": prompt_msg["id"],
        "result": {
            "stopReason": "end_turn",
            "usage": {"totalTokens": 46, "inputTokens": 12, "outputTokens": 34},
        },
    })


next_req = 0
pending = {}  # our request id -> ("permission" | "elicitation", prompt_msg)
session_count = 0

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)

    if "method" not in msg:
        # A response to one of our requests.
        entry = pending.pop(msg.get("id"), None)
        if entry:
            kind, prompt_msg = entry
            record({kind: msg.get("result", msg.get("error"))})
            if kind == "permission":
                # After the permission answer, ask an elicitation.
                next_req += 1
                pending[next_req] = ("elicitation", prompt_msg)
                send({
                    "jsonrpc": "2.0",
                    "id": next_req,
                    "method": "elicitation/create",
                    "params": {
                        "mode": "form",
                        "sessionId": prompt_msg["params"]["sessionId"],
                        "requestedSchema": {"type": "object", "properties": {}},
                        "message": "Proceed?",
                    },
                })
            else:
                finish_turn(prompt_msg)
        continue

    method = msg["method"]
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"],
              "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        session_count += 1
        record({"session_new": {
            "cwd": msg["params"].get("cwd"),
            "mcpServers": msg["params"].get("mcpServers", []),
        }})
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "sessionId": "sess-%d" % session_count,
            "configOptions": config_options(),
        }})
    elif method == "session/resume":
        record({"session_resume": msg["params"].get("sessionId")})
        if msg["params"].get("sessionId") == "known-session":
            send({"jsonrpc": "2.0", "id": msg["id"], "result": {
                "configOptions": config_options(),
            }})
        else:
            send({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32002, "message": "no such session"}})
    elif method == "session/set_config_option":
        record({"config_option": msg["params"]})
        if msg["params"].get("configId") == "model":
            current_model = msg["params"].get("value")
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"configOptions": config_options()}})
    elif method == "session/prompt":
        if hang:
            record({"prompt_hang": True})
            time.sleep(3600)
        if exit_on_prompt:
            record({"prompt_exit": True})
            # Die without answering, but keep the stdout pipe open via a
            # grandchild — the connection sees no EOF, so only the driver's
            # exit detection unwedges the pending request. os._exit skips
            # stdio flush, matching a real crash.
            subprocess.Popen(["sleep", "60"], stdin=subprocess.DEVNULL, stdout=sys.stdout, stderr=subprocess.DEVNULL)
            os._exit(1)
        sid = msg["params"]["sessionId"]
        record({"prompt": msg["params"].get("prompt")})
        if fail_prompt_on_session and sid in fail_prompt_on_session:
            record({"prompt_failed": sid})
            send({"jsonrpc": "2.0", "id": msg["id"],
                  "error": {"code": -32603,
                            "message": "400 total message size exceeds limit"}})
            continue
        if not empty:
            chunk(sid, "Hello, ")
        # Reject is listed first: the driver must still pick the allow option.
        options = (
            [{"optionId": "reject", "name": "Reject", "kind": "reject_once"}]
            if no_allow
            else [
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
            ]
        )
        next_req += 1
        pending[next_req] = ("permission", msg)
        send({
            "jsonrpc": "2.0",
            "id": next_req,
            "method": "session/request_permission",
            "params": {
                "sessionId": sid,
                "toolCall": {"toolCallId": "t1", "title": "run tests"},
                "options": options,
            },
        })
    # Anything else (notifications, cancels) is ignored.
