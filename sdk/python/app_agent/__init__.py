"""App Agent Framework — Python SDK.

Connect to a running app-agent server to create sessions,
run agents, checkpoint execution, and replay from saved state.

Usage:
    from app_agent import AgentClient

    client = AgentClient("http://localhost:8080")

    # Run an agent
    session = client.run({"document": "Hello world"})
    print(session.output)

    # Save checkpoint and replay
    checkpoint = session.checkpoint()
    replayed = client.replay(checkpoint)
"""

from app_agent.client import AgentClient, Session, Checkpoint

__all__ = ["AgentClient", "Session", "Checkpoint"]
