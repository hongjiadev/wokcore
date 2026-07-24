"""Reproducible independent protobuf/Connect verification for Cursor fixtures."""

from __future__ import annotations

import hashlib
from pathlib import Path

import google.protobuf
from google.protobuf import descriptor_pb2, descriptor_pool, message_factory


PINNED_PROTOBUF_VERSION = "6.33.5"
FIXTURE_SHA256 = {
    "request/run.connect.hex": "602e78062b9e339314ecc5ba8ec38695f82fa6e3311a9fd378e64f453dc6d42e",
    "stream/tool.connect.hex": "b9fc35502595b7caa4343d4ddfba05684b89ba3473a4475f26f0a1eca302f592",
    "stream/exec.connect.hex": "68a9bb76c85d22d98ff0f36af18b6c619cb179badcd42623a122ec9e621e03e5",
}

TYPE = descriptor_pb2.FieldDescriptorProto
OPTIONAL = TYPE.LABEL_OPTIONAL
REPEATED = TYPE.LABEL_REPEATED
STRING = TYPE.TYPE_STRING
BYTES = TYPE.TYPE_BYTES
INT32 = TYPE.TYPE_INT32
UINT32 = TYPE.TYPE_UINT32
MESSAGE = TYPE.TYPE_MESSAGE


def add_message(file_descriptor, name):
    message = file_descriptor.message_type.add()
    message.name = name
    return message


def add_oneof(message, name):
    oneof = message.oneof_decl.add()
    oneof.name = name
    return len(message.oneof_decl) - 1


def add_field(
    message,
    name,
    number,
    field_type,
    *,
    type_name=None,
    label=OPTIONAL,
    oneof=None,
):
    field = message.field.add()
    field.name = name
    field.number = number
    field.type = field_type
    field.label = label
    if type_name is not None:
        field.type_name = f".agent.v1.{type_name}"
    if oneof is not None:
        field.oneof_index = oneof


