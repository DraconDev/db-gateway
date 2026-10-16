# Pilot soak log — billing_dev_pilot (started 2026-10-16)

Day-0 record: docs/pilot-day0-2026-10-16.md. One line per probe.


## 2026-10-16T09:24:16Z
```
{"completed_at": 1792142227, "databases": 2, "outcome": "ok", "set": "20261016T091707Z", "started_at": 1792142227, "step": "prune", "ts": "20261016T091707Z"}
===
database                              status  local txid        wal size
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```

## 2026-10-16T10:17:16Z
```
{"completed_at": 1792145824, "databases": 2, "outcome": "ok", "set": "20261016T101704Z", "started_at": 1792145824, "step": "prune", "ts": "20261016T101704Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```

## 2026-10-16T11:17:31Z
```
{"completed_at": 1792149427, "databases": 2, "outcome": "ok", "set": "20261016T111707Z", "started_at": 1792149427, "step": "prune", "ts": "20261016T111707Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```

## 2026-10-16T12:35:21Z
```
{"completed_at": 1792153037, "databases": 2, "outcome": "ok", "set": "20261016T121717Z", "started_at": 1792153037, "step": "prune", "ts": "20261016T121717Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```

## 2026-10-16T13:17:19Z
```
{"completed_at": 1792156627, "databases": 2, "outcome": "ok", "set": "20261016T131706Z", "started_at": 1792156626, "step": "prune", "ts": "20261016T131706Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```

## 2026-10-16T14:19:48Z
```
{"completed_at": 1792160237, "databases": 2, "outcome": "ok", "set": "20261016T141717Z", "started_at": 1792160237, "step": "prune", "ts": "20261016T141717Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```

## 2026-10-16T15:17:10Z
```
{"completed_at": 1792163821, "databases": 2, "outcome": "ok", "set": "20261016T151701Z", "started_at": 1792163821, "step": "prune", "ts": "20261016T151701Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```

## 2026-10-16T16:19:10Z
```
{"completed_at": 1792167437, "databases": 2, "outcome": "ok", "set": "20261016T161717Z", "started_at": 1792167437, "step": "prune", "ts": "20261016T161717Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```

## 2026-10-16T17:20:57Z
```
{"completed_at": 1792171037, "databases": 2, "outcome": "ok", "set": "20261016T171717Z", "started_at": 1792171037, "step": "prune", "ts": "20261016T171717Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```

## 2026-10-16T18:17:16Z
```
{"completed_at": 1792174626, "databases": 2, "outcome": "ok", "set": "20261016T181705Z", "started_at": 1792174625, "step": "prune", "ts": "20261016T181705Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```

## 2026-10-16T18:29:39Z
```
{"completed_at": 1792175379, "databases": 2, "outcome": "ok", "set": "20261016T182939Z", "started_at": 1792175379, "step": "prune", "ts": "20261016T182939Z"}
===
{"billing_dev_pilot.sha256":"5631dfb63e49f024ea259fd34ca7371f8278364f4268c0c40963342459ff4641","size":20480,"verify_probe.sha256":"66222dd0c7a305e84ac12e6da13df8af5ea1c21f7ab841a29b892cd431bb251d","size":24576,"registry.metadata.sqlite.bak.sha256":"a2b5d08a139cabc3be2bf8917e814a8664d21a1b5ad9485de371f4c116ff86d2","size":32768,"timestamp":"20261016T182939Z","verification":"validated_before_publication"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```
Manual mid-hour probe fired through /usr/local/bin/micro2-backup.sh while the dg-backup timer is still on its 50-minute runway to 19:17. Set published, registry validated_before_publication, both DB checksums recorded. attempts.jsonl trailing 57 entries: 57 outcome ok, 15 with databases=2 (post-launch sets).

## 2026-10-16T19:17:17Z
```
{"completed_at": 1792178237, "databases": 2, "outcome": "ok", "set": "20261016T191717Z", "started_at": 1792178237, "step": "prune", "ts": "20261016T191717Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```
Hourly dg-backup.timer fired automatically (systemd OnCalendar *-*-* *:17:00) while the agent session was paused. attempts.jsonl trailing 58 entries: 58 outcome ok, 16 with databases=2 (post-launch sets), max 7d gap 1.21h.

## 2026-10-16T20:17:17Z
```
{"completed_at": 1792181837, "databases": 2, "outcome": "ok", "set": "20261016T201717Z", "started_at": 1792181837, "step": "prune", "ts": "20261016T201717Z"}
===
/var/lib/dg/verify_probe.sqlite       ok      0000000000000001  4.2 kB
/var/lib/dg/billing_dev_pilot.sqlite  ok      0000000000000002  16 kB
```
Hourly dg-backup.timer fired automatically (systemd OnCalendar *-*-* *:17:00) while the agent session was paused. attempts.jsonl trailing 59 entries: 59 outcome ok, 17 with databases=2 (post-launch sets).
