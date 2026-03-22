#!/bin/bash
# Docker entrypoint for tttt
# If no args given, launch claude. Otherwise pass args to tttt (with restart loop).

if [ $# -eq 0 ]; then
    exec claude
fi

while true; do
    /usr/local/bin/tttt "$@"
    echo "[$(date)] tttt exited. Restarting..."
    sleep 1
done
