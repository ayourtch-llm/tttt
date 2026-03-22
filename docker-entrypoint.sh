#!/bin/bash
# Docker entrypoint wrapper for tttt
# Loops around tttt invocations, with heartbeat between restarts

while true; do
    /usr/local/bin/tttt "$@"
    echo "[$(date)] tttt exited. Restarting..."
    sleep 1
done