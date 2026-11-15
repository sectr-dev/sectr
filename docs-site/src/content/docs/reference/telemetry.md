---
title: Usage pings
description: The anonymous ping the sectr CLI sends at most once a day, with every field listed and every way to turn it off.
---

The `sectr` CLI sends an anonymous ping at most once a day. It tells us how many installs are in use and which versions they run, and nothing else. This page lists everything in it.

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
| `program` | Which program is pinging: `cli` |
| `version` | Its version |
| `install_id` | A random id made on this machine. It isn't derived from anything about you or the machine |

`sectr telemetry status` prints the exact body your CLI would send, and sends nothing.

## What's never sent

Anything else. No command names or arguments, no errors, no timings, no operating system or hardware details, no file paths, no project or app names, no server addresses, and no account, org, key or session ids. The ping isn't linked to a Sectr account, even when you're logged in.

## When it's sent

At most once a day (UTC), when you run a command.

The first time you run `sectr`, it prints a notice about usage pings and sends nothing. The next command sends the first ping. Each day after that, the first command you run sends one.

A ping goes out in the background while your command runs. If the collector can't be reached, the CLI waits for it at most half a second as the command ends, once a day, and tries again the next time you run a command.

`sectr --help`, `sectr --version` and `sectr telemetry` never send a ping.

## The install id

The id is in `~/.config/sectr/telemetry.toml` (or under `$XDG_CONFIG_HOME/sectr/`), beside the days of the last ping and the last try. It's a random UUID with no timestamp in it.

To reset it, delete the file. The next command makes a new id and shows the notice again. `sectr telemetry off` deletes the id, and `sectr telemetry on` makes a new one, so nothing links your pings from before and after.

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
| `sectr telemetry off` | Turns pings off for this machine and deletes the install id. `sectr telemetry on` turns them back on |
| `SECTR_TELEMETRY=off` | Turns pings off wherever the variable is set. `false`, `0` and `no` work too |
| `DO_NOT_TRACK=1` | The same, for the [cross-tool convention](https://consoledonottrack.com/). Any value but empty or `0` counts |

With either variable set, the CLI creates no install id and no state file, and shows no notice. A variable can only turn pings off: `SECTR_TELEMETRY=on` doesn't override `DO_NOT_TRACK` or `sectr telemetry off`.

If `SECTR_TELEMETRY` is set to something else, the CLI prints a warning and sends nothing.

## Send pings somewhere else

Set `SECTR_TELEMETRY_URL` to send pings to another address, for example a local server where you can read them, or your own copy of the collector:

```sh
SECTR_TELEMETRY_URL=http://localhost:9000/v1/ping sectr telemetry status
```
