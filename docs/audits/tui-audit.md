# `dg` terminal view — audit

Audit of the interactive terminal view (`dg`, `dg tui`): all six tabs, the
database/table page, the rows browser, the command palette and its actions,
keyboard and mouse interaction, responsive layout and resizing, and terminal
startup, error states and teardown.

- **Scope**: `src/tui/**`, the TUI-only parts of `src/main.rs`, `docs/tui.md`,
  and the TUI test suites. Shared view code (`src/views.rs`) was touched only
  where a TUI defect needed it, with the CLI's behaviour protected by the
  existing CLI tests.
- **Out of scope by the objective**: API/storage semantics, and Turso. No
  Turso endpoint was contacted during this audit and no route or statement
  contract changed.
- **Runtime evidence**: `docs/evidence/tui-audit-runtime.log` (raw pty
  stream) and `docs/evidence/tui-audit-runtime.txt` (rendered frames), made by
  `scripts/capture-tui-evidence.sh` against a disposable gateway it creates in
  a temporary directory. The long-running local gateway was left at its
  seven-database baseline.

## How this was tested

| Layer | What it exercises | Where |
|---|---|---|
| Frame tests | A real `ratatui` frame drawn into `TestBackend`: what an operator would actually read, at a chosen width and height | `tests/tui_layout.rs`, `tests/tui_page.rs`, `tests/tui_browser.rs`, `tests/tui_mouse.rs`, `tests/tui_schema.rs`, `tests/tui_tables.rs`, `tests/tui_view.rs` |
| Interaction tests | Keys and mouse events against a drawn frame, resolving clicks from the geometry the frame recorded | `tests/tui_mouse.rs`, `tests/tui_page.rs`, `tests/tui_browser.rs` |
| Gateway tests | The view's own reads against a real in-process gateway/router: reachability, error reporting, contents cadence | `tests/tui_refresh.rs`, `tests/tui_actions.rs`, `tests/tui_schema.rs` |
| Real pty | `script(1)` + `stty`: alternate screen, raw mode, exit, shell left working | `tests/tui_terminal.rs` |
| Unit | Terminal restoration guard, theme contrast, footer arithmetic | `src/tui/screen.rs`, `src/tui/theme.rs`, `tests/tui_browser.rs` |

Every finding below was reproduced as a failing test (or a failing real pty
run) **before** the fix, and the test that now passes is named.

## Coverage matrix

### The six tabs

| Tab | Answer it must give | Evidence |
|---|---|---|
| Databases | Every database, what it holds (`tables`, `rows` from the file), and the since-restart traffic counters | `tests/tui_layout.rs::the_databases_tab_says_how_big_each_database_is`, `::a_database_with_no_traffic_is_on_screen_with_zeroes`, `::idle_databases_read_as_zeroes_not_as_missing` |
| Service | Version, uptime, request totals/rate, error counts, admission permits, wait percentiles, sparkline, placement | `tests/tui_layout.rs::the_service_tab_says_where_the_gateway_is`, `::a_gateway_that_reports_no_placement_says_so_rather_than_looking_empty` |
| Backups | Freshness objective, cadence, last run, coverage, replica, drill, restore window, per-database freshness table, published sets | `tests/tui_layout.rs::the_backups_tab_says_whether_the_copies_are_any_good`, `::the_freshness_table_keeps_every_database_row_at_any_height`, `::an_unlisted_state_row_remains_visible_at_short_heights`, `::the_backups_tab_says_which_moments_the_copy_can_be_taken_back_to`, `::the_freshness_reference_is_named_as_an_objective_not_as_an_age_threshold` |
| Keys | Every outstanding credential, its role, the databases it reaches | `tests/tui_layout.rs::every_tab_answers_its_own_question_and_only_its_own`, `tests/tui_mouse.rs::clicking_a_credential_opens_nothing` |
| Queries | Statement shapes, runs, rows read/written, time, credentials | `tests/tui_layout.rs::the_queries_tab_says_which_statement_is_the_expensive_one` |
| Locations | Each database's file, its size, whether it is there; a gateway that does not report placement says so | `tests/tui_layout.rs::the_locations_tab_says_where_each_database_is_and_whether_it_is_there` |

