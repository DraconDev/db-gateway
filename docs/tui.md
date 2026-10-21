# The terminal view (`dg tui`)

`dg tui` is the operator's screen: the service, its databases, their
backup freshness and its credentials, all live at once, with the
actions that change them a keystroke away.

```sh
dg --key-file owner.key tui
```

It talks to the same gateway, over the same API, with the same
credential as every other command — `--server` and `--key-file` mean
what they mean everywhere else. A remote gateway is reached the same
way it always is: through an SSH tunnel.

## What is on the screen

| Pane | What it answers |
|---|---|
| Top bar | Which gateway, when it was last read, and whether it answered |
| Databases | Every database the gateway lists, with the rows it has read and written, its own wait p95, and when it was last written |
| Service | The service dashboard: version, uptime, request totals and rate, error counts and rate, admission permits, the wait percentiles, and the recent request-rate sparkline |
| Backups | The backup root, the stale-after threshold, the published sets, and each database's freshness state |
| Keys | Every outstanding credential, its role, and the databases it may reach |
| Last action | What the action you just ran did, and the rows it produced |

The view reads the service again every two seconds. `r` reads it
again at once, and `--refresh <seconds>` changes the cadence.

## The keys

| Key | What it does |
|---|---|
| `:` | Open the actions |
| `r` | Read the service again |
| `tab` | Move to the next pane |
| `↑` / `↓` | Scroll the focused pane |
| `page up` / `page down` | Scroll the focused pane a screen at a time |
| `home` | Back to the top of the focused pane |
| `q` | Leave the view |
| `ctrl-c` | Leave the view, from anywhere |

A pane that holds more than it shows says so: the database list and
the credential list carry their count in the pane's title.

In the actions list: type to filter, `↑`/`↓` to choose, `enter` to
go, `esc` to go back.

## The actions

| Action | What it types | What it does |
|---|---|---|
| refresh | — | Reads the service again |
| query | `query <database> <sql>` | Runs one statement; the rows appear in the last-action pane |
| run sql file | `run <database> <script.sql>` | Runs a SQL file as one atomic batch, statement by statement |
| run batch file | `batch <database> <batch.json>` | Runs a JSON batch file |
| create database | `create <name>` | Creates an empty database |
| delete database | `delete <name>` | Tombstones a database — the exact name has to be typed again |
| clone database | `clone <source> <destination>` | Copies a database |
| export database | `export <name> <file.sqlite>` | Downloads a snapshot to a file, published atomically |
| import database | `import <name> <file.sqlite>` | Uploads a file as a new database |
| issue key | `issue <name> <role> <database>…` | Issues a scoped credential; the key is shown once |
| revoke key | `revoke <name>` | Revokes a credential — the exact name has to be typed again |
| rotate owner key | `rotate` | Replaces the owner credential; the old one dies immediately |
| quit | — | Leaves the view |

The roles are `database_read_only` and `database_read_write`.

### Destructive actions

Deleting a database and revoking a credential ask for the exact name
before they do anything, and the owner-key rotation asks for the word
`rotate`. This is the same rule the CLI's `--confirm` flag enforces
(`src/confirmation.rs`), so an act that destroys something cannot
happen on a stray keypress in either surface.

### A script that is too long

The batch API takes at most 32 statements, and a longer file cannot
be split without giving up the atomicity a migration needs. So the
view refuses an over-long script *before it is sent*, and says how
many statements the file holds, what the API accepts, and that nothing
was sent — the same refusal, with the same numbers, that the CLI
gives (`src/execution.rs`).

## What the view will not do

- It does not change the API. Every route it uses is one the CLI and
  the TypeScript client already use.
- It does not keep its own copy of the truth: the numbers on screen
  are read from the shared view layer (`src/views.rs`), the same
  functions the CLI draws from, so the two surfaces cannot tell an
  operator different stories about the same gateway.
- It does not invent a request. Every action maps to a documented
  method and path in one place (`src/tui/actions.rs`), and the
  destructive ones are confirmed by the shared rule.
- It does not leave your terminal broken. The alternate screen and
  raw mode are always given back, including when the view panics
  (`src/tui/screen.rs`).

## When the terminal is not a terminal

`dg tui` refuses to start when stdout is not a terminal, and says so.
For machine output, use the one-shot commands with `--json`: their
output is unchanged by any of this.

## Related

- [docs/api.md](api.md) — the API the view talks to
- [docs/cli-turso-mapping.md](cli-turso-mapping.md) — the command
  surface next to Turso's
- [docs/architecture.md](architecture.md) — where the view sits
- `docs/evidence/tui-view-2026-10-21.txt` — a real session against a
  real gateway, rebuilt from the raw pty capture beside it
  (`scripts/capture-tui-evidence.sh` makes another)
