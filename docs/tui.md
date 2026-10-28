# The terminal view

`dg` with no command is the operator's screen: the service, its
databases, their backup freshness and its credentials, each reachable
through its own tab, with the actions that change them a keystroke away.

```sh
dg --key-file owner.key
```

`dg tui` still works and is the explicit spelling — the two are the
same view, so a script that already runs `dg tui` does not have to
change. A bare `dg` only opens the view when stdout AND stdin are a
terminal and `--json` was not given. Otherwise it behaves exactly as it
did when the subcommand was required: the usage goes to **stderr**,
stdout stays empty, the text is plain (no colour escapes, even when
`CLICOLOR_FORCE` or `FORCE_COLOR` is set), and the exit code is 2 —
because a pipe cannot drive a full-screen view, and a script reading
`dg`'s command list (or piping `dg --json`) must not be handed one, nor
handed help prose or escape sequences where it expected silence.

It talks to the same gateway, over the same API, with the same
credential as every other command — `--server` and `--key-file` mean
what they mean everywhere else. A remote gateway is reached the same
way it always is: through an SSH tunnel.

## What is on the screen

A tab strip, and the body of the tab that is showing.

The bar along the top names the gateway, says how long ago it was read, and
says whether it answered. It is **reachable** when it answered anything at
all: a refused backup list does not mean the service is down, so one answer
out of five is an answer. The bar used to say otherwise — the threshold was
written down as "fewer than four failures", which was right when the view
read four things and wrong the moment it read five, and the view then said
*unreachable* while the gateway was answering. The rule is counted from the
answers now, so a sixth read cannot leave it behind.

The view reads those five things together rather than one after another, so
a gateway that hangs costs one read's budget rather than five.

| Tab | Key | What it answers |
|---|---|---|
| Databases | `1` | Every database the gateway lists, with the rows it has read and written, its own wait p95 and its worst wait, and when it was last written |
| Service | `2` | The service dashboard — version, uptime, request totals and rate, error counts, admission permits, the wait percentiles, the recent request-rate sparkline — and where the gateway is running: the host, the address it bound, the data directory, the backup root, the room left on the disk and the retention it holds a deleted name for |
| Backups | `3` | Whether the copies can be trusted: the backup root, the freshness objective, the approved snapshot cadence, the last run, what the newest set carries, where the off-host copy is and whether it is running, the last restore drill, the window of moments the copy can be taken back to, the last restore that was taken, the published sets, and each database's freshness state |
| Keys | `4` | Every outstanding credential, its role, and the databases it may reach |
| Queries | `5` | Which statement is the expensive one — the statement shapes the service has run, how often each ran, the rows it read and wrote, how long it took, and which credentials ran it |
| Locations | `6` | Where each database's file is on disk, how big it is, and whether it is there — a live registry row whose file is gone does no work at all, and no counter would say so |

The tabs are also reached with `tab` and `shift-tab`. Each tab carries its
number and, where it has one, its count: the strip is a summary, not just a
switchboard, so an operator can see how much is in a tab without opening it.

On a terminal too narrow for the whole strip, the **counts go and the names
stay**. The counts are the summary; the names are the promise that every tab
exists. A tab nobody can see is a tab nobody opens, so the sixth one is never
the one that gets cut off — and the number keys still reach every tab
whatever the width.

The view shows **one tab at a time** rather than tiling every answer. Tiling
forces each answer into a fraction of the width, which squeezes a wide table
and cuts a long list; a tab gives the whole body to the thing being read. The
cost — six answers are off screen — is paid by the strip, which always names
every tab.

### One question per tab

Each tab answers one question and shows one answer. The answer may be
several facets of that one answer — the backups tab's freshness table
and its list of published sets are both about whether the copies can be
trusted — but no tab answers another tab's question.

The per-database traffic breakdown is the databases tab's, and it is not
repeated on the service tab: the same rows under a second heading, on a
second tab, is the tiling problem returning in a new shape.

