#!/bin/bash

# Create log files for LiteLLM
mkdir -p logs

# Start LiteLLM and redirect both stdout and stderr to log files
uv run litellm --config ./litellm_config.yaml > logs/litellm.out 2> logs/litellm.err &
LITELLM_PID=$!

# Start Chidori and stream its output to console while still maintaining the background process
chidori-core run --load /usr/src/example_agent 2>&1 &
CHIDORI_PID=$!

# Optional: Print PIDs for debugging
echo "LiteLLM PID: $LITELLM_PID"
echo "Chidori PID: $CHIDORI_PID"

# Wait for any process to exit
wait -n

# Capture the exit status
EXIT_STATUS=$?

# Optional: Print which process exited
if ! kill -0 $LITELLM_PID 2>/dev/null; then
    echo "LiteLLM process exited first with status $EXIT_STATUS"
elif ! kill -0 $CHIDORI_PID 2>/dev/null; then
    echo "Chidori process exited first with status $EXIT_STATUS"
fi

# Clean up any remaining processes
kill $LITELLM_PID $CHIDORI_PID 2>/dev/null

# Exit with status of process that exited first
exit $EXIT_STATUS