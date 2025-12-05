Raku Service Environment - vmstat-free-score.raku
=================================================
A script to characterize system resource availability resulting in a final
freedom score (0..100, higher = more idle) using hysteresis over N samples.

After the score is calculated, it is uploaded to a sorted set in Valkey for
a single cycle answer as to which worker node is most idle.

Lua instructions
----------------
````
SCRIPT LOAD "$(cat restrict_zadd_worker_nodes.lua)"
EVALSHA <sha> 1 RSE^worker-node-candidates mos01 10 candidate123
````
Deployment
----------
````
scp systemd/vmstat-free-score.* mos01:/home/mdevine/
scp systemd/vmstat-free-score.* mos02:/home/mdevine/
scp systemd/vmstat-free-score.* mos03:/home/mdevine/
ssh root@mos01 "install -m 0644 /home/mdevine/vmstat-free-score.service /etc/systemd/system/"
ssh root@mos02 "install -m 0644 /home/mdevine/vmstat-free-score.service /etc/systemd/system/"
ssh root@mos03 "install -m 0644 /home/mdevine/vmstat-free-score.service /etc/systemd/system/"
ssh root@mos01 "install -m 0644 /home/mdevine/vmstat-free-score.timer /etc/systemd/system/"
ssh root@mos02 "install -m 0644 /home/mdevine/vmstat-free-score.timer /etc/systemd/system/"
ssh root@mos03 "install -m 0644 /home/mdevine/vmstat-free-score.timer /etc/systemd/system/"
ssh root@mos01 systemctl daemon-reload
ssh root@mos02 systemctl daemon-reload
ssh root@mos03 systemctl daemon-reload
ssh root@mos01 systemctl enable --now vmstat-free-score.timer
ssh root@mos02 systemctl enable --now vmstat-free-score.timer
ssh root@mos03 systemctl enable --now vmstat-free-score.timer
````
Usage
-----
First, all containers should start at the top of the minute.

Any mos01, mos02, or mos03 resident script can execute:
```
valkey-cli -h 172.19.2.254 --raw ZREVRANGE RSE^worker-node-candidates 0 0
```
which will return a hostname.  If their hostname matches, they run the workload.