**Where the databases live is a question of its own**, and it used to be
answered inside the databases tab, as a second box under the traffic table.
That is the tiling problem in a smaller frame: the body was split in two,
the placement table took whatever the traffic table left, and on a screen
with enough databases the placement table was squeezed to a header and a row.
Placement is the sixth tab now, and each of the six answers has the whole
body to itself.

The tabs are written down in one place — `Focus::ALL` in `src/tui/mod.rs` —
and the strip, the number keys, the `tab` / `shift-tab` cycle and the
command bar's range are all built from it. They used to be written down in
six places, and they drifted: the command bar said `1-4` while a fifth tab
was on the screen, because only one of the six lists had been updated. A
list written once cannot drift.

## Which query is heavy

The queries tab answers the question a hosted provider cannot answer exactly.
Every statement passes through the gateway on its way to SQLite, so the gateway
is the only place that can say what each one cost. For a provider that answer
is a privacy liability and a tenant-isolation problem, and nothing in their
business depends on it; here there is no middleman, and the operator owns the
box it all happens on.

A row names a statement by its **shape** — the SQL with every literal
replaced by a placeholder — so the table says how expensive a statement is
without quoting the data it carried. `SELECT * FROM users WHERE email =
'ada@example.com'` is shown as `SELECT * FROM users WHERE email = ?`. The
identifiers survive, because a column name is schema, and the schema is what
an operator needs to see.

Each row carries how many times the shape ran, the rows it read and wrote, the
total time it took and its slowest single run, and the credentials that ran
it. The heavy statement is first: the ordering is by rows read, then by total
time.

A long shape is clipped to the width of the table, with an ellipsis, because
a statement that wraps pushes every other column off the screen. The full text
is in `dg queries`, and in `GET /v1/queries`.

The tab is empty when nothing has run yet, and says so rather than drawing
nothing. The shapes it holds live in the gateway's memory, so a restart
empties the tab and it fills again as the statements run: it answers a
present-tense question, and the trail that has to outlive a restart is
`dg audit`'s, which is on disk.

## Where things are

The service tab says where the gateway is running — the host, the address it
actually bound, the directory it keeps its databases in, and the backup root
it watches — and the locations tab says where each database's file is and
whether that file is there. The counters say what a database did; only the
file says whether it could.

Neither answer can be worked out by the client. A gateway is usually reached
through a tunnel, so from the operator's machine it is a loopback port and
nothing more, and the machine on the other end is the one holding the data.
The gateway is therefore the only place that can report it, and it does so in
`GET /v1/status` — the same answer the CLI's `dg status` prints.

A gateway that does not report its placement (an older build) says so in
words: the service tab shows *placement: not reported by this gateway*, and
the databases tab says that it does not report where its databases live. An
empty table would have been a different claim — a gateway with no databases
— and the two must not look alike.

## Are the copies any good

The backups tab answers the question that comes after "where is it": can I
trust what I have? Four facts, each in its own group, because they are four
different claims:

- **last run** — when a backup last ran and whether it worked. A failed run
  publishes no set, so this record is the only evidence that one happened
  and failed.
- **coverage** — what the newest set carries, and whether it carries the
  registry. The registry matters as much as the data: without it the copies
  hold files that no key can open.
- **replica** — where the off-host copy is (the bucket, the endpoint, the
  region), how long it is kept, what it covers, and whether the sidecar that
  does the copying is running.
- **restore drill** — whether the off-host copy has ever been restored and
  compared against the live gateway. A backup that was never restored is an
  assumption.
- **point-in-time restore** — the window of moments the copy can be taken
  back to, and the last restore that was actually taken. "There is a backup"
  is a claim; "you can go back to any moment between these two" is the one
  that matters at two in the morning. A window nobody has ever exercised is
  a promise, so the last restore is shown beside it.

