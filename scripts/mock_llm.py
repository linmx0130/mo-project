#!/usr/bin/env python3
"""Tiny OpenAI-compatible mock LLM server for manual smoke tests.

Replays canned chat-completion SSE responses. Behavior is keyed on the
session prompt and a per-connection request counter (ThreadingHTTPServer
gives each worker its own thread/connection, so parent and subagent
workers each get their own sequence):

  * prompt contains "subagent" -> request 1 asks to spawn a subagent,
    request 2 gives the final answer
  * prompt contains "slow"     -> request 1 asks for `bash sleep 60`
                                 (use this to test cancel)
  * otherwise                  -> read_file greeting.txt, then
                                 bash wc -w greeting.txt, then final answer

Gateway session-title requests (a system prompt mentioning "short title")
get a bare title derived from the first user message, so the auto-title
flow works in smoke tests too.

Usage: python3 scripts/mock_llm.py [port]
"""
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 9001

state = threading.local()


def stream_text(text, words_per_chunk=3):
    """Split a text answer into several content deltas so the worker
    journals token-by-token `message_delta` events (the smoke-test path for
    streaming)."""
    words = text.split(" ")
    chunks = [
        " ".join(words[i : i + words_per_chunk])
        for i in range(0, len(words), words_per_chunk)
    ]
    return [{"content": c + (" " if i < len(chunks) - 1 else "")} for i, c in enumerate(chunks)]


def stream_reasoning(text, words_per_chunk=4):
    """Split a reasoning trace into several `reasoning_content` deltas so
    the worker journals reasoning token-by-token, like a reasoning model."""
    words = text.split(" ")
    chunks = [
        " ".join(words[i : i + words_per_chunk])
        for i in range(0, len(words), words_per_chunk)
    ]
    return [
        {"reasoning_content": c + (" " if i < len(chunks) - 1 else "")}
        for i, c in enumerate(chunks)
    ]


def sse(deltas, prompt_tokens=None, completion_tokens=8):
    payload = "".join(
        f"data: {json.dumps({'choices': [{'delta': d}]})}\n\n" for d in deltas
    )
    if prompt_tokens is not None:
        # The final chunk (empty choices) carries the whole call's usage,
        # like real OpenAI-compatible servers when the request sets
        # stream_options.include_usage.
        payload += (
            "data: "
            + json.dumps(
                {
                    "choices": [],
                    "usage": {
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": completion_tokens,
                        "total_tokens": prompt_tokens + completion_tokens,
                    },
                }
            )
            + "\n\n"
        )
    return (payload + "data: [DONE]\n\n").encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        n = getattr(state, "count", 0)
        state.count = n + 1
        user_msgs = [m for m in body.get("messages", []) if m.get("role") == "user"]
        prompt = user_msgs[0].get("content", "") if user_msgs else ""

        system_msgs = [m for m in body.get("messages", []) if m.get("role") == "system"]
        if any("short title" in m.get("content", "") for m in system_msgs):
            # Gateway session-title generation: reply with a bare title
            # derived from the first user message.
            deltas = [
                {"role": "assistant"},
                {"content": (prompt[:40] if prompt else "Untitled session")},
            ]
        elif "slow" in prompt:
            deltas = [
                {"role": "assistant"},
                {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_slow",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": '{"command": "sleep 60"}',
                            },
                        }
                    ]
                },
            ]
        elif "subagent" in prompt:
            if n == 0:
                deltas = [
                    {"role": "assistant"},
                    {
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "call_sub",
                                "type": "function",
                                "function": {
                                    "name": "spawn_subagent",
                                    "arguments": (
                                        '{"prompt": "Subagent task: report the '
                                        'word count of greeting.txt using bash."}'
                                    ),
                                },
                            }
                        ]
                    },
                ]
            else:
                deltas = (
                    [{"role": "assistant"}]
                    + stream_reasoning(
                        "The subagent should have counted the words; "
                        "I will report its result."
                    )
                    + stream_text(
                        "The subagent reported its result. Everything works."
                    )
                )
        elif n == 0:
            deltas = [
                {"role": "assistant"},
                {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": '{"path": "greeting.txt"}',
                            },
                        }
                    ]
                },
            ]
        elif n == 1:
            deltas = [
                {"role": "assistant"},
                {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_2",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": '{"command": "wc -w greeting.txt"}',
                            },
                        }
                    ]
                },
            ]
        else:
            # Final answer: streamed token-by-token, reasoning first then
            # content (message_delta events for both fields).
            deltas = (
                [{"role": "assistant"}]
                + stream_reasoning(
                    "I read the greeting and ran wc; "
                    "I will summarize the result."
                )
                + stream_text(
                    "Smoke test complete: read the greeting and counted "
                    "its words. All tools worked."
                )
            )

        # Emit a usage chunk only when the client asked for it
        # (stream_options.include_usage), like real providers. The worker
        # does, so smoke tests exercise the context_usage journal path; the
        # token count grows per request so the status bar visibly climbs.
        if (body.get("stream_options") or {}).get("include_usage") is True:
            prompt_tokens = getattr(state, "usage_tokens", 25)
            state.usage_tokens = prompt_tokens + 15
        else:
            prompt_tokens = None

        payload = sse(deltas, prompt_tokens=prompt_tokens)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    print(f"mock LLM listening on http://127.0.0.1:{PORT}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
