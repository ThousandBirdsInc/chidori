#!/usr/bin/env python3
"""
Demonstrates the Python SDK with multiple sessions, checkpointing, and replay.

Prerequisites:
    1. Start the server:
       LITELLM_API_URL=http://localhost:4401/v1 LITELLM_API_KEY=sk-litellm-master-key \
         ./target/debug/app-agent serve examples/agents/summarizer.star --port 8080

    2. Run this script:
       PYTHONPATH=sdk/python python3 examples/sdk_demo.py
"""

import sys
import os
import time
import json

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk", "python"))

from app_agent import AgentClient, Checkpoint

BASE_URL = os.environ.get("AGENT_URL", "http://localhost:8080")


def main():
    client = AgentClient(BASE_URL)

    # Verify the server is running.
    print("=== Health Check ===")
    print(client.health())
    print()

    # --------------------------------------------------------------------------
    # 1. Run multiple independent sessions with different inputs
    # --------------------------------------------------------------------------
    print("=== Running 3 independent sessions ===")
    print()

    documents = [
        "Rust is a systems programming language focused on safety and performance.",
        "Python is popular for data science due to its rich ecosystem of libraries.",
        "Go was designed at Google for building scalable network services.",
    ]

    sessions = []
    for i, doc in enumerate(documents):
        print(f"Session {i+1}: running...")
        t0 = time.time()
        session = client.run({"document": doc})
        elapsed = time.time() - t0
        sessions.append(session)
        print(f"  ID:     {session.id}")
        print(f"  Status: {session.status}")
        print(f"  Time:   {elapsed:.1f}s (live LLM calls)")
        print(f"  Output: {json.dumps(session.output, indent=2)[:200]}...")
        print()

    # --------------------------------------------------------------------------
    # 2. Save checkpoints to disk
    # --------------------------------------------------------------------------
    print("=== Saving checkpoints ===")
    print()

    checkpoint_dir = "/tmp/app-agent-checkpoints"
    os.makedirs(checkpoint_dir, exist_ok=True)

    for i, session in enumerate(sessions):
        cp = session.checkpoint()
        path = f"{checkpoint_dir}/session_{i+1}.json"
        cp.save(path)
        print(f"  Session {i+1}: saved {len(cp.call_log)} call(s) → {path}")

    print()

    # --------------------------------------------------------------------------
    # 3. Replay from checkpoints — same output, zero LLM calls
    # --------------------------------------------------------------------------
    print("=== Replaying from checkpoints (no LLM calls) ===")
    print()

    for i in range(len(sessions)):
        path = f"{checkpoint_dir}/session_{i+1}.json"
        cp = Checkpoint.load(path)
        print(f"Session {i+1}: replaying from checkpoint ({len(cp.call_log)} cached calls)...")
        t0 = time.time()
        replayed = client.replay(cp)
        elapsed = time.time() - t0
        print(f"  ID:     {replayed.id}")
        print(f"  Status: {replayed.status}")
        print(f"  Time:   {elapsed:.1f}s (replay, no LLM)")

        # Verify the output matches the original.
        original_output = sessions[i].output
        if replayed.output == original_output:
            print(f"  Output: MATCHES original ✓")
        else:
            print(f"  Output: DIFFERS from original!")
            print(f"    Original: {json.dumps(original_output)[:100]}")
            print(f"    Replayed: {json.dumps(replayed.output)[:100]}")
        print()

    # --------------------------------------------------------------------------
    # 4. List all sessions on the server
    # --------------------------------------------------------------------------
    print("=== All sessions on server ===")
    all_sessions = client.list_sessions()
    for s in all_sessions:
        print(f"  {s['id'][:8]}... status={s['status']}")
    print(f"\nTotal: {len(all_sessions)} sessions")


if __name__ == "__main__":
    main()