Cross-tab invariants: `::every_tab_is_named_on_screen_and_holds_its_own_answer`,
`::every_tab_answers_its_own_question_and_only_its_own`,
`::the_tabs_are_written_down_once_and_everything_is_built_from_that_list`,
`::every_tab_is_reachable_from_the_keyboard`, `::a_tab_cannot_be_scrolled_into_nothing`.

### Database page and rows browser

| Behaviour | Evidence |
|---|---|
| A database opens as a page: tables on the left, the selected table's values on the right | `tests/tui_page.rs::the_page_is_drawn_with_the_list_and_the_values`, `tests/tui_mouse.rs::clicking_a_database_opens_it` |
| Selection moves with arrows/click/wheel and always stays on screen | `::moving_through_the_list_wraps_nothing_and_stops_at_its_ends`, `::moving_past_the_visible_list_keeps_the_selection_on_screen`, `::a_click_after_list_scroll_selects_the_table_at_that_visible_row`, `::the_wheel_over_the_table_list_moves_the_table_selection`, `::the_table_list_wheel_stops_at_the_last_table` |
| Enter opens the table's rows; `s` opens its structure; Esc returns to the dashboard | `::enter_opens_the_selected_table_in_the_rows_browser`, `::esc_returns_to_the_dashboard` |
| Rows browser footer, sorting, filtering, paging ends, refused reads | `tests/tui_browser.rs` (27 tests) |
| Every fetched row stays reachable, on any terminal height | `::a_short_rows_browser_screen_keeps_the_whole_fetched_page_visible`, `::the_smallest_rows_browser_scrolls_the_fetched_page_without_losing_rows` |
| A table opened in the browser takes the whole screen | `::an_opened_table_takes_the_whole_screen`, `::leaving_the_browser_returns_the_dashboard` |

### Palette and actions

| Behaviour | Evidence |
|---|---|
| Every action appears once, and asks for the documented route | `tests/tui_actions.rs::every_action_is_in_the_catalogue_once`, `::the_actions_ask_the_gateway_for_what_the_api_documents` |
| Destructive acts are marked and confirmed by the same rule the CLI uses | `::a_destructive_action_has_to_be_confirmed_by_name`, `tests/tui_layout.rs::the_palette_marks_the_actions_that_cannot_be_taken_back`, `::a_deletion_asks_for_the_name_twice_and_runs_only_then` |
| A request states what it can commit; a read is never reported as a possible write | `::every_request_carries_what_it_can_commit`, `::a_read_only_query_that_cannot_reach_the_gateway_says_so`, `::a_query_that_writes_is_answered_as_a_write` |
| An over-long script is refused before it is sent | `::a_whole_request_is_never_made_for_an_over_long_script` |
| The palette survives a terminal with no room | `tests/tui_layout.rs::the_action_list_survives_a_terminal_with_no_room`, `::the_palette_keeps_a_gap_between_a_title_and_its_hint` |

### Keyboard and mouse

| Behaviour | Evidence |
|---|---|
| `1`–`6`, tab/shift-tab, arrows, page keys, home, `r`, `:`, `q`, ctrl-c | `tests/tui_layout.rs::every_tab_is_reachable_from_the_keyboard`, `::the_bar_names_the_keys_that_work` |
| A click on a tab label opens that tab, on any line of the strip | `tests/tui_mouse.rs::clicking_a_tab_label_switches_to_that_tab`, `::clicking_a_tab_on_the_second_strip_line_opens_it` |
| Separators, the top bar and the status line are not targets | `::clicking_between_labels_leaves_the_tab_alone` |
| The wheel scrolls only the open tab, and stops at the top | `::the_wheel_scrolls_the_open_tab_and_stops_at_the_top`, `::the_wheel_scrolls_only_the_open_tab` |
| A click never starts an action, and is ignored while a dialogue is open | `::a_click_never_starts_an_action`, `::a_click_is_ignored_while_a_dialogue_is_open` |
| The hover mark follows the pointer and marks no header | `::the_row_under_the_pointer_is_marked`, `::a_pointer_over_the_header_marks_no_row` |
| A real terminal size (191×50) keeps rows clickable | `::a_table_row_is_clickable_at_a_real_terminal_size` |

### Responsive layout and resizing