The two tables below them are the per-database freshness and the published
sets. The freshness table is reserved its full row count first -- one row
per database, plus its frame -- the published-sets table keeps its minimum
promise of a header and one row, and the field list takes the remainder
and scrolls. A table that has nothing but its header is a table with no
answer in it, so on a terminal too short for even that, the table is
capped by the room left after the published-sets minimum and keeps its
header and as many rows as fit. The set list, which is a log of what has
been published, takes the space that is left and scrolls.

The consequence lands on wide deployments. The table carries a row per
database, so thirty databases reserve thirty rows before the summary sees
any room: the field list takes the small remainder and scrolls, and the
facts about the copy -- where it is, whether it was ever restored --
start below the fold. They are not gone: the arrow keys reach every fact,
and the scroll never runs past the end of what the tab holds. What does
not fit is a keystroke away.

The number at the top of the tab is the **freshness objective**, not an age
limit. Freshness is decided by whether a committed change is covered by a
verified copy, so a set can be older than the objective and still be `fresh`
while the database is idle; the table below is the authority, and the
objective is the reference that decides when an unrepresented change deserves
an operator's attention.

These facts come from the host, not from the gateway's own memory: the
sidecar's configuration, the drill's result and the restore window live on
the machine that owns the bucket, and the host publishes them where the
gateway reads them. A host that publishes nothing says so in words — *this
host publishes no off-host copy status* — rather than leaving a hole where a
fact should be.

The restore window is the one that turns a backup into a recovery. The
replica keeps a rolling window of write-ahead log, so the data can be
restored to a moment and not only to the last hourly snapshot. The window
is published by `scripts/restore-point.sh --list`, and taking a restore is
the host's job — it holds the bucket's credentials — through
`scripts/restore-point.sh --apply --to <ISO8601> --database <name>`, which
restores into a fresh directory, verifies the copy and publishes what it
produced. The view reports both the window and the last restore, so the
claim and the evidence sit next to each other.

## The colours

The palette is dark, high-contrast, and **semantic**: colours are assigned to
meanings once, in `src/tui/theme.rs`, and nothing else in the view names a
colour. A failure and a success cannot drift into looking alike, because they
are the same two constants everywhere.

Every colour that carries text meets WCAG AA (4.5:1) against the background,
and the emphasised ones meet AAA (7:1); the ratios are asserted in the theme's
own tests, so a future tweak that hurts readability fails the build. Colour is
never the only signal: a failure is red *and* says so, the active tab is
tinted *and* underlined, and the caret marks it a third time.

## The command bar

The bar along the bottom names the keys that matter, in the same order on
every screen: `1-6` tabs, `tab` next tab, `↑↓` scroll, `:` commands, `r`
refresh, `q` quit. Everything else is behind `:` — the palette is powerful but
invisible, and nobody finds a command they cannot see, so the ones an operator
needs constantly are always on screen with the key visually distinct from the
label.

The error, when there is one, is at the end of that line. It says what
happened rather than listing every failure — *four of five reads failed:
forbidden* — because five refusals of the same kind are one fact, not five,
and the bar has one line for it. A message too long for the line is cut with
an ellipsis, so a message that runs off the edge says that it did.

## The database page

Opening a database opens a **page**, not a pane: its tables down the
left, and the selected table's values filling the rest of the screen.

```text
┌ inventory ───┐┌ values ──────────────────────────────────────
│▸ products  60││products in inventory
│  stock_moves││┌ rows ───────────────────────────────────────
│  suppliers  │││id   sku        name      price_cents  stock
│             │││1    SKU-1001   item 1    537            3
│             │││...
│             ││rows 1-10 of 60   [n] next  [s] structure
```

A database is a place you go, so it gets a page with room for the thing
you came to look at. The earlier shape — a note under a dashboard that
kept the whole screen — had room for a summary, not for a table.

| Key | What it does |
|---|---|
| `↓` / `↑` | Move to the next / previous table |
| Click a table name | Select that table directly |
| `enter` | Open the selected table in the rows browser |
| `s` | The selected table's structure |
| `esc` | Back to the dashboard |

The list and the values always agree: moving the selection reads that
table's rows, so the name on the left is the data on the right.

