import pytest
from unittest.mock import AsyncMock, MagicMock
from chidori import Chidori


@pytest.mark.asyncio
async def test_simple_agent():
    client = Chidori("100", "http://localhost:9800")
    await client.start_server(":memory:")
    await client.deno_code_node(
        name="InspirationalQuote",
        code="""
            return {"promptResult": "placeholder for openai call" }
            """
    )
    pn = await client.deno_code_node(
        name="CodeNode",
        queries=["""
            query Q {
                InspirationalQuote {
                  promptResult
                }
            }
            """],
        code="""
            return {"output": "Here is your quote for "+ new Date() + {{InspirationalQuote.promptResult}} }
            """,
        is_template=True
    )
    await client.play(0, 0)
    await pn.query(0, 100)
    assert 1 == 1



