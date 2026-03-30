#!/bin/bash
# Docker entrypoint for tttt
# If no args given, run tttt with "-e claude".
# Otherwise pass all supplied args to tttt.
# In both cases tttt is restarted automatically on exit.

if [ $# -eq 0 ]; then
    set -- -e claude
fi

while true; do
    /usr/local/bin/tttt "$@"
    echo "[$(date)] tttt exited. Restarting..."
    sleep 1
done