| Behaviour | Evidence |
|---|---|
| Six tabs named at 40/80/100/120/191 columns, wrapping when they must | `tests/tui_layout.rs::a_narrow_terminal_still_names_every_tab` |
| The body survives 80×24 without losing its answers | `::the_view_fits_a_small_terminal_without_losing_the_databases`, `tests/tui_browser.rs::a_short_rows_browser_screen_keeps_the_whole_fetched_page_visible` |
| Growing the terminal releases a list scroll that is no longer needed | `tests/tui_page.rs::moving_past_the_visible_list_keeps_the_selection_on_screen` (second half) |
| Zero-sized and tiny terminals do not panic | `tests/tui_layout.rs::the_action_list_survives_a_terminal_with_no_room` |

### Terminal startup, errors and teardown

| Behaviour | Evidence |
|---|---|
| The view enters and leaves the alternate screen and leaves a working shell | `tests/tui_terminal.rs::the_view_draws_the_gateway_and_gives_the_terminal_back`, `::the_view_leaves_a_shell_that_works` |
| A bare `dg` opens the view on a terminal, and prints usage to stderr off one | `::a_bare_invocation_opens_the_view_on_a_terminal`, `::a_bare_invocation_off_a_terminal_prints_usage_and_changes_nothing` |
| Explicit `dg tui` refuses redirected stdin before taking the screen | `::explicit_tui_refuses_redirected_input_before_taking_over_the_terminal` |
| Restoration happens even if entry fails half-way, or the view panics | `src/tui/screen.rs::a_partial_entry_failure_still_gives_the_terminal_back`, `::a_panic_still_gives_the_terminal_back`, `::the_screen_is_given_back_exactly_once` |
| An unreachable or partly-refusing gateway says so without looking healthy | `tests/tui_refresh.rs::the_view_says_connected_when_the_gateway_answered_something`, `::the_view_says_unreachable_when_nothing_answered`, `tests/tui_layout.rs::an_unreachable_gateway_says_so_rather_than_looking_healthy` |
| A long error is cut and says that it was cut | `tests/tui_layout.rs::a_long_error_is_cut_and_says_so` |

## Findings

Severity is what the defect costs an operator: **high** = wrong answer about
the data, or an unusable terminal; **medium** = a control that lies or a
reachable state that cannot be left; **low** = cosmetic or documentation.

### F1 — A rows browser on a short terminal hid fetched rows behind a footer that promised them (high)

**Repro.** `tests/tui_browser.rs::a_short_rows_browser_screen_keeps_the_whole_fetched_page_visible`:
draw a 10-row page at 120×16. The footer read `rows 1-10 of 62` while only
`ROW-01`…`ROW-05` were on screen. The pane spent four framed lines on the
headline, database, table and grid frame, so a short terminal clipped the page
it claimed to show, and `n` moved to the *next* page — the missing rows were
unreachable by any key.

**Fix.** Below 20 body lines the browser draws a compact form: one identity
line, the grid, one footer line (`compact_rows_pane`, `src/tui/draw.rs:1407`).
When even that cannot show all ten rows, the arrow keys and the wheel scroll
within the fetched page and the footer adds `visible 3-8 of 10 fetched`
(`browser_scroll_rows`, `src/tui/mod.rs:999`); `n`/`p`, left/right and page
up/down still change the query page.

**Tests.** `a_short_rows_browser_screen_keeps_the_whole_fetched_page_visible`,
`the_smallest_rows_browser_scrolls_the_fetched_page_without_losing_rows`.

### F2 — A filtered rows browser reported the unfiltered total (high)

**Repro.** Open a database, filter a table through the browser (`f`), then
let the database page re-read the selected table. `load_selected_table` set
`total` from the structure's row count, so the footer said `rows 1-10 of 60`
for an answer of 24 rows and paging disagreed with the count.

**Fix.** `load_selected_table` (`src/tui/mod.rs:1571`) reads
`views::rows_count_query` under the active filter and uses that count; the
structure total is used only when there is no filter.

**Test.** `tests/tui_refresh.rs::database_page_keeps_its_rows_browser_page_when_reloading`.

### F3 — Re-reading the database page threw away the browser's page (medium)

**Repro.** In the same path: press `n` in the rows browser opened from a
database page, then let the page re-read. `load_selected_table` built
`Browser::new`, resetting page 1 to page 0 — the key did nothing after the
read, and the operator could not stay on a page.

