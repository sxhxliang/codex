#!/usr/bin/env python3
"""Local mock server for Codex ChatGPT account debugging.

This server provides the smallest HTTP surface needed to debug the Codex
ChatGPT account flow without talking to real OpenAI services.

Supported flows:
- Browser login via `/oauth/authorize` -> local Codex callback.
- Device code login via `/api/accounts/deviceauth/*` and `/codex/device`.
- Token exchange via `/oauth/token`.
- Token refresh via `/oauth/token` with `grant_type=refresh_token`.
- Responses API via `/backend-api/codex/responses`.
- Models API via `/backend-api/codex/models`.
- ChatGPT backend helpers via `/backend-api/wham/*`.
- Browser task pages via `/codex/tasks/<task_id>`.
- Codex Apps MCP via `/backend-api/wham/apps`.

The implementation uses only the Python standard library.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import html
import json
import secrets
import struct
import threading
import time
from http.cookies import SimpleCookie
from dataclasses import dataclass, field
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler
from http.server import ThreadingHTTPServer
from typing import Dict
from typing import Optional
from urllib.parse import parse_qs
from urllib.parse import urlencode
from urllib.parse import urlparse


WEBSOCKET_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def fake_jwt(payload: dict) -> str:
    header = {"alg": "none", "typ": "JWT"}
    return ".".join(
        (
            b64url(json.dumps(header, separators=(",", ":")).encode("utf-8")),
            b64url(json.dumps(payload, separators=(",", ":")).encode("utf-8")),
            b64url(b"signature"),
        )
    )


def unix_now() -> int:
    return int(time.time())


def iso8601_now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def truncate_text(value: str, limit: int) -> str:
    compact = " ".join(value.split())
    if len(compact) <= limit:
        return compact
    return compact[: limit - 1].rstrip() + "..."


def render_sse(events: list[dict]) -> bytes:
    chunks: list[str] = []
    for event in events:
        kind = event.get("type", "message")
        chunks.append(f"event: {kind}\n")
        chunks.append(f"data: {json.dumps(event, separators=(',', ':'))}\n\n")
    return "".join(chunks).encode("utf-8")


def websocket_accept_value(key: str) -> str:
    digest = hashlib.sha1(f"{key}{WEBSOCKET_GUID}".encode("ascii")).digest()
    return base64.b64encode(digest).decode("ascii")


def read_exact(rfile, size: int) -> bytes:
    chunks: list[bytes] = []
    remaining = size
    while remaining > 0:
        chunk = rfile.read(remaining)
        if not chunk:
            raise EOFError("websocket peer closed")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_websocket_frame(rfile) -> tuple[int, bytes]:
    header = read_exact(rfile, 2)

    first_byte, second_byte = header
    opcode = first_byte & 0x0F
    masked = bool(second_byte & 0x80)
    payload_len = second_byte & 0x7F

    if payload_len == 126:
        payload_len = struct.unpack("!H", read_exact(rfile, 2))[0]
    elif payload_len == 127:
        payload_len = struct.unpack("!Q", read_exact(rfile, 8))[0]

    masking_key = read_exact(rfile, 4) if masked else b""
    payload = read_exact(rfile, payload_len)

    if masked:
        payload = bytes(
            byte ^ masking_key[index % 4] for index, byte in enumerate(payload)
        )

    return opcode, payload


def write_websocket_frame(wfile, opcode: int, payload: bytes = b"") -> None:
    first_byte = 0x80 | (opcode & 0x0F)
    payload_len = len(payload)
    if payload_len < 126:
        header = bytes([first_byte, payload_len])
    elif payload_len < (1 << 16):
        header = bytes([first_byte, 126]) + struct.pack("!H", payload_len)
    else:
        header = bytes([first_byte, 127]) + struct.pack("!Q", payload_len)
    wfile.write(header)
    wfile.write(payload)
    wfile.flush()


def send_websocket_text(wfile, payload: dict) -> None:
    write_websocket_frame(
        wfile,
        0x1,
        json.dumps(payload, separators=(",", ":")).encode("utf-8"),
    )


def send_websocket_close(wfile) -> None:
    write_websocket_frame(wfile, 0x8)


def extract_text_fragments(value) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, dict):
        fragments: list[str] = []
        text = value.get("text")
        if isinstance(text, str):
            fragments.append(text)
        content = value.get("content")
        if isinstance(content, list):
            for item in content:
                fragments.extend(extract_text_fragments(item))
        input_items = value.get("input")
        if isinstance(input_items, list):
            for item in input_items:
                fragments.extend(extract_text_fragments(item))
        return fragments
    if isinstance(value, list):
        fragments: list[str] = []
        for item in value:
            fragments.extend(extract_text_fragments(item))
        return fragments
    return []


def extract_items_by_type(value, item_types: set[str]) -> list[dict]:
    if isinstance(value, dict):
        items: list[dict] = []
        if value.get("type") in item_types:
            items.append(value)
        content = value.get("content")
        if isinstance(content, list):
            for item in content:
                items.extend(extract_items_by_type(item, item_types))
        input_items = value.get("input")
        if isinstance(input_items, list):
            for item in input_items:
                items.extend(extract_items_by_type(item, item_types))
        return items
    if isinstance(value, list):
        items: list[dict] = []
        for item in value:
            items.extend(extract_items_by_type(item, item_types))
        return items
    return []


CONNECTOR_ID = "calendar"
CONNECTOR_NAME = "Calendar"
CONNECTOR_DESCRIPTION = "Plan events and manage your calendar."
DISCOVERABLE_CALENDAR_ID = "connector_2128aebfecb84f64a069897515042a44"
DISCOVERABLE_GMAIL_ID = "connector_68df038e0ba48191908c8434991bbac2"
MCP_PROTOCOL_VERSION = "2025-11-25"
MCP_SERVER_NAME = "mock-codex-apps"
MCP_SERVER_VERSION = "1.0.0"
CALENDAR_CREATE_EVENT_RESOURCE_URI = "connector://calendar/tools/calendar_create_event"
CALENDAR_LIST_EVENTS_RESOURCE_URI = "connector://calendar/tools/calendar_list_events"
APPLY_PATCH_APPROVAL_DEMO_CALL_ID = "call_mock_apply_patch_approval_demo"
APPLY_PATCH_APPROVAL_DEMO_FILE = "APPROVAL_DEMO.txt"
APPLY_PATCH_APPROVAL_DEMO_TRIGGER = "mock apply_patch approval demo"
APPLY_PATCH_APPROVAL_DEMO_FILE_CONTENT = (
    "hello from the mock apply_patch approval demo\n"
)


@dataclass
class LimitBucket:
    limit_id: str
    used_percent: int
    window_mins: int
    resets_in_secs: int
    limit_name: Optional[str] = None

    @classmethod
    def parse(cls, raw: str) -> "LimitBucket":
        parts = raw.split(":")
        if len(parts) not in (4, 5):
            raise argparse.ArgumentTypeError(
                "additional limit must be LIMIT_ID:USED_PERCENT:WINDOW_MINS:RESETS_IN_SECS[:LIMIT_NAME]"
            )
        limit_id, used, window_mins, resets_in_secs = parts[:4]
        limit_name = parts[4] if len(parts) == 5 else None
        try:
            return cls(
                limit_id=limit_id,
                used_percent=int(used),
                window_mins=int(window_mins),
                resets_in_secs=int(resets_in_secs),
                limit_name=limit_name,
            )
        except ValueError as exc:
            raise argparse.ArgumentTypeError(
                "additional limit numeric fields must be integers"
            ) from exc

    def as_usage_payload(self) -> dict:
        return {
            "limit_name": self.limit_name or self.limit_id,
            "metered_feature": self.limit_id,
            "rate_limit": {
                "allowed": True,
                "limit_reached": self.used_percent >= 100,
                "primary_window": {
                    "used_percent": self.used_percent,
                    "limit_window_seconds": self.window_mins * 60,
                    "reset_after_seconds": self.resets_in_secs,
                    "reset_at": unix_now() + self.resets_in_secs,
                },
            },
        }


@dataclass
class DeviceCodeRecord:
    device_auth_id: str
    user_code: str
    approved: bool = False
    polls: int = 0
    authorization_code: str = field(
        default_factory=lambda: f"device-code-{secrets.token_urlsafe(10)}"
    )
    code_challenge: str = field(
        default_factory=lambda: f"challenge-{secrets.token_urlsafe(8)}"
    )
    code_verifier: str = field(
        default_factory=lambda: f"verifier-{secrets.token_urlsafe(16)}"
    )


@dataclass
class TaskRecord:
    task_id: str
    title: str
    created_at: float
    updated_at: float
    current_turn_id: str
    user_turn_id: str
    assistant_turn_id: str
    user_prompt: str
    assistant_response: str
    archived: bool = False
    has_unread_turn: bool = False

    def as_task_response(self) -> dict:
        return {
            "id": self.task_id,
            "created_at": self.created_at,
            "title": self.title,
            "has_generated_title": True,
            "current_turn_id": self.current_turn_id,
            "has_unread_turn": self.has_unread_turn,
            "denormalized_metadata": None,
            "archived": self.archived,
            "external_pull_requests": [],
        }

    def as_list_item(self) -> dict:
        return {
            "id": self.task_id,
            "title": self.title,
            "has_generated_title": True,
            "updated_at": self.updated_at,
            "created_at": self.created_at,
            "task_status_display": {
                "status": "completed",
                "label": "Completed",
            },
            "archived": self.archived,
            "has_unread_turn": self.has_unread_turn,
            "pull_requests": [],
        }

    def as_turn_details(self) -> dict:
        return {
            "task": self.as_task_response(),
            "current_user_turn": {
                "id": self.user_turn_id,
                "attempt_placement": 0,
                "turn_status": "completed",
                "sibling_turn_ids": [],
                "input_items": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {
                                "content_type": "text",
                                "text": self.user_prompt,
                            }
                        ],
                    }
                ],
                "output_items": [],
                "worklog": {"messages": []},
            },
            "current_assistant_turn": {
                "id": self.assistant_turn_id,
                "attempt_placement": 0,
                "turn_status": "completed",
                "sibling_turn_ids": [],
                "input_items": [],
                "output_items": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "content_type": "text",
                                "text": self.assistant_response,
                            }
                        ],
                    }
                ],
                "worklog": {
                    "messages": [
                        {
                            "author": {"role": "assistant"},
                            "content": {
                                "parts": [
                                    {
                                        "content_type": "text",
                                        "text": self.assistant_response,
                                    }
                                ]
                            },
                        }
                    ]
                },
            },
            "current_diff_task_turn": None,
        }


@dataclass
class ServerState:
    args: argparse.Namespace
    device_codes: Dict[str, DeviceCodeRecord] = field(default_factory=dict)
    auth_codes: Dict[str, float] = field(default_factory=dict)
    browser_sessions: Dict[str, str] = field(default_factory=dict)
    tasks: Dict[str, TaskRecord] = field(default_factory=dict)
    lock: threading.Lock = field(default_factory=threading.Lock)

    def build_auth_claims(self) -> dict:
        return {
            "chatgpt_plan_type": self.args.plan_type,
            "chatgpt_user_id": self.args.chatgpt_user_id,
            "chatgpt_account_id": self.args.chatgpt_account_id,
            "organization_id": self.args.organization_id
            or self.args.chatgpt_account_id,
            "project_id": self.args.project_id,
            "completed_platform_onboarding": self.args.completed_platform_onboarding,
            "is_org_owner": self.args.is_org_owner,
        }

    def build_id_token(self) -> str:
        payload = {
            "email": self.args.email,
            "https://api.openai.com/profile": {
                "email": self.args.email,
            },
            "https://api.openai.com/auth": self.build_auth_claims(),
        }
        return fake_jwt(payload)

    def build_access_token(self) -> str:
        payload = {
            "sub": self.args.chatgpt_user_id,
            "jti": self.args.access_token,
            "https://api.openai.com/auth": self.build_auth_claims(),
        }
        return fake_jwt(payload)

    def build_models_response(self) -> dict:
        return {
            "models": [
                {
                    "slug": self.args.model_slug,
                    "display_name": self.args.model_display_name,
                    "description": self.args.model_description,
                    "default_reasoning_level": self.args.model_default_reasoning_level,
                    "supported_reasoning_levels": [
                        {
                            "effort": "low",
                            "description": "Fast responses with lighter reasoning",
                        },
                        {
                            "effort": "medium",
                            "description": "Balances speed and reasoning depth for everyday tasks",
                        },
                        {
                            "effort": "high",
                            "description": "Greater reasoning depth for complex problems",
                        },
                        {
                            "effort": "xhigh",
                            "description": "Extra high reasoning depth for complex problems",
                        },
                    ],
                    "shell_type": "shell_command",
                    "visibility": "list",
                    "supported_in_api": True,
                    "priority": self.args.model_priority,
                    "availability_nux": None,
                    "upgrade": None,
                    "base_instructions": (
                        "You are Codex, a coding agent running against the local "
                        "mock ChatGPT account server."
                    ),
                    "supports_reasoning_summaries": True,
                    "default_reasoning_summary": "auto",
                    "support_verbosity": True,
                    "default_verbosity": "low",
                    "apply_patch_tool_type": "freeform",
                    "web_search_tool_type": "text",
                    "truncation_policy": {
                        "mode": "tokens",
                        "limit": self.args.model_truncation_limit,
                    },
                    "supports_parallel_tool_calls": True,
                    "supports_image_detail_original": True,
                    "context_window": self.args.model_context_window,
                    "experimental_supported_tools": [],
                    "input_modalities": ["text", "image"],
                    "prefer_websockets": False,
                    "supports_search_tool": False,
                }
            ]
        }

    def create_auth_code(self, prefix: str) -> str:
        code = f"{prefix}-{secrets.token_urlsafe(12)}"
        with self.lock:
            self.auth_codes[code] = time.time()
        return code

    def create_browser_session(self, username: str) -> str:
        session_id = f"session-{secrets.token_urlsafe(18)}"
        with self.lock:
            self.browser_sessions[session_id] = username
        return session_id

    def has_browser_session(self, session_id: Optional[str]) -> bool:
        if not session_id:
            return False
        with self.lock:
            return session_id in self.browser_sessions

    def clear_browser_session(self, session_id: Optional[str]) -> None:
        if not session_id:
            return
        with self.lock:
            self.browser_sessions.pop(session_id, None)

    def mark_device_code_approved(self, user_code: str) -> bool:
        normalized = user_code.strip().upper()
        with self.lock:
            for record in self.device_codes.values():
                if record.user_code == normalized:
                    record.approved = True
                    return True
        return False

    def find_device_code(self, device_auth_id: str, user_code: str) -> Optional[DeviceCodeRecord]:
        normalized = user_code.strip().upper()
        with self.lock:
            record = self.device_codes.get(device_auth_id)
            if record is None or record.user_code != normalized:
                return None
            return record

    def response_text_for_prompt(self, prompt: str) -> str:
        template = self.args.responses_output_text
        return (
            template.replace("{prompt}", prompt)
            .replace("{model}", self.args.model_slug)
            .replace("{account_id}", self.args.chatgpt_account_id)
        )

    def title_for_prompt(self, prompt: str) -> str:
        if prompt.strip():
            return truncate_text(prompt, 72)
        return f"{self.args.task_title_prefix} {len(self.tasks) + 1}"

    def create_task(self, prompt: str) -> TaskRecord:
        timestamp = time.time()
        task_id = f"task_{secrets.token_hex(6)}"
        user_turn_id = f"turn_user_{secrets.token_hex(4)}"
        assistant_turn_id = f"turn_assistant_{secrets.token_hex(4)}"
        record = TaskRecord(
            task_id=task_id,
            title=self.title_for_prompt(prompt),
            created_at=timestamp,
            updated_at=timestamp,
            current_turn_id=assistant_turn_id,
            user_turn_id=user_turn_id,
            assistant_turn_id=assistant_turn_id,
            user_prompt=prompt or "Mock task prompt",
            assistant_response=self.response_text_for_prompt(prompt or "Mock task prompt"),
        )
        with self.lock:
            self.tasks[task_id] = record
        return record

    def list_tasks(self) -> list[TaskRecord]:
        with self.lock:
            return sorted(
                self.tasks.values(),
                key=lambda task: task.updated_at,
                reverse=True,
            )

    def get_task(self, task_id: str) -> Optional[TaskRecord]:
        with self.lock:
            return self.tasks.get(task_id)

    def requirements_response(self) -> dict:
        contents = self.args.requirements_contents
        return {
            "contents": contents,
            "sha256": hashlib.sha256(contents.encode("utf-8")).hexdigest(),
            "updated_at": iso8601_now(),
            "updated_by_user_id": self.args.chatgpt_user_id,
        }


class MockHandler(BaseHTTPRequestHandler):
    server: "MockServer"
    BROWSER_SESSION_COOKIE = "mock_codex_browser_session"
    protocol_version = "HTTP/1.1"

    def do_GET(self) -> None:
        parsed = urlparse(self.path)
        if parsed.path == "/healthz":
            self.respond_json({"ok": True})
            return
        if parsed.path in {"/backend-api/codex/responses", "/v1/responses"}:
            if self.is_websocket_upgrade():
                self.handle_responses_websocket()
                return
        if parsed.path in {"/models", "/v1/models", "/backend-api/codex/models"}:
            self.handle_models()
            return
        if parsed.path == "/.well-known/oauth-authorization-server/mcp":
            self.handle_mcp_oauth_metadata()
            return
        if parsed.path in {
            "/connectors/directory/list",
            "/backend-api/connectors/directory/list",
        }:
            self.handle_connectors_directory()
            return
        if parsed.path in {
            "/connectors/directory/list_workspace",
            "/backend-api/connectors/directory/list_workspace",
        }:
            self.handle_connectors_directory_workspace()
            return
        if parsed.path == "/oauth/authorize":
            self.handle_authorize(parsed)
            return
        if parsed.path == "/oauth/logout":
            self.handle_browser_logout(parsed)
            return
        if parsed.path == "/codex/device":
            self.render_device_page(parsed, message=None)
            return
        if parsed.path in {
            "/api/codex/usage",
            "/wham/usage",
            "/backend-api/wham/usage",
        }:
            self.handle_usage()
            return
        if parsed.path in {
            "/api/codex/config/requirements",
            "/wham/config/requirements",
            "/backend-api/wham/config/requirements",
        }:
            self.handle_config_requirements()
            return
        if parsed.path in {
            "/api/codex/tasks/list",
            "/wham/tasks/list",
            "/backend-api/wham/tasks/list",
        }:
            self.handle_task_list()
            return
        if parsed.path.startswith("/codex/tasks/"):
            self.handle_task_page(parsed)
            return
        if self.try_handle_task_details(parsed.path):
            return
        if parsed.path == "/deviceauth/callback":
            self.respond_html(
                HTTPStatus.OK,
                "<html><body><h1>Mock device callback reached</h1></body></html>",
            )
            return
        self.respond_json(
            {"error": f"unknown path: {parsed.path}"},
            status=HTTPStatus.NOT_FOUND,
        )

    def do_POST(self) -> None:
        parsed = urlparse(self.path)
        if parsed.path == "/oauth/login":
            self.handle_browser_login()
            return
        if parsed.path == "/oauth/token":
            self.handle_oauth_token()
            return
        if parsed.path in {"/backend-api/codex/responses", "/v1/responses"}:
            self.handle_responses()
            return
        if parsed.path == "/api/accounts/deviceauth/usercode":
            self.handle_device_usercode()
            return
        if parsed.path == "/api/accounts/deviceauth/token":
            self.handle_device_token()
            return
        if parsed.path == "/codex/device":
            self.handle_device_approval()
            return
        if parsed.path in {
            "/api/codex/tasks",
            "/wham/tasks",
            "/backend-api/wham/tasks",
        }:
            self.handle_task_create()
            return
        if parsed.path in {
            "/api/codex/apps",
            "/wham/apps",
            "/backend-api/wham/apps",
        }:
            self.handle_apps_json_rpc()
            return
        self.respond_json(
            {"error": f"unknown path: {parsed.path}"},
            status=HTTPStatus.NOT_FOUND,
        )

    def log_message(self, format: str, *args: object) -> None:
        print(f"[mock-account] {self.address_string()} - {format % args}")

    def is_websocket_upgrade(self) -> bool:
        upgrade = self.headers.get("Upgrade", "")
        connection = self.headers.get("Connection", "")
        return upgrade.lower() == "websocket" and "upgrade" in connection.lower()

    def read_body(self) -> bytes:
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length)

    def send_body(
        self,
        body: bytes,
        content_type: str,
        status: HTTPStatus = HTTPStatus.OK,
        extra_headers: Optional[list[tuple[str, str]]] = None,
    ) -> None:
        self.send_response(status.value)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        for key, value in extra_headers or []:
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(body)

    def respond_json(
        self,
        payload: dict,
        status: HTTPStatus = HTTPStatus.OK,
        extra_headers: Optional[list[tuple[str, str]]] = None,
    ) -> None:
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_body(
            body,
            "application/json",
            status=status,
            extra_headers=extra_headers,
        )

    def respond_html(
        self,
        status: HTTPStatus,
        html_body: str,
        extra_headers: Optional[list[tuple[str, str]]] = None,
    ) -> None:
        body = html_body.encode("utf-8")
        self.send_body(
            body,
            "text/html; charset=utf-8",
            status=status,
            extra_headers=extra_headers,
        )

    def redirect(
        self,
        location: str,
        extra_headers: Optional[list[tuple[str, str]]] = None,
    ) -> None:
        self.send_response(HTTPStatus.FOUND.value)
        self.send_header("Location", location)
        self.send_header("Content-Length", "0")
        for key, value in extra_headers or []:
            self.send_header(key, value)
        self.end_headers()

    def parse_form_body(self) -> dict[str, list[str]]:
        return parse_qs(self.read_body().decode("utf-8"))

    def parse_json_body(self) -> Optional[dict]:
        try:
            payload = json.loads(self.read_body().decode("utf-8"))
        except json.JSONDecodeError:
            self.respond_json(
                {"error": "invalid json body"},
                status=HTTPStatus.BAD_REQUEST,
            )
            return None
        if not isinstance(payload, dict):
            self.respond_json(
                {"error": "expected top-level JSON object"},
                status=HTTPStatus.BAD_REQUEST,
            )
            return None
        return payload

    def require_chatgpt_auth(self) -> bool:
        auth_header = self.headers.get("authorization")
        account_id = self.headers.get("chatgpt-account-id")
        if auth_header is None or not auth_header.startswith("Bearer "):
            self.respond_json(
                {"error": "missing bearer token"},
                status=HTTPStatus.UNAUTHORIZED,
            )
            return False
        if (
            self.server.state.args.strict_account_header
            and account_id != self.server.state.args.chatgpt_account_id
        ):
            self.respond_json(
                {"error": "chatgpt-account-id mismatch"},
                status=HTTPStatus.UNAUTHORIZED,
            )
            return False
        return True

    def send_sse(self, events: list[dict]) -> None:
        self.send_body(
            render_sse(events),
            "text/event-stream",
            extra_headers=[
                ("Cache-Control", "no-cache"),
                ("Connection", "close"),
            ],
        )

    @staticmethod
    def build_assistant_message_events(message_id: str, output_text: str) -> list[dict]:
        return [
            {
                "type": "response.output_item.added",
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "id": message_id,
                    "content": [
                        {
                            "type": "output_text",
                            "text": "",
                        }
                    ],
                },
            },
            {
                "type": "response.output_text.delta",
                "delta": output_text,
            },
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "id": message_id,
                    "content": [
                        {
                            "type": "output_text",
                            "text": output_text,
                        }
                    ],
                },
            },
        ]

    @staticmethod
    def approval_demo_patch() -> str:
        return (
            "*** Begin Patch\n"
            f"*** Add File: {APPLY_PATCH_APPROVAL_DEMO_FILE}\n"
            f"+{APPLY_PATCH_APPROVAL_DEMO_FILE_CONTENT.rstrip()}\n"
            "*** End Patch\n"
        )

    @staticmethod
    def is_apply_patch_approval_demo_prompt(prompt: str) -> bool:
        return APPLY_PATCH_APPROVAL_DEMO_TRIGGER in prompt.lower()

    def approval_demo_completed_text(self) -> str:
        return (
            "apply_patch approval demo completed: the client approved the "
            f"file change and created {APPLY_PATCH_APPROVAL_DEMO_FILE}."
        )

    def build_responses_events(self, payload: dict) -> list[dict]:
        prompt = "\n".join(extract_text_fragments(payload.get("input", []))).strip()
        if not prompt:
            prompt = payload.get("instructions", "").strip() or "Mock request"
        response_id = f"resp_{secrets.token_hex(6)}"
        print(f"Mocking response for prompt: {prompt}")

        events = [
            {
                "type": "response.created",
                "response": {"id": response_id},
            }
        ]

        function_call_outputs = extract_items_by_type(
            payload.get("input", []),
            {"function_call_output", "custom_tool_call_output"},
        )
        if any(
            item.get("call_id") == APPLY_PATCH_APPROVAL_DEMO_CALL_ID
            for item in function_call_outputs
        ):
            output_text = self.approval_demo_completed_text()
            token_count = len(prompt.split()) + len(output_text.split())
            message_id = f"msg_{secrets.token_hex(4)}"
            events.extend(self.build_assistant_message_events(message_id, output_text))
            events.append(
                {
                    "type": "response.completed",
                    "response": {
                        "id": response_id,
                        "output": [],
                        "usage": {
                            "input_tokens": max(1, len(prompt.split())),
                            "input_tokens_details": None,
                            "output_tokens": max(1, len(output_text.split())),
                            "output_tokens_details": None,
                            "total_tokens": max(2, token_count),
                        },
                    },
                }
            )
            return events

        if self.is_apply_patch_approval_demo_prompt(prompt):
            patch = self.approval_demo_patch()
            events.append(
                {
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "name": "apply_patch",
                        "arguments": json.dumps({"input": patch}, separators=(",", ":")),
                        "call_id": APPLY_PATCH_APPROVAL_DEMO_CALL_ID,
                    },
                }
            )
            token_count = len(prompt.split())
            events.append(
                {
                    "type": "response.completed",
                    "response": {
                        "id": response_id,
                        "output": [],
                        "usage": {
                            "input_tokens": max(1, len(prompt.split())),
                            "input_tokens_details": None,
                            "output_tokens": 1,
                            "output_tokens_details": None,
                            "total_tokens": max(2, token_count + 1),
                        },
                    },
                }
            )
            return events

        output_text = self.server.state.response_text_for_prompt(prompt)
        token_count = len(prompt.split()) + len(output_text.split())
        if payload.get("generate") is not False:
            message_id = f"msg_{secrets.token_hex(4)}"
            events.extend(self.build_assistant_message_events(message_id, output_text))

        events.append(
            {
                "type": "response.completed",
                "response": {
                    "id": response_id,
                    "output": [],
                    "usage": {
                        "input_tokens": max(1, len(prompt.split())),
                        "input_tokens_details": None,
                        "output_tokens": max(1, len(output_text.split())),
                        "output_tokens_details": None,
                        "total_tokens": max(2, token_count),
                    },
                },
            }
        )
        return events

    def browser_login_username(self) -> str:
        return self.server.state.args.login_username or self.server.state.args.email

    def browser_login_password(self) -> str:
        return self.server.state.args.login_password

    def browser_session_id(self) -> Optional[str]:
        raw_cookie = self.headers.get("Cookie")
        if not raw_cookie:
            return None
        cookie = SimpleCookie()
        cookie.load(raw_cookie)
        morsel = cookie.get(self.BROWSER_SESSION_COOKIE)
        if morsel is None:
            return None
        return morsel.value

    def is_browser_authenticated(self) -> bool:
        return self.server.state.has_browser_session(self.browser_session_id())

    @staticmethod
    def original_authorize_path(parsed) -> str:
        return parsed.path if not parsed.query else f"{parsed.path}?{parsed.query}"

    @staticmethod
    def normalize_continue_to(continue_to: str) -> str:
        if continue_to.startswith("/oauth/authorize"):
            return continue_to
        return "/oauth/authorize"

    @staticmethod
    def normalize_logout_continue_to(continue_to: str) -> str:
        if continue_to.startswith("/"):
            return continue_to
        return "/oauth/authorize"

    def handle_authorize(self, parsed) -> None:
        params = parse_qs(parsed.query)
        redirect_uri = params.get("redirect_uri", [None])[0]
        state = params.get("state", [""])[0]
        if not redirect_uri:
            self.respond_json(
                {"error": "redirect_uri is required"},
                status=HTTPStatus.BAD_REQUEST,
            )
            return
        if not self.is_browser_authenticated():
            self.render_browser_login_page(
                continue_to=self.original_authorize_path(parsed),
                error_message=None,
            )
            return
        code = self.server.state.create_auth_code("auth")
        query = urlencode({"code": code, "state": state})
        separator = "&" if "?" in redirect_uri else "?"
        self.redirect(f"{redirect_uri}{separator}{query}")

    def handle_browser_login(self) -> None:
        params = self.parse_form_body()
        username = params.get("username", [""])[0]
        password = params.get("password", [""])[0]
        continue_to = self.normalize_continue_to(params.get("continue_to", [""])[0])
        if (
            username != self.browser_login_username()
            or password != self.browser_login_password()
        ):
            self.render_browser_login_page(
                continue_to=continue_to,
                error_message="Invalid username or password.",
            )
            return

        session_id = self.server.state.create_browser_session(username)
        self.redirect(
            continue_to,
            extra_headers=[
                (
                    "Set-Cookie",
                    (
                        f"{self.BROWSER_SESSION_COOKIE}={session_id}; "
                        "HttpOnly; Path=/; SameSite=Lax"
                    ),
                )
            ],
        )

    def handle_browser_logout(self, parsed) -> None:
        params = parse_qs(parsed.query)
        continue_to = self.normalize_logout_continue_to(
            params.get("continue_to", ["/oauth/authorize"])[0]
        )
        self.server.state.clear_browser_session(self.browser_session_id())
        self.redirect(
            continue_to,
            extra_headers=[
                (
                    "Set-Cookie",
                    (
                        f"{self.BROWSER_SESSION_COOKIE}=; "
                        "Expires=Thu, 01 Jan 1970 00:00:00 GMT; "
                        "HttpOnly; Path=/; SameSite=Lax"
                    ),
                )
            ],
        )

    def handle_oauth_token(self) -> None:
        content_type = self.headers.get("Content-Type", "")
        body = self.read_body().decode("utf-8")
        if "application/x-www-form-urlencoded" in content_type:
            params = parse_qs(body)
        elif "application/json" in content_type:
            payload = json.loads(body or "{}")
            params = {
                key: [value]
                for key, value in payload.items()
                if isinstance(value, str)
            }
        else:
            self.respond_json(
                {"error": "expected application/json or application/x-www-form-urlencoded"},
                status=HTTPStatus.BAD_REQUEST,
            )
            return
        grant_type = params.get("grant_type", [""])[0]
        if grant_type == "authorization_code":
            self.respond_json(
                {
                    "id_token": self.server.state.build_id_token(),
                    "access_token": self.server.state.build_access_token(),
                    "refresh_token": self.server.state.args.refresh_token,
                }
            )
            return
        if grant_type == "refresh_token":
            refresh_token = params.get("refresh_token", [""])[0]
            if refresh_token != self.server.state.args.refresh_token:
                self.respond_json(
                    {
                        "error": {
                            "code": "refresh_token_invalidated",
                            "message": "refresh token is not recognized by the mock server",
                        }
                    },
                    status=HTTPStatus.UNAUTHORIZED,
                )
                return
            self.respond_json(
                {
                    "id_token": self.server.state.build_id_token(),
                    "access_token": self.server.state.build_access_token(),
                    "refresh_token": self.server.state.args.refresh_token,
                }
            )
            return
        if grant_type == "urn:ietf:params:oauth:grant-type:token-exchange":
            self.respond_json({"access_token": self.server.state.args.api_key})
            return
        self.respond_json(
            {"error": f"unsupported grant_type: {grant_type}"},
            status=HTTPStatus.BAD_REQUEST,
        )

    def handle_models(self) -> None:
        self.respond_json(
            self.server.state.build_models_response(),
            extra_headers=[("ETag", self.server.state.args.models_etag)],
        )

    def handle_responses(self) -> None:
        if not self.require_chatgpt_auth():
            return
        payload = self.parse_json_body()
        if payload is None:
            return
        self.send_sse(self.build_responses_events(payload))

    def handle_responses_websocket(self) -> None:
        if not self.require_chatgpt_auth():
            return

        websocket_key = self.headers.get("Sec-WebSocket-Key")
        if not websocket_key:
            self.respond_json(
                {"error": "missing Sec-WebSocket-Key"},
                status=HTTPStatus.BAD_REQUEST,
            )
            return

        self.close_connection = True
        self.send_response(HTTPStatus.SWITCHING_PROTOCOLS.value)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", websocket_accept_value(websocket_key))
        self.end_headers()

        while True:
            try:
                opcode, payload = read_websocket_frame(self.rfile)
            except EOFError:
                break

            if opcode == 0x8:
                send_websocket_close(self.wfile)
                break
            if opcode == 0x9:
                write_websocket_frame(self.wfile, 0xA, payload)
                continue
            if opcode != 0x1:
                send_websocket_text(
                    self.wfile,
                    {
                        "type": "error",
                        "status": 400,
                        "error": {
                            "type": "invalid_request_error",
                            "message": f"unsupported websocket opcode: {opcode}",
                        },
                    },
                )
                continue

            try:
                request_payload = json.loads(payload.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                send_websocket_text(
                    self.wfile,
                    {
                        "type": "error",
                        "status": 400,
                        "error": {
                            "type": "invalid_request_error",
                            "message": "invalid websocket JSON payload",
                        },
                    },
                )
                continue

            print(
                "Mock websocket request:",
                truncate_text(json.dumps(request_payload, separators=(",", ":")), 160),
            )
            if request_payload.get("type") != "response.create":
                send_websocket_text(
                    self.wfile,
                    {
                        "type": "error",
                        "status": 400,
                        "error": {
                            "type": "invalid_request_error",
                            "message": "expected websocket request type response.create",
                        },
                    },
                )
                continue

            for event in self.build_responses_events(request_payload):
                send_websocket_text(self.wfile, event)

    def handle_device_usercode(self) -> None:
        record = DeviceCodeRecord(
            device_auth_id=f"device-auth-{secrets.token_urlsafe(8)}",
            user_code=self.generate_user_code(),
        )
        with self.server.state.lock:
            self.server.state.device_codes[record.device_auth_id] = record
        self.respond_json(
            {
                "device_auth_id": record.device_auth_id,
                "user_code": record.user_code,
                "interval": str(self.server.state.args.device_code_interval_secs),
            }
        )

    def handle_device_token(self) -> None:
        try:
            payload = json.loads(self.read_body().decode("utf-8"))
        except json.JSONDecodeError:
            self.respond_json(
                {"error": "invalid json body"},
                status=HTTPStatus.BAD_REQUEST,
            )
            return
        device_auth_id = payload.get("device_auth_id", "")
        user_code = payload.get("user_code", "")
        record = self.server.state.find_device_code(device_auth_id, user_code)
        if record is None:
            self.respond_json(
                {"error": "unknown device code"},
                status=HTTPStatus.NOT_FOUND,
            )
            return

        with self.server.state.lock:
            record.polls += 1
            approved = record.approved or (
                self.server.state.args.device_code_auto_approve
                and record.polls > self.server.state.args.device_code_pending_polls
            )
            if approved:
                record.approved = True
                response = {
                    "authorization_code": record.authorization_code,
                    "code_challenge": record.code_challenge,
                    "code_verifier": record.code_verifier,
                }
            else:
                response = None

        if response is None:
            self.respond_json(
                {"status": "pending"},
                status=HTTPStatus.NOT_FOUND,
            )
            return
        self.respond_json(response)

    def handle_usage(self) -> None:
        if not self.require_chatgpt_auth():
            return

        body = {
            "plan_type": self.server.state.args.plan_type,
            "rate_limit": {
                "allowed": True,
                "limit_reached": self.server.state.args.primary_used_percent >= 100,
                "primary_window": {
                    "used_percent": self.server.state.args.primary_used_percent,
                    "limit_window_seconds": self.server.state.args.primary_window_mins * 60,
                    "reset_after_seconds": self.server.state.args.primary_resets_in_secs,
                    "reset_at": unix_now() + self.server.state.args.primary_resets_in_secs,
                },
                "secondary_window": None,
            },
            "additional_rate_limits": [
                bucket.as_usage_payload()
                for bucket in self.server.state.args.additional_limit
            ],
        }
        if self.server.state.args.secondary_used_percent is not None:
            body["rate_limit"]["secondary_window"] = {
                "used_percent": self.server.state.args.secondary_used_percent,
                "limit_window_seconds": self.server.state.args.secondary_window_mins * 60,
                "reset_after_seconds": self.server.state.args.secondary_resets_in_secs,
                "reset_at": unix_now() + self.server.state.args.secondary_resets_in_secs,
            }
        self.respond_json(body)

    def handle_config_requirements(self) -> None:
        if not self.require_chatgpt_auth():
            return
        self.respond_json(self.server.state.requirements_response())

    def handle_task_list(self) -> None:
        if not self.require_chatgpt_auth():
            return
        self.respond_json(
            {
                "items": [task.as_list_item() for task in self.server.state.list_tasks()],
                "cursor": None,
            }
        )

    def handle_task_create(self) -> None:
        if not self.require_chatgpt_auth():
            return
        payload = self.parse_json_body()
        if payload is None:
            return
        prompt = "\n".join(extract_text_fragments(payload)).strip()
        if not prompt:
            for key in ("prompt", "title", "instructions"):
                value = payload.get(key)
                if isinstance(value, str) and value.strip():
                    prompt = value.strip()
                    break
        task = self.server.state.create_task(prompt)
        self.respond_json({"task": {"id": task.task_id}})

    def try_handle_task_details(self, path: str) -> bool:
        prefixes = [
            "/api/codex/tasks/",
            "/wham/tasks/",
            "/backend-api/wham/tasks/",
        ]
        for prefix in prefixes:
            if not path.startswith(prefix):
                continue
            remainder = path[len(prefix) :]
            parts = [part for part in remainder.split("/") if part]
            if len(parts) == 1:
                self.handle_task_details(parts[0])
                return True
            if len(parts) == 4 and parts[1] == "turns" and parts[3] == "sibling_turns":
                self.handle_sibling_turns(parts[0], parts[2])
                return True
        return False

    def handle_task_details(self, task_id: str) -> None:
        if not self.require_chatgpt_auth():
            return
        task = self.server.state.get_task(task_id)
        if task is None:
            self.respond_json(
                {"error": f"unknown task: {task_id}"},
                status=HTTPStatus.NOT_FOUND,
            )
            return
        self.respond_json(task.as_turn_details())

    def handle_sibling_turns(self, task_id: str, turn_id: str) -> None:
        if not self.require_chatgpt_auth():
            return
        task = self.server.state.get_task(task_id)
        if task is None:
            self.respond_json(
                {"error": f"unknown task: {task_id}"},
                status=HTTPStatus.NOT_FOUND,
            )
            return
        self.respond_json(
            {
                "sibling_turns": [
                    {
                        "id": turn_id,
                        "task_id": task.task_id,
                        "attempt_placement": 0,
                    }
                ]
            }
        )

    def handle_task_page(self, parsed) -> None:
        task_id = parsed.path.rsplit("/", 1)[-1]
        task = self.server.state.get_task(task_id)
        if task is None:
            self.respond_html(
                HTTPStatus.NOT_FOUND,
                f"<html><body><h1>Unknown task</h1><p>{html.escape(task_id)}</p></body></html>",
            )
            return
        body = f"""<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <title>{html.escape(task.title)}</title>
  <style>
    body {{ font-family: sans-serif; margin: 0; background: #f5f7fb; color: #111827; }}
    .shell {{ max-width: 820px; margin: 4rem auto; background: white; border: 1px solid #d1d5db; border-radius: 16px; padding: 2rem; box-shadow: 0 12px 30px rgba(0, 0, 0, 0.08); }}
    pre {{ background: #f8fafc; border: 1px solid #e5e7eb; border-radius: 12px; padding: 1rem; white-space: pre-wrap; }}
    .meta {{ color: #6b7280; font-size: 0.95rem; }}
  </style>
</head>
<body>
  <div class="shell">
    <h1>{html.escape(task.title)}</h1>
    <p class="meta">Task ID: <code>{html.escape(task.task_id)}</code></p>
    <h2>User prompt</h2>
    <pre>{html.escape(task.user_prompt)}</pre>
    <h2>Assistant response</h2>
    <pre>{html.escape(task.assistant_response)}</pre>
  </div>
</body>
</html>"""
        self.respond_html(HTTPStatus.OK, body)

    def handle_mcp_oauth_metadata(self) -> None:
        origin = f"http://{self.server.server_address[0]}:{self.server.server_address[1]}"
        self.respond_json(
            {
                "authorization_endpoint": f"{origin}/oauth/authorize",
                "token_endpoint": f"{origin}/oauth/token",
                "scopes_supported": [""],
            }
        )

    def handle_connectors_directory(self) -> None:
        self.respond_json(
            {
                "apps": [
                    {
                        "id": DISCOVERABLE_CALENDAR_ID,
                        "name": "Google Calendar",
                        "description": "Plan events and schedules.",
                    },
                    {
                        "id": DISCOVERABLE_GMAIL_ID,
                        "name": "Gmail",
                        "description": "Find and summarize email threads.",
                    },
                ],
                "nextToken": None,
            }
        )

    def handle_connectors_directory_workspace(self) -> None:
        self.respond_json({"apps": [], "nextToken": None})

    def handle_apps_json_rpc(self) -> None:
        if not self.require_chatgpt_auth():
            return
        payload = self.parse_json_body()
        if payload is None:
            return
        method = payload.get("method")
        request_id = payload.get("id")
        if not isinstance(method, str):
            self.respond_json(
                {"error": "missing method in JSON-RPC request"},
                status=HTTPStatus.BAD_REQUEST,
            )
            return

        if method == "initialize":
            params = payload.get("params", {})
            protocol_version = (
                params.get("protocolVersion")
                if isinstance(params, dict)
                else None
            ) or MCP_PROTOCOL_VERSION
            self.respond_json(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "protocolVersion": protocol_version,
                        "capabilities": {"tools": {"listChanged": True}},
                        "serverInfo": {
                            "name": MCP_SERVER_NAME,
                            "version": MCP_SERVER_VERSION,
                        },
                    },
                }
            )
            return

        if method == "notifications/initialized" or method.startswith("notifications/"):
            self.send_response(HTTPStatus.ACCEPTED.value)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        if method == "tools/list":
            self.respond_json(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "tools": [
                            {
                                "name": "calendar_create_event",
                                "description": "Create a calendar event.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "title": {"type": "string"},
                                        "starts_at": {"type": "string"},
                                        "timezone": {"type": "string"},
                                    },
                                    "required": ["title", "starts_at"],
                                    "additionalProperties": False,
                                },
                                "_meta": {
                                    "connector_id": CONNECTOR_ID,
                                    "connector_name": CONNECTOR_NAME,
                                    "connector_description": CONNECTOR_DESCRIPTION,
                                    "_codex_apps": {
                                        "resource_uri": CALENDAR_CREATE_EVENT_RESOURCE_URI,
                                        "contains_mcp_source": True,
                                        "connector_id": CONNECTOR_ID,
                                    },
                                },
                            },
                            {
                                "name": "calendar_list_events",
                                "description": "List calendar events.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "query": {"type": "string"},
                                        "limit": {"type": "integer"},
                                    },
                                    "additionalProperties": False,
                                },
                                "_meta": {
                                    "connector_id": CONNECTOR_ID,
                                    "connector_name": CONNECTOR_NAME,
                                    "connector_description": CONNECTOR_DESCRIPTION,
                                    "_codex_apps": {
                                        "resource_uri": CALENDAR_LIST_EVENTS_RESOURCE_URI,
                                        "contains_mcp_source": True,
                                        "connector_id": CONNECTOR_ID,
                                    },
                                },
                            },
                        ],
                        "nextCursor": None,
                    },
                }
            )
            return

        if method == "tools/call":
            params = payload.get("params", {})
            arguments = params.get("arguments", {}) if isinstance(params, dict) else {}
            codex_apps_meta = (
                params.get("_meta", {}).get("_codex_apps")
                if isinstance(params, dict)
                and isinstance(params.get("_meta"), dict)
                else None
            )
            tool_name = params.get("name", "") if isinstance(params, dict) else ""
            title = arguments.get("title", "") if isinstance(arguments, dict) else ""
            starts_at = arguments.get("starts_at", "") if isinstance(arguments, dict) else ""
            self.respond_json(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "content": [
                            {
                                "type": "text",
                                "text": f"called {tool_name} for {title} at {starts_at}",
                            }
                        ],
                        "structuredContent": {
                            "_codex_apps": codex_apps_meta,
                        },
                        "isError": False,
                    },
                }
            )
            return

        self.respond_json(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {
                    "code": -32601,
                    "message": f"method not found: {method}",
                },
            }
        )

    def handle_device_approval(self) -> None:
        params = self.parse_form_body()
        user_code = params.get("user_code", [""])[0]
        message = "Approved device code." if self.server.state.mark_device_code_approved(user_code) else "Device code not found."
        self.render_device_page(urlparse(self.path), message=message)

    def render_browser_login_page(
        self, continue_to: str, error_message: Optional[str]
    ) -> None:
        username = html.escape(self.browser_login_username())
        password_hint = html.escape(self.browser_login_password())
        continue_to = html.escape(continue_to, quote=True)
        error_html = (
            f"<p class='error'>{html.escape(error_message)}</p>"
            if error_message
            else ""
        )
        body = f"""<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <title>Mock Codex Browser Login</title>
  <style>
    body {{ font-family: sans-serif; margin: 0; background: #f5f7fb; color: #111827; }}
    .shell {{ max-width: 420px; margin: 8vh auto; background: white; border: 1px solid #d1d5db; border-radius: 16px; padding: 2rem; box-shadow: 0 12px 30px rgba(0, 0, 0, 0.08); }}
    h1 {{ margin: 0 0 0.75rem; font-size: 1.5rem; }}
    p {{ line-height: 1.5; }}
    .hint {{ color: #4b5563; font-size: 0.95rem; }}
    .error {{ color: #b91c1c; background: #fef2f2; border: 1px solid #fecaca; padding: 0.75rem; border-radius: 10px; }}
    form {{ display: grid; gap: 0.85rem; margin-top: 1.25rem; }}
    label {{ display: grid; gap: 0.35rem; font-weight: 600; }}
    input {{ padding: 0.7rem 0.8rem; border: 1px solid #cbd5e1; border-radius: 10px; font: inherit; }}
    button {{ margin-top: 0.25rem; padding: 0.8rem 1rem; border: 0; border-radius: 999px; background: #111827; color: white; font: inherit; cursor: pointer; }}
    code {{ background: #f3f4f6; padding: 0.1rem 0.35rem; border-radius: 6px; }}
  </style>
</head>
<body>
  <div class="shell">
    <h1>Sign in to Mock Codex</h1>
    <p class="hint">This mock issuer now requires a browser login before it will complete <code>/oauth/authorize</code>.</p>
    <p class="hint">Configured credentials: <code>{username}</code> / <code>{password_hint}</code></p>
    {error_html}
    <form method="post" action="/oauth/login">
      <input type="hidden" name="continue_to" value="{continue_to}" />
      <label>
        Username
        <input name="username" autocomplete="username" />
      </label>
      <label>
        Password
        <input name="password" type="password" autocomplete="current-password" />
      </label>
      <button type="submit">Continue</button>
    </form>
  </div>
</body>
</html>"""
        self.respond_html(HTTPStatus.OK, body)

    def render_device_page(self, parsed, message: Optional[str]) -> None:
        with self.server.state.lock:
            records = list(self.server.state.device_codes.values())
        rows = []
        for record in records:
            rows.append(
                "<tr>"
                f"<td><code>{html.escape(record.user_code)}</code></td>"
                f"<td>{'yes' if record.approved else 'no'}</td>"
                f"<td>{record.polls}</td>"
                "</tr>"
            )
        table_rows = "".join(rows) or "<tr><td colspan='3'>No active device codes yet.</td></tr>"
        flash = f"<p><strong>{html.escape(message)}</strong></p>" if message else ""
        body = f"""<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <title>Mock Codex Device Auth</title>
  <style>
    body {{ font-family: sans-serif; margin: 2rem auto; max-width: 48rem; }}
    code {{ background: #f4f4f4; padding: 0.2rem 0.35rem; }}
    table {{ border-collapse: collapse; width: 100%; margin-top: 1rem; }}
    td, th {{ border: 1px solid #ddd; padding: 0.5rem; text-align: left; }}
    form {{ display: flex; gap: 0.5rem; margin-top: 1rem; }}
    input {{ flex: 1; padding: 0.5rem; }}
    button {{ padding: 0.5rem 0.8rem; }}
  </style>
</head>
<body>
  <h1>Mock Codex Device Auth</h1>
  <p>Open this page after <code>codex login --device-auth</code> prints a user code.</p>
  {flash}
  <form method="post" action="/codex/device">
    <input name="user_code" placeholder="Enter the printed user code" />
    <button type="submit">Approve</button>
  </form>
  <table>
    <thead>
      <tr><th>User code</th><th>Approved</th><th>Polls</th></tr>
    </thead>
    <tbody>{table_rows}</tbody>
  </table>
</body>
</html>"""
        self.respond_html(HTTPStatus.OK, body)

    @staticmethod
    def generate_user_code() -> str:
        return f"{secrets.token_hex(2).upper()}-{secrets.token_hex(2).upper()}"


class MockServer(ThreadingHTTPServer):
    def __init__(self, server_address, handler_cls, state: ServerState) -> None:
        super().__init__(server_address, handler_cls)
        self.state = state


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Local mock server for Codex ChatGPT account flows."
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--email", default="debug@example.com")
    parser.add_argument(
        "--login-username",
        help="Browser login username. Defaults to --email when omitted.",
    )
    parser.add_argument(
        "--login-password",
        default="debug-password",
        help="Browser login password used by the mock /oauth/authorize flow.",
    )
    parser.add_argument("--plan-type", default="pro")
    parser.add_argument("--chatgpt-account-id", default="org-debug")
    parser.add_argument("--chatgpt-user-id", default="user-debug")
    parser.add_argument("--organization-id")
    parser.add_argument("--project-id", default="")
    parser.add_argument(
        "--completed-platform-onboarding",
        action="store_true",
        default=True,
        help="Emit completed_platform_onboarding=true in the mock JWT auth claims.",
    )
    parser.add_argument(
        "--no-completed-platform-onboarding",
        dest="completed_platform_onboarding",
        action="store_false",
        help="Emit completed_platform_onboarding=false in the mock JWT auth claims.",
    )
    parser.add_argument(
        "--is-org-owner",
        action="store_true",
        help="Emit is_org_owner=true in the mock JWT auth claims.",
    )
    parser.add_argument(
        "--access-token",
        default="mock-chatgpt-access-token",
        help="Opaque identifier stored in the mock JWT access token jti claim.",
    )
    parser.add_argument("--refresh-token", default="mock-chatgpt-refresh-token")
    parser.add_argument("--api-key", default="sk-mock-api-key")
    parser.add_argument(
        "--responses-output-text",
        default="Mock response from {model} for: {prompt}",
        help="Template used by /backend-api/codex/responses and generated task details.",
    )
    parser.add_argument(
        "--requirements-contents",
        default=(
            "# mock requirements\n"
            "[network]\n"
            "allowed_domains = [\"api.openai.com\", \"chatgpt.com\"]\n"
        ),
        help="Contents returned by the mock requirements endpoint.",
    )
    parser.add_argument(
        "--task-title-prefix",
        default="Mock task",
        help="Fallback prefix used when generated task titles have no prompt text.",
    )
    parser.add_argument("--model-slug", default="gpt-5.3-codex")
    parser.add_argument("--model-display-name", default="gpt-5.3-codex")
    parser.add_argument(
        "--model-description",
        default="Mock remote model served by the local ChatGPT account server.",
    )
    parser.add_argument("--model-default-reasoning-level", default="medium")
    parser.add_argument("--model-priority", type=int, default=0)
    parser.add_argument("--model-context-window", type=int, default=272000)
    parser.add_argument("--model-truncation-limit", type=int, default=10000)
    parser.add_argument("--models-etag", default="mock-models-etag-v1")
    parser.add_argument("--primary-used-percent", type=int, default=42)
    parser.add_argument("--primary-window-mins", type=int, default=60)
    parser.add_argument("--primary-resets-in-secs", type=int, default=120)
    parser.add_argument("--secondary-used-percent", type=int, default=5)
    parser.add_argument("--secondary-window-mins", type=int, default=1440)
    parser.add_argument("--secondary-resets-in-secs", type=int, default=43200)
    parser.add_argument(
        "--additional-limit",
        action="append",
        type=LimitBucket.parse,
        default=[],
        help="LIMIT_ID:USED_PERCENT:WINDOW_MINS:RESETS_IN_SECS[:LIMIT_NAME]",
    )
    parser.add_argument("--device-code-interval-secs", type=int, default=1)
    parser.add_argument("--device-code-pending-polls", type=int, default=1)
    parser.add_argument(
        "--device-code-auto-approve",
        action="store_true",
        help="Automatically approve device codes after the pending poll threshold.",
    )
    parser.add_argument(
        "--strict-account-header",
        action="store_true",
        help="Require `chatgpt-account-id` to match the configured account id on /api/codex/usage.",
    )
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    state = ServerState(args=args)
    server = MockServer((args.host, args.port), MockHandler, state)
    login_username = args.login_username or args.email

    print(f"Mock account server listening on http://{args.host}:{args.port}")
    print(f"OAuth issuer: http://{args.host}:{args.port}")
    print(f"Browser login: {login_username} / {args.login_password}")
    print(
        f"Models endpoints: http://{args.host}:{args.port}/models, /v1/models, and /backend-api/codex/models"
    )
    print(f"Responses endpoint: http://{args.host}:{args.port}/backend-api/codex/responses")
    print(f"ChatGPT backend base URL: http://{args.host}:{args.port}/backend-api")
    print(f"Device auth page: http://{args.host}:{args.port}/codex/device")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down mock account server.")
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
