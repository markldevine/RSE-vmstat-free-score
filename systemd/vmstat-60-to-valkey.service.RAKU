[Unit]
Description=vmstat 60s sampler -> valkey
Requires=valkey.service
After=valkey.service

[Service]
Type=simple
ExecStart=/var/lib/data/raku/bin/raku /var/lib/data/scripts/vmstat-60-to-valkey.raku
Restart=always
RestartSec=1s

# Prevent systemd from throttling frequent restarts
StartLimitIntervalSec=0
StartLimitBurst=0

# Optional niceties (you can keep or remove these)
# Ensure we don't hang long on stop
TimeoutStopSec=5s
# Log to journal
StandardOutput=journal
StandardError=journal

# If your script needs a specific working dir, uncomment:
# WorkingDirectory=/var/lib/data/scripts

[Install]
WantedBy=multi-user.target