def build_schema():
    descriptor = descriptor_pb2.FileDescriptorProto(
        name="cursor_fixture_probe.proto",
        package="agent.v1",
        syntax="proto3",
    )

    client = add_message(descriptor, "AgentClientMessage")
    client_message = add_oneof(client, "message")
    add_field(
        client,
        "run_request",
        1,
        MESSAGE,
        type_name="AgentRunRequest",
        oneof=client_message,
    )
    run = add_message(descriptor, "AgentRunRequest")
    add_field(run, "conversation_state", 1, MESSAGE, type_name="ConversationStateStructure")
    add_field(run, "action", 2, MESSAGE, type_name="ConversationAction")
    add_field(run, "model_details", 3, MESSAGE, type_name="ModelDetails")
    add_field(run, "mcp_tools", 4, MESSAGE, type_name="McpTools")
    add_field(run, "conversation_id", 5, STRING)

    action = add_message(descriptor, "ConversationAction")
    action_kind = add_oneof(action, "action")
    add_field(
        action,
        "user_message_action",
        1,
        MESSAGE,
        type_name="UserMessageAction",
        oneof=action_kind,
    )
    user_action = add_message(descriptor, "UserMessageAction")
    add_field(user_action, "user_message", 1, MESSAGE, type_name="UserMessage")
    add_field(user_action, "request_context", 2, MESSAGE, type_name="RequestContext")
    user_message = add_message(descriptor, "UserMessage")
    add_field(user_message, "text", 1, STRING)
    add_field(user_message, "message_id", 2, STRING)
    add_message(descriptor, "RequestContext")

    model = add_message(descriptor, "ModelDetails")
    add_field(model, "model_id", 1, STRING)
    add_field(model, "display_model_id", 3, STRING)
    add_field(model, "display_name", 4, STRING)
    add_field(model, "display_name_short", 5, STRING)
    tools = add_message(descriptor, "McpTools")
    add_field(
        tools,
        "mcp_tools",
        1,
        MESSAGE,
        type_name="McpToolDefinition",
        label=REPEATED,
    )
    tool_definition = add_message(descriptor, "McpToolDefinition")
    add_field(tool_definition, "name", 1, STRING)
    add_field(tool_definition, "description", 2, STRING)
    add_field(tool_definition, "input_schema", 3, BYTES)
    add_field(tool_definition, "provider_identifier", 4, STRING)
    add_field(tool_definition, "tool_name", 5, STRING)

    server = add_message(descriptor, "AgentServerMessage")
    server_message = add_oneof(server, "message")
    for name, number, type_name in [
        ("interaction_update", 1, "InteractionUpdate"),
        ("exec_server_message", 2, "ExecServerMessage"),
        ("conversation_checkpoint_update", 3, "ConversationStateStructure"),
    ]:
        add_field(
            server,
            name,
            number,
            MESSAGE,
            type_name=type_name,
            oneof=server_message,
        )
    add_message(descriptor, "ExecServerMessage")
    state = add_message(descriptor, "ConversationStateStructure")
    add_field(state, "token_details", 5, MESSAGE, type_name="ConversationTokenDetails")
    token_details = add_message(descriptor, "ConversationTokenDetails")
    add_field(token_details, "used_tokens", 1, UINT32)

    interaction = add_message(descriptor, "InteractionUpdate")
    interaction_message = add_oneof(interaction, "message")
    for name, number, type_name in [
        ("text_delta", 1, "TextDeltaUpdate"),
        ("tool_call_started", 2, "ToolCallStartedUpdate"),
        ("tool_call_completed", 3, "ToolCallCompletedUpdate"),
        ("thinking_delta", 4, "ThinkingDeltaUpdate"),
        ("partial_tool_call", 7, "PartialToolCallUpdate"),
        ("token_delta", 8, "TokenDeltaUpdate"),
        ("turn_ended", 14, "TurnEndedUpdate"),
    ]:
        add_field(
            interaction,
            name,
            number,
            MESSAGE,
            type_name=type_name,
            oneof=interaction_message,
        )
    for name in ["TextDeltaUpdate", "ThinkingDeltaUpdate"]:
        message = add_message(descriptor, name)
        add_field(message, "text", 1, STRING)
    started = add_message(descriptor, "ToolCallStartedUpdate")
    add_field(started, "call_id", 1, STRING)
    add_field(started, "tool_call", 2, MESSAGE, type_name="ToolCall")
    add_field(started, "model_call_id", 3, STRING)
    partial = add_message(descriptor, "PartialToolCallUpdate")
    add_field(partial, "call_id", 1, STRING)
    add_field(partial, "tool_call", 2, MESSAGE, type_name="ToolCall")
    add_field(partial, "args_text_delta", 3, STRING)
    add_field(partial, "model_call_id", 4, STRING)
    completed = add_message(descriptor, "ToolCallCompletedUpdate")
    add_field(completed, "call_id", 1, STRING)
    add_field(completed, "tool_call", 2, MESSAGE, type_name="ToolCall")

    tool_call = add_message(descriptor, "ToolCall")
    tool_kind = add_oneof(tool_call, "tool")
    add_field(
        tool_call,
        "mcp_tool_call",
        15,
        MESSAGE,
        type_name="McpToolCall",
        oneof=tool_kind,
    )
    mcp_call = add_message(descriptor, "McpToolCall")
    add_field(mcp_call, "args", 1, MESSAGE, type_name="McpArgs")
    mcp_args = add_message(descriptor, "McpArgs")
    add_field(mcp_args, "name", 1, STRING)
    add_field(mcp_args, "tool_call_id", 3, STRING)
    add_field(mcp_args, "provider_identifier", 4, STRING)
    add_field(mcp_args, "tool_name", 5, STRING)
    token = add_message(descriptor, "TokenDeltaUpdate")
    add_field(token, "tokens", 1, INT32)
    add_message(descriptor, "TurnEndedUpdate")

    pool = descriptor_pool.DescriptorPool()
    pool.Add(descriptor)

    def message_class(name):
        return message_factory.GetMessageClass(
            pool.FindMessageTypeByName(f"agent.v1.{name}")
        )

    return message_class("AgentClientMessage"), message_class("AgentServerMessage")


def load_hex(path):
    source = path.read_text(encoding="utf-8")
    tokens = []
    for line in source.splitlines():
        tokens.extend(line.split("#", 1)[0].split())
    return bytes.fromhex("".join(tokens))


