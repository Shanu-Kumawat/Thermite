---
name: Bug report
about: Something misbehaves
title: ""
labels: bug
---

**What happened:**

**What you expected:**

**Machine:** laptop model / kernel / distro / Thermite version
(`thermite status` first line)

**Logs:**

```
journalctl -u thermite -b --no-pager | tail -50
```

**Config** (`/etc/thermite/thermite.toml`, if relevant — no secrets there,
but feel free to trim):

**Failsafe involved?** If fans went to 100% or control returned to
firmware, temperatures at the time (`thermite status`):