**Fix.** The browser state is reused when it is on the same
database/table, so the page, sort and filter survive the re-read; a different
table starts fresh.

**Test.** `database_page_keeps_its_rows_browser_page_when_reloading` (asserts
page 1 is still page 1 after the reload).

### F4 — A mixed-case table name could not be opened by clicking it (medium)

**Repro.** `tests/tui_mouse.rs::clicking_a_mixed_case_table_name_opens_it`:
a table named `UserData` is listed in the opened database; a click on its row
queued nothing. The click path validated the name with the *database* name
rule (`store::validate_name`), which refuses mixed case, while the palette
path used the table rule (`actions::valid_table`) that accepts it — so the
same table was reachable by keyboard and not by mouse.

**Fix.** `table_under_pointer` (`src/tui/mod.rs:795`) uses the one table-name
policy, `actions::valid_table`. Unsafe spellings are still refused.

**Test.** `clicking_a_mixed_case_table_name_opens_it` (plus the existing
`clicking_a_column_opens_nothing` for the negative case).

### F5 — `dg tui` took the terminal over with stdin redirected (high)

**Repro.** `tests/tui_terminal.rs::explicit_tui_refuses_redirected_input_before_taking_over_the_terminal`:
run `dg tui … </dev/null` in a pty. The command checked only stdout, so it
entered the alternate screen, then blocked forever on an input stream that
would never produce an event — the pty test timed out and had to kill it.

**Fix.** `src/main.rs:1033` requires both stdin and stdout to be terminals
before the view starts, matching the bare-invocation guard.

**Test.** `explicit_tui_refuses_redirected_input_before_taking_over_the_terminal`
(exits non-zero, reports `not_a_terminal`, and never writes `ESC[?1049h`).

### F6 — A half-completed terminal entry left the terminal broken (high)

**Repro.** Read of `src/tui/mod.rs::run` and `src/tui/screen.rs::enter`:
raw mode is enabled first, then the alternate screen and mouse reporting are
written, then the cursor is hidden. The restoration guard was constructed only
after `enter()` returned `Ok`, so a failure in any of those steps left the
operator in raw mode with no echo and no cursor — the exact state the guard
exists to prevent.

**Fix.** The guard takes ownership before entry and performs the entry itself
(`Guard::enter`, `src/tui/screen.rs:108`), so `Drop` restores a partially
entered terminal.

**Test.** `src/tui/screen.rs::a_partial_entry_failure_still_gives_the_terminal_back`
(a screen whose `enter` fails records `partial-enter` then `restore`), plus
the pre-existing panic and normal-exit tests.

### F7 — A one-time credential stayed on screen after its result was left (medium)

**Repro.** Issuing a key or rotating the owner credential puts the raw key in
the outcome pane. That pane outlives tab switches and manual refreshes, so a
secret that was meant to be shown once stayed visible after the operator moved
to another tab or asked for a fresh read.

**Fix.** An outcome carrying a `key` fact is cleared when the keyboard leaves
its tab (`focus_on`, `src/tui/mod.rs:729`) and on manual refresh
(`clear_secret_outcome`, `src/tui/mod.rs:737`). It stays while the operator
remains on the result, which is when it has to be readable.

**Tests.** `tests/tui_page.rs::leaving_a_tab_clears_a_one_time_credential_outcome`,
`tests/tui_refresh.rs::manual_refresh_clears_a_one_time_key_result`.

### F8 — A failed contents read kept stale counts and marked them current (high)

**Repro.** `tests/tui_refresh.rs::a_failed_contents_read_shows_unknown_counts_and_retries`:
let `/v1/contents` start failing after a successful read. The databases tab
kept drawing the old `tables`/`rows` — indistinguishable from fresh figures —
and the failure was neither reported nor retried, because the clock advanced
regardless of the outcome.

**Fix.** `refresh_contents` (`src/tui/mod.rs:1804`) replaces the counts with
explicit `null`s (drawn as `unknown`), reports the error through the bar and
leaves `contents_refreshed` unset so the next poll retries.

**Test.** `a_failed_contents_read_shows_unknown_counts_and_retries`.

### F9 — The tab strip clipped at narrow widths (high)