def connect_frames(data):
    offset = 0
    frames = []
    while offset < len(data):
        assert len(data) - offset >= 5
        flags = data[offset]
        size = int.from_bytes(data[offset + 1 : offset + 5], "big")
        end = offset + 5 + size
        assert end <= len(data)
        frames.append((flags, data[offset + 5 : end]))
        offset = end
    return frames


def verify_request(root, Client):
    [(flags, payload)] = connect_frames(load_hex(root / "request/run.connect.hex"))
    assert flags == 0
    parsed = Client.FromString(payload)
    assert parsed.WhichOneof("message") == "run_request"
    run = parsed.run_request
    assert run.action.user_message_action.user_message.text == "hi"
    assert run.action.user_message_action.user_message.message_id == "req_wire"
    assert run.model_details.model_id == "composer-2.5"
    assert run.conversation_id == "thread_wire"
    assert parsed.SerializeToString(deterministic=True) == payload

    independently_encoded = Client()
    independent_run = independently_encoded.run_request
    independent_run.conversation_state.SetInParent()
    user_action = independent_run.action.user_message_action
    user_action.user_message.text = "hi"
    user_action.user_message.message_id = "req_wire"
    user_action.request_context.SetInParent()
    for field in ["model_id", "display_model_id", "display_name", "display_name_short"]:
        setattr(independent_run.model_details, field, "composer-2.5")
    independent_run.conversation_id = "thread_wire"
    assert independently_encoded.SerializeToString(deterministic=True) == payload
    return f"request=run_request bytes={len(payload)} model={run.model_details.model_id}"


def response_summary(message):
    kind = message.WhichOneof("message")
    if kind == "exec_server_message":
        return "exec_server_message"
    assert kind == "interaction_update"
    update = message.interaction_update
    update_kind = update.WhichOneof("message")
    value = getattr(update, update_kind)
    if update_kind in {"text_delta", "thinking_delta"}:
        return f'{update_kind}("{value.text}")'
    if update_kind == "tool_call_started":
        args = value.tool_call.mcp_tool_call.args
        return f"tool_call_started({value.call_id},{args.tool_name})"
    if update_kind == "partial_tool_call":
        return f"partial_tool_call({value.args_text_delta})"
    if update_kind == "tool_call_completed":
        return f"tool_call_completed({value.call_id})"
    if update_kind == "token_delta":
        return f"token_delta({value.tokens})"
    assert update_kind == "turn_ended"
    return "turn_ended"


def verify_responses(root, Server):
    frames = connect_frames(load_hex(root / "stream/tool.connect.hex"))
    summaries = []
    for flags, payload in frames[:-1]:
        assert flags == 0
        parsed = Server.FromString(payload)
        assert parsed.SerializeToString(deterministic=True) == payload
        summaries.append(response_summary(parsed))
    assert frames[-1] == (2, b"{}")
    assert summaries == [
        'text_delta("Hello")',
        'thinking_delta("Checking.")',
        "tool_call_started(call_cursor,weather)",
        'partial_tool_call({"city":)',
        'partial_tool_call({"city":"Paris"})',
        "tool_call_completed(call_cursor)",
        "token_delta(3)",
        "turn_ended",
    ]

    [(flags, payload)] = connect_frames(load_hex(root / "stream/exec.connect.hex"))
    assert flags == 0
    parsed = Server.FromString(payload)
    assert parsed.SerializeToString(deterministic=True) == payload
    assert response_summary(parsed) == "exec_server_message"
    return f"responses={summaries} end_stream={{}} exec=exec_server_message"


def main():
    assert google.protobuf.__version__ == PINNED_PROTOBUF_VERSION, (
        f"protobuf runtime {google.protobuf.__version__}; "
        f"expected {PINNED_PROTOBUF_VERSION}"
    )
    root = Path(__file__).resolve().parent
    for relative, expected in FIXTURE_SHA256.items():
        actual = hashlib.sha256((root / relative).read_bytes()).hexdigest()
        assert actual == expected, f"{relative}: {actual} != {expected}"
    Client, Server = build_schema()
    print(verify_request(root, Client))
    print(verify_responses(root, Server))
    print(f"protobuf={PINNED_PROTOBUF_VERSION} fixture_sha256=ok deterministic=ok")


if __name__ == "__main__":
    main()