## The rows browser

Opening a table opens its **rows**: the data, ten at a time. The table
takes the WHOLE screen while it is open — it is the thing you opened, not
a note about it under a dashboard. `esc` puts the dashboard back.

This is a place the operator stays in, not a one-shot read, so the table,
the page and the choices narrowing it are kept together — every keystroke
re-asks the same question rather than starting over.

| Key | What it does |
|---|---|
| `n`, `↓` / `→` / `pgdn`, or the wheel down | The next page |
| `p`, `↑` / `←` / `pgup`, or the wheel up | The previous page |
| `o` | Choose the sort column by name; `stock:desc` reverses, an empty answer clears it |
| `f` | Edit the filter; an empty condition clears it |
| `c` | Clear the filter and go back to the first page |
| `s` | The table's structure — the columns, their types and their flags |
| `esc` | Leave the browser |

The footer says which slice of the answer is on screen, and how it got
that way:

```text
rows 21-30 of 62   sorted by stock desc   filter: stock < 40   [p] prev  [n] next  [s] structure
```

`of 62` is the size of the **filtered answer**, never the table's total.
A footer that said the table's total under a filter would make every
page look like it was the table, and a footer that said the page length
would make every page look like the last one.

The sort column is **named, not assumed**. It used to cycle off the
first column, which on a typical table is the `id` — the rowid alias — so
"sorted by id asc" reordered nothing (the unsorted page is already in
rowid order) and every other column was unreachable from the view. `o`
now opens a prompt listing the table's real columns, and the name is
validated against that schema before it can reach a statement: a column
the table does not have is refused, not interpolated.

The arrow keys and the wheel page the browser rather than scrolling it:
a browser has pages, not a scrollbar, so scrolling would move content
that is already the whole page while the operator meant "show me the
next one".

Sorting and filtering are applied **by the gateway**, in the statement,
not by discarding rows after they arrive: otherwise page 2 of a filtered
view would be empty while the footer claimed there was more. The sort
column is checked against the table's real columns before it is written
into the statement, and the filter is checked by the same read-only rule
the query endpoint uses, so the browser cannot write — a filter that
would modify data is refused, not run.

`esc` leaves the browser and returns to the dashboard; `s` leaves for the
table's structure. Neither quits the view.

## The keys

| Key | What it does |
|---|---|
| `:` | Open the actions |
| `r` | Read the service again |
| `tab` | Move to the next tab |
| `↑` / `↓` | Scroll the open tab |
| `page up` / `page down` | Scroll the open tab a screen at a time |
| `home` | Back to the top of the open tab |
| `q` | Leave the view |
| `ctrl-c` | Leave the view, from anywhere |

A tab that holds more than it shows says so: the database list and the
credential list carry their count in the tab strip. The strip is the summary
of every tab — including the ones you cannot see — so it is where the count
belongs, and the body's frame does not repeat it.

The scroll never runs past the end of what the tab holds. A tab scrolled
into an empty screen reads as *there is nothing here*, which is a different
and much worse claim than *you have reached the end*, so the last row stays
on screen and a table that already fits does not scroll at all.

A table that needs explaining says so under itself. The database list's
`wait p95` and `wait max` are different facts, and the line under the rows
says which is which.

The counts are read on their own, slower clock than the two-second
traffic poll: counting every row of every database means opening every
file, which is not work to do every two seconds for an idle service. The
slower clock is NOT a promise that the figures are fresh when the
operator asks, so `r` and opening a database both re-read them — a count
answered from a cache up to 30 seconds old is exactly the stale figure
this tab exists to stop showing.

The databases tab leads with what each database **holds** — its `tables`
and `rows`, read from the file — and only then the traffic figures. The
counters (`rows read*`, `rows written*`, `waits*`, `last write*`) are
marked because they are in memory and a gateway restart zeroes them: a
tab built only from them shows seven full databases as seven empty ones,
which is exactly what it did before the counts were added.

