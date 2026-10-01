#!/usr/bin/env python3
"""Smoke-test graph tools over an actual MCP stdio session."""

from __future__ import annotations

import argparse
import asyncio
import os
import sys
import tempfile
from pathlib import Path

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client


ROOT = Path(__file__).resolve().parents[1]


async def smoke(harc_bin: Path) -> None:
    env = os.environ.copy()
    env["HARC_BIN"] = str(harc_bin.resolve())
    env["HARC_MCP_WORKSPACE_ROOTS"] = str(ROOT)
    server = StdioServerParameters(
        command=sys.executable,
        args=[str(ROOT / "mcp/harc_mcp_server.py")],
        env=env,
    )
    async with stdio_client(server) as (reader, writer):
        async with ClientSession(reader, writer) as session:
            await session.initialize()
            tools = {tool.name for tool in (await session.list_tools()).tools}
            required = {"harc_graph_context", "harc_dev_graph_query"}
            assert required <= tools, f"Missing MCP tools: {required - tools}"

            with tempfile.TemporaryDirectory(prefix=".harcgraph-mcp-smoke-", dir=ROOT) as tmp:
                dev = await session.call_tool(
                    "harc_dev_graph_query",
                    {
                        "query": "transactor lowering",
                        "index": str(Path(tmp) / "dev"),
                        "roles": ["lowerer"],
                        "token_budget": 350,
                    },
                )
                assert not dev.isError, dev
                dev_text = "\n".join(item.text for item in dev.content if item.type == "text")
                assert "lower_transactor" in dev_text, dev_text

                user = await session.call_tool(
                    "harc_graph_context",
                    {
                        "task": "AXI Lite transactor",
                        "index": str(Path(tmp) / "user"),
                        "token_budget": 350,
                    },
                )
                assert not user.isError, user
                user_text = "\n".join(item.text for item in user.content if item.type == "text")
                assert "AxilXactor" in user_text, user_text

                custom_source = Path(tmp) / "custom.harc"
                custom_index = Path(tmp) / "custom-index"
                custom_source.write_text("module CustomFirst kind verilator\nend module CustomFirst\n")
                indexed = await session.call_tool(
                    "harc_graph_index",
                    {"paths": [str(custom_source)], "out": str(custom_index)},
                )
                assert not indexed.isError, indexed
                custom_source.write_text("module CustomSecond kind verilator\nend module CustomSecond\n")
                refreshed = await session.call_tool(
                    "harc_graph_context",
                    {"task": "CustomSecond", "index": str(custom_index)},
                )
                assert not refreshed.isError, refreshed
                refreshed_text = "\n".join(item.text for item in refreshed.content if item.type == "text")
                assert "CustomSecond" in refreshed_text, refreshed_text
    print("MCP stdio graph smoke passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--harc", type=Path, required=True)
    args = parser.parse_args()
    asyncio.run(smoke(args.harc))
