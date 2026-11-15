---
title: Usage pings
description: The anonymous ping the sectr CLI and a self-hosted server send at most once a day, with every field listed and every way to turn it off.
---

The `sectr` CLI and a [self-hosted](/guides/self-hosting/) Sectr server each send an anonymous ping at most once a day. It tells us how many installs are in use and which versions they run, and nothing else. This page lists everything in it.

## What's sent

One small HTTPS request, with this as its whole body:

```json
{
  "program": "cli",
  "version": "0.2.2",
  "install_id": "3f0e9c1a-6b1d-4c57-9a52-0d6f1c2b7e88"
}
```

| Field | What it is |
| -- | -- |
| `program` | Which program is pinging: `cli` or `server` |
| `version` | Its version |
| `install_id` | A random id made by this install. It isn't derived from anything about you, your machine or your data |

`sectr telemetry status` prints the exact body your CLI would send, and sends nothing.

## What's never sent

Anything else. No command names or arguments, no errors, no timings, no operating system or hardware details, no file paths, no project or app names, no server addresses, no counts of apps, sessions or users, and no account, org, key or session ids. The ping isn't linked to a Sectr account, even when you're logged in.

## When it's sent

**The CLI** pings at most once a day (UTC), when you run a command.

The first time you run `sectr`, it prints a notice about usage pings and sends nothing. The next command sends the first ping. Each day after that, the first command you run sends one.

A command run with `--json` (`sectr send`, `sectr tail`) never prints anything about pings, so its output stays plain JSON even with stderr merged in. On a new machine it doesn't show the notice either: the notice waits for the first command without `--json`, and nothing is sent before it.

A ping goes out in the background while your command runs. If the collector can't be reached, the CLI waits for it at most half a second as the command ends, once a day, and tries again the next time you run a command.

`sectr --help`, `sectr --version` and `sectr telemetry` never send a ping.

**A self-hosted server** sends its first ping a minute after it starts, then one every 24 hours while it runs. When it starts, it logs a line that says whether pings are on. A ping that fails is dropped until the next one is due.

Sectr's hosted service sends no server pings: they count self-hosted installs only. The CLI's ping is the same whichever server you use, and doesn't say which.

## The install id

**The CLI's** id is in `~/.config/sectr/telemetry.toml` (or under `$XDG_CONFIG_HOME/sectr/`), beside the days of the last ping and the last try. To reset it, delete the file. The next command makes a new id and shows the notice again. `sectr telemetry off` deletes the id, and `sectr telemetry on` makes a new one, so nothing links your pings from before and after.

**The server's** id is the one row of the `server_instance` table in its database, made when the first ping is due. Servers that share a database share the id. To reset it, delete the row. A server with pings off never makes one.

Both are random UUIDs with no timestamp in them.

## Your IP address

Pings go to a small collector at `telemetry.sectr.dev`, a Cloudflare Worker, and not straight to our analytics. Like any request, a ping arrives there from your IP address. The collector doesn't read the address, doesn't log it and doesn't pass it on:

- It checks the three fields, then builds a new request that holds only them and sends that to PostHog, where the counts are kept. None of your request's headers go with it.
- It keeps no logs.
- The PostHog project is also set to discard IP addresses and to skip location lookup.

The collector's code is public: [sectr-dev/sectr-usage](https://github.com/sectr-dev/sectr-usage).

## Turn it off

Any one of these turns pings off:

| How | What it does |
| -- | -- |
| `sectr telemetry off` | Turns the CLI's pings off for this machine and deletes its install id. `sectr telemetry on` turns them back on |
| `SECTR_TELEMETRY=off` | Turns pings off for the CLI or the server, wherever the variable is set. `false`, `0` and `no` work too |
| `DO_NOT_TRACK=1` | The same, for the [cross-tool convention](https://consoledonottrack.com/). Any value but empty or `0` counts |

For a self-hosted server, put `SECTR_TELEMETRY=off` in its `.env` and run `docker compose up -d`.

With either variable set, nothing is created: no install id, no state file, and no notice from the CLI. A variable can only turn pings off: `SECTR_TELEMETRY=on` doesn't override `DO_NOT_TRACK` or `sectr telemetry off`.

If `SECTR_TELEMETRY` is set to something that's neither on nor off, the CLI prints a warning and sends nothing, and the server refuses to start, so a typo can't leave pings on.

## Send pings somewhere else

Set `SECTR_TELEMETRY_URL` to send pings to another address, for example a local server where you can read them, or your own copy of the collector. It works for the CLI and the server:

```sh
SECTR_TELEMETRY_URL=http://localhost:9000/v1/ping sectr telemetry status
```