**Repro.** `tests/tui_layout.rs::a_narrow_terminal_still_names_every_tab` at
40 columns: the strip drew `3 backup` and then nothing — `keys`, `queries`
and `locations` were off the line entirely, and the recorded click geometry
covered labels that were never painted. The strip had one line and no way to
grow.

**Fix.** The strip measures how many lines its names need and wraps
(`tab_strip`/`strip_lines`/`draw_strip`, `src/tui/draw.rs:709`), the body is
laid out below the measured height, and each label records its own row and
column (`TabGeometry`, `src/tui/draw.rs:60`) so a click on any line resolves
to the right tab. Counts still leave before names do.

**Tests.** `a_narrow_terminal_still_names_every_tab` (40/80/100/120/191),
`tests/tui_mouse.rs::clicking_a_tab_on_the_second_strip_line_opens_it`.

### F10 — The database page's table list could scroll away from its selection (medium)

**Repro.** `tests/tui_page.rs::moving_past_the_visible_list_keeps_the_selection_on_screen`
with 30 tables on a short screen: the selection moved below the fold, and
because clicks were resolved against an assumed offset, a click on a visible
name could select a different table
(`a_click_after_list_scroll_selects_the_table_at_that_visible_row`).

**Fix.** `DatabasePage::keep_selection_visible` clamps the scroll so the
selection is always drawn, and the drawn list records its real area, start and
length (`PageList`) so a click maps to the row that was painted.

**Tests.** `moving_past_the_visible_list_keeps_the_selection_on_screen`,
`a_click_after_list_scroll_selects_the_table_at_that_visible_row`.

### F11 — The wheel acted on the wrong thing inside an opened database (medium)

**Repro.** `tests/tui_page.rs::the_wheel_over_values_pages_rows_without_changing_the_table`
and `tests/tui_browser.rs::wheel_events_outside_the_body_do_not_change_the_browser_page`:
a wheel over the values column moved the *table selection* instead of the
rows, and a wheel over the chrome (top bar, status line) changed the browser
page anyway.

**Fix.** The wheel is routed by where the pointer really is: over the table
list it moves the selection, over the values it scrolls the fetched rows (or
pages when they all fit), outside the body it does nothing.

**Tests.** those two, plus `the_table_list_wheel_stops_at_the_last_table`.

### F12 — A manual refresh inside the contents window reused old counts (medium)

**Repro.** `tests/tui_refresh.rs::a_manual_refresh_re_reads_the_contents` and
`::palette_refresh_invalidates_contents_inside_the_cadence_window`: pressing
`r` (or choosing *refresh* in the palette) within 30 seconds of the last
contents read reported the cached counts. The counts clock exists to keep an
*idle* view from counting every row every two seconds; it was never a promise
that `r` answers with old numbers.

**Fix.** `Invocation::Refresh` invalidates the contents clock before reading
(`contents_stale`, `src/tui/mod.rs:1851`), from both the `r` key and the
palette.

**Tests.** both of the above.

### F13 — A successful write did not refresh the databases tab's counts (medium)

**Repro.** `tests/tui_refresh.rs::a_successful_write_refreshes_database_contents_after_the_write`:
insert a row through the view; the databases tab kept the pre-write row count
until the 30-second window expired.

**Fix.** A write that succeeded through a `/v1/databases…` route (and a
database import, which uses the chunked transfer path) marks the counts stale,
so the refresh that follows the action re-reads them. Key-management writes do
not, because they change no database's contents.

**Test.** `a_successful_write_refreshes_database_contents_after_the_write`.

### F14 — The rows browser's generated SELECT was classified as a write (low)

**Repro.** Read of `src/tui/mod.rs::query_sql`: it built a bare
`client::request` with `POST`, so `Semantics::from(&Method)` classified every
browser read as a write. With an unreachable gateway the row read would have
been reported as *writes may have committed* for a generated `SELECT`.

**Fix.** `query_sql` builds the same typed `Invocation::Query` request the
palette uses and passes its `semantics` through
(`client::request_with_semantics`), so a generated read is a read.

**Test.** `tests/tui_actions.rs::every_request_carries_what_it_can_commit`
pins the classification for a `SELECT`, and
`::a_read_only_query_that_cannot_reach_the_gateway_says_so` pins the wording
that classification produces.