In the actions list: type to filter, `↑`/`↓` to choose, `enter` to
go, `esc` to go back.

## The mouse

The view turns mouse reporting on when it opens and off again when it
leaves, so the shell you return to keeps its selection and its wheel.

| Gesture | What it does |
|---|---|
| click a tab's label | Open that tab, exactly as its number key does |
| move the pointer over a table | Mark the row under it, so a click can be seen before it is taken |
| click a row that names a database | Open that database: its tables, with their row and column counts, appear in the last action |
| click a table in that list | Open that table's rows in the browser: the data, a page at a time |
| wheel over the body | Scroll the open tab; three lines a notch |
| click a scrolled body | Back to the top of the open tab |

Only the strip and the body are targets: a click on the separators, the
top bar or the status line does nothing, and a click that lands while the
actions list or a prompt is open belongs to that dialogue rather than to
the view underneath it. The mark follows the pointer and leaves with it:
a header is never marked, because a header is not a row, and a table the
pointer has left keeps no mark behind.

The mouse's doorway and the keyboard's are different depths, and both
are one gesture. A click opens the **database** and then the **table**:
what is in it, and what its columns are. Enter opens a table's **rows**,
with the database already typed into the prompt, so only the table name
is left to type.

Mouse support deliberately stops at reading. Nothing in the view can be
*changed* by a click — the rows that change something are the actions
list's, and those are chosen by the keyboard — because the destructive
actions are confirmed by typing their own name and that confirmation
must not be reachable by a misplaced pointer. The keys and queries tabs
open nothing: their rows are not databases.

## The actions

| Action | What it types | What it does |
|---|---|---|
| refresh | — | Reads the service again |
| query | `query <database> <sql>` | Runs one statement; the rows appear in the last-action area |
| tables | `tables <database>` | Lists one database's tables with row counts |
| schema | `schema <database> [table]` | Reads a table's structure: each column's declared type, its not-null and primary-key flags, the table's indexes and foreign keys, and its `CREATE` statement |
| open table | `open <database> <table> [page]` | Opens a table in the rows browser: 10 rows a page, sortable and filterable (Enter on the databases tab lists the action with the scrolled-to database already typed into its prompt; clicking the table in an opened database does the same without typing) |
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

The three irreversible actions — **delete database**, **revoke key** and
**rotate owner key** — are marked with a `!` in the actions list and drawn
in the colour the theme keeps for a destructive act. Listing them beside
*refresh* with the same weight would tell the operator that the two are the
same kind of thing, and they are not. The marker is a character and not only
a colour, so it survives a monochrome terminal.

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
- It does not call a read a write. A query is a POST whether it reads
  or writes, so the statements decide what a request can commit, with
  the same rule the server uses and the same rule the CLI uses. A
  read-only query that cannot reach the gateway is reported as an
  unavailable gateway; only a statement that writes is reported as a
  write whose outcome is in doubt.
- It does not leave your terminal broken. The alternate screen and
  raw mode are always given back, including when the view panics
  (`src/tui/screen.rs`).

## When the terminal is not a terminal

The view refuses to start when stdout is not a terminal, and says so.
A bare `dg` in that position prints the usage to stderr and exits 2
instead — the behaviour it had before the view was the default, down to
the stream — and `dg tui` reports `not_a_terminal` with the same advice. For machine output, use the one-shot commands with
`--json`: their output is unchanged by any of this.

## Related

- [docs/api.md](api.md) — the API the view talks to
- [docs/cli-turso-mapping.md](cli-turso-mapping.md) — the command
  surface next to Turso's
- [docs/architecture.md](architecture.md) — where the view sits
- `docs/evidence/tui-view-2026-10-22.txt` — a real session against a
  real gateway, rebuilt from the raw pty capture beside it. **Historical:**
  it predates the tabbed view, so it shows the tiled layout and the old
  status bar, and its layout lines should not be read as current. Read it
  for the request/response traffic. `scripts/capture-tui-evidence.sh`
  makes a current one.
