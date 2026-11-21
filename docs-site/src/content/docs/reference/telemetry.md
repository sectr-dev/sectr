---
title: Telemetry
description: The anonymous request the sectr CLI and a self-hosted server send at most once a day, with every field listed and every way to turn it off.
---

The `sectr` CLI and a [self-hosted](/guides/self-hosting/) Sectr server each send one anonymous request at most once a day: which program it is, its version, and a random install ID. It tells us how many installs are in use and which versions they run, and nothing else. This page lists everything in it.

## What's sent

One small HTTPS request, with this as its whole body:

```json
{
    "program": "cli",
    "version": "0.2.2",
    "install_id": "3f0e9c1a-6b1d-4c57-9a52-0d6f1c2b7e88"
}
```

| Field        | What it is                                                                                            |
| ------------ | ----------------------------------------------------------------------------------------------------- |
| `program`    | Which program sent it: `cli` or `server`                                                              |
| `version`    | Its version                                                                                           |
| `install_id` | A random ID made by this install. It isn't derived from anything about you, your machine or your data |

`sectr telemetry status` shows the exact body your CLI would send.

## What's never stored

Anything else. No command names or arguments, no errors, no timings, no operating system or hardware details, no file paths, no project or app names, no server addresses, no counts of apps, sessions or users, and no account, org, key or session ids. The request isn't linked to a Sectr account, even when you're logged in.

Your IP address isn't kept either. The collector at `telemetry.sectr.dev` ([its code](https://github.com/sectr-dev/sectr-usage)) forwards only the three fields to our analytics and keeps no logs, and our analytics discards IP addresses.

## When it's sent

**The CLI** sends one a day at most, from the first command you run that day. The very first time you run `sectr`, it prints a notice about telemetry and sends nothing.

**A self-hosted server** sends one a minute after it starts, then one every 24 hours, and logs whether telemetry is on when it starts.

## The install ID

It's a random UUID. The CLI keeps it in `~/.config/sectr/telemetry.toml`, and deleting that file resets it. `sectr telemetry off` deletes the ID, and `sectr telemetry on` makes a new one. A self-hosted server keeps its ID in its database.

## Turn it off

Any one of these turns telemetry off:

| How                   | What it does                                                                                                     |
| --------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `sectr telemetry off` | Turns the CLI's telemetry off for this machine and deletes its install ID. `sectr telemetry on` turns it back on |
| `SECTR_TELEMETRY=off` | Turns telemetry off for the CLI or the server, wherever the variable is set. `false`, `0` and `no` work too      |
| `DO_NOT_TRACK=1`      | The same, through the [`DO_NOT_TRACK` convention](https://donottrack.sh) that many developer tools honor. Any value but empty or `0` counts     |

For a self-hosted server, put `SECTR_TELEMETRY=off` in its `.env` and run `docker compose up -d`.

With either variable set, nothing is created: no install ID, no state file, and no notice from the CLI. A variable can only turn telemetry off: `SECTR_TELEMETRY=on` doesn't override `DO_NOT_TRACK` or `sectr telemetry off`.