## Checked and found correct (no change)

These were audited because they are where a view like this usually breaks, and
the existing behaviour holds. Each is pinned by a test, so a future change
that breaks it fails the build rather than an operator.

| Area | What was checked | Pin |
|---|---|---|
| Colour | Every text colour meets WCAG AA against the dark background; emphasised ones meet AAA; colour is never the only signal | `src/tui/theme.rs` tests |
| Scroll bounds | No tab can be scrolled into an empty screen; a table that already fits does not scroll | `tests/tui_layout.rs::a_tab_cannot_be_scrolled_into_nothing` |
| Zero-sized terminal | The palette popup does not `u16::clamp`-panic on an empty range | `::the_action_list_survives_a_terminal_with_no_room` |
| Error line | A message longer than the bar is cut and marked with an ellipsis | `::a_long_error_is_cut_and_says_so` |
| Freshness vocabulary | `behind` is drawn dim and is only readable next to the cadence; the reference is named an *objective*, not an age limit | `::the_freshness_reference_is_named_as_an_objective_not_as_an_age_threshold` |
| Destructive reach | No pointer gesture can reach delete/revoke/rotate; they need their typed confirmation | `tests/tui_mouse.rs::a_click_never_starts_an_action`, `tests/tui_actions.rs::a_destructive_action_has_to_be_confirmed_by_name` |
| Read/write honesty | The browser and the palette classify a request by its statements, with the server's rule | `tests/tui_actions.rs::every_request_carries_what_it_can_commit` |
| CLI parity | The CLI and the view draw the same numbers from the shared view layer | `tests/tui_view.rs::the_cli_and_the_view_show_the_same_numbers` |
| Text hygiene | A cell from the wire is escaped at the one point it becomes terminal bytes | `src/render.rs` tests, exercised through every grid |

## Explicit dispositions

- **Twelve data files under `docs/evidence/tui-live-2026-10-25-raw/` and the
  older `docs/evidence/tui-view-2026-10-22.txt` stay historical.** They predate
  the tabbed layout, and `docs/tui.md` says so where it links them. Rewriting a
  capture would destroy the only record of what that build drew.
- **The `_dg_*` and `_litestream*` tables stay hidden from the structure view**
  but remain visible in `dg info`. That asymmetry is deliberate: the structure
  view answers "what is my schema", and `dg info` answers "what is in this
  file". Both behaviours are documented and unchanged by this audit.
- **No Turso call was made.** The objective excludes it, and nothing in the
  view needs it: every route it uses is the gateway's own.
- **The long-running gateway was not modified.** The runtime evidence came
  from a disposable gateway created by the capture script; the local gateway
  still lists its seven databases, and no live exercise ran against it.

## Runtime evidence

`scripts/capture-tui-evidence.sh docs/evidence/tui-audit-runtime.log docs/evidence/tui-audit-runtime.txt`
created a fresh gateway in a temporary directory, seeded one table, published
a backup set, wrote after it, added a second database, then drove the view in
a 120×40 pty: `:` → `query` → `demo select * from players` → Enter → `3`
(backups tab) → `q`.

The captured stream contains, in order:

- startup: `ESC[?1049h` (alternate screen entered) exactly once;
- the palette and query interaction: the action named, then the outcome pane
  reading `3 rows` with the `rows` grid below it listing `aria`, `bo`, `cy`;
- tab navigation: the strip with the caret on `3 backups`, the freshness table
  showing `demo … behind … 5s lag` beside `snapshot cadence: 1h 0m`, and the
  published-sets table;
- teardown: `ESC[?1049l` (alternate screen left) exactly once, and the process
  exiting cleanly.

The rendered frames are in `docs/evidence/tui-audit-runtime.txt`.

## Gate results

| Gate | Command | Result |
|---|---|---|
| Workspace tests | `cargo test --workspace --all-targets` | 91 suites, 663 passed, 0 failed |
| Formatting | `cargo fmt --check` | clean |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` | no issues, no suppressions added |
| TUI suites | `cargo test --locked --test tui_layout --test tui_page --test tui_browser --test tui_mouse --test tui_refresh --test tui_terminal --test tui_actions --test tui_schema --test tui_tables --test tui_view` | pass |
