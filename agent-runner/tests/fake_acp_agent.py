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
  FAKE_AGENT_HANG=1   — never answer session/prompt on its own (stall tests):
                        a session/cancel ends the turn with stopReason
                        "cancelled" …
  FAKE_AGENT_HANG_MARKER — path; like HANG but only while the marker file
                        does not exist (the first hang creates it). A retry
                        attempt spawns a fresh agent process, so per-process
                        state cannot express "stall once, then succeed".
  FAKE_AGENT_IGNORE_CANCEL=1 — … unless this is set: the cancel is recorded
                        and the hang continues (grace-expiry → ChildGuard
                        SIGTERM→SIGKILL tests).
  FAKE_AGENT_HANG_GRANDCHILD=1 — with HANG: on session/prompt spawn a
                        `sleep` grandchild holding the stdout pipe open (pid
                        appended to FAKE_AGENT_PIDFILE on its own line) so
                        tests can verify the whole process group is killed.
  FAKE_AGENT_QUOTA=1  — fail session/prompt with a provider quota error
                        ("HTTP 403: weekly usage limit") instead of a turn
                        (permanent, no-retry classification tests).
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

capture_path = os.environ.get("FAKE_AGENT_CAPTURE")
hang = os.environ.get("FAKE_AGENT_HANG") == "1"
hang_marker = os.environ.get("FAKE_AGENT_HANG_MARKER")
ignore_cancel = os.environ.get("FAKE_AGENT_IGNORE_CANCEL") == "1"
hang_grandchild = os.environ.get("FAKE_AGENT_HANG_GRANDCHILD") == "1"
quota = os.environ.get("FAKE_AGENT_QUOTA") == "1"
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
hung_prompt = None  # session/prompt msg being hung (FAKE_AGENT_HANG)

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
        if hang or (hang_marker and not os.path.exists(hang_marker)):
            record({"prompt_hang": True})
            if hang_marker:
                # First hang only: the next attempt's process sees the marker.
                open(hang_marker, "w").close()
            if hang_grandchild:
                # Keep the stdout pipe open via a grandchild and expose its
                # pid: after the ChildGuard TERM→KILLs the group, tests assert
                # the grandchild is gone too.
                gc = subprocess.Popen(
                    ["sleep", "3600"], stdin=subprocess.DEVNULL, stdout=sys.stdout, stderr=subprocess.DEVNULL
                )
                if pidfile:
                    with open(pidfile, "a") as f:
                        f.write("\n%d" % gc.pid)
            # Stay in the read loop so a session/cancel can end (or, with
            # IGNORE_CANCEL, be observed and ignored) the hung turn.
            hung_prompt = msg
            continue
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
        if quota:
            record({"prompt_quota": sid})
            send({"jsonrpc": "2.0", "id": msg["id"],
                  "error": {"code": -32603,
                            "message": "HTTP 403: you have reached your weekly usage limit for this model"}})
            continue
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
    elif method == "session/cancel":
        record({"cancel": msg["params"].get("sessionId")})
        if hung_prompt is not None and not ignore_cancel:
            send({
                "jsonrpc": "2.0",
                "id": hung_prompt["id"],
                "result": {"stopReason": "cancelled"},
            })
            hung_prompt = None
        # With IGNORE_CANCEL the hang continues past the driver's grace window.
    # Anything else (notifications) is ignored.
