# dagpane

**A reactive runtime for data apps: an interaction recomputes the cells that depend on it, and nothing else.**

You write an app as a TOML manifest of cells. Each cell names the inputs it reads. `dagpane`
builds a graph from those declarations once, orders it by height, and when an input moves it
evaluates only the cells downstream of the change, then sends a patch rather than a page.

The image is the binary and the app you give it. `scratch` plus one static musl executable:
no shell, no package manager, no libc, no Python and no Node. The front end is compiled into
the executable as a single HTML file, so there is no build step to reproduce inside the
container and nothing for it to fetch at run time.

## Read this before you expose it

**There is no authentication in this version.** `dagpane run` binds loopback by default and
warns when told to bind anything else. Inside a container, "anything else" is the only useful
choice, which is why every example below passes `--host 0.0.0.0`.

Put it behind something that authenticates before it is reachable by anyone you would not
show the data to. Whoever can open the page can move every input in the app and read every
value it computes.

## Run it

Run these from the directory holding your app manifest and its data.

```sh
docker pull mancube/dagpane:0.1.0

# what version is in here
docker run --rm mancube/dagpane:0.1.0 --version

# check an app without running it, and print the graph it would build
docker run --rm -v "$PWD:/app:ro" mancube/dagpane:0.1.0 check /app/your-app.toml
docker run --rm -v "$PWD:/app:ro" mancube/dagpane:0.1.0 graph /app/your-app.toml

# serve it
docker run --rm -p 8787:8787 -v "$PWD:/app:ro" \
  mancube/dagpane:0.1.0 run /app/your-app.toml --host 0.0.0.0
```

The app is mounted rather than baked in. There is no shell in the image and no example inside
it, so a bind mount is both the only way and the way you would really run this. Read-only is
enough: `dagpane` reads the manifest and its data sources and writes nothing back.

To try the bundled example, clone the source repository and mount its `examples` directory as
`/app`; `sales.toml` is in there.

`check` and `graph` answer questions about an app that has not run yet, which is possible
precisely because a cell's inputs are written down rather than discovered while evaluating.
`explain` answers the question the project exists to answer: what did that interaction
actually cost.

## What you get back

`explain` and the live page report counts, not timings. The subcommand takes the same
arguments in the container as it does on a host; this is the recorded run against the bundled
example, 600 rows of synthetic sales in 11 cells and 7 panes:

```console
$ dagpane explain examples/sales.toml --set min_amount=400
dagpane: Sales explorer — 11 cells, 7 panes
first render: 8 of 11 cells evaluated

set min_amount = 400
  epoch 2 — looked at 8 of 11 cells
    set      min_amount
    ran      filtered             changed
    ran      order_count          changed
    ran      region_totals        changed
    ran      channels             same value — nothing below it ran
    ran      top_orders           same value — nothing below it ran
    ran      revenue              changed
    reused   channel_count        its inputs had not moved
  3 cell(s) never looked at: sales, region, all_time_revenue
  patch: 3 of 7 panes — revenue, order_count, region_totals
```

Read the last four lines. `channels` recomputed and produced the value it already held, so
`channel_count` below it did not run at all. Three cells were never looked at, including the
one holding all 600 rows. Three of seven panes went on the wire, because the other four were
still correct.

A count only means something against a named app. "8 of 11 cells" is a fact about that
manifest and that interaction; the same slider move over a differently shaped graph gives a
different number.

There are no timings here and no benchmark claim on this page. The engine is clockless on
purpose, because an engine that needs a clock to be tested behaves differently under a test
harness. Wall-clock microseconds are measured around a pass by the server and reported per
interaction, never aggregated into a headline.

## Tags and platforms

| Tag | What it is |
| --- | --- |
| `0.1.0` | The release. Immutable once published. |
| `latest` | The most recent non-prerelease. Moves. Pin the version for anything that matters. |

**`linux/amd64` only.** The build pins the `x86_64-unknown-linux-musl` target, and publishing
an `arm64` manifest whose binary is `x86_64` would advertise a container that cannot start.
One honest architecture beats two where one is a lie. On Apple Silicon this runs under
emulation.

The binary is verified static inside the builder, so a dynamic-linking regression is a build
failure rather than a surprise on your machine.

## What is in the image

```
/dagpane     the binary, the entrypoint
/LICENSE     Apache-2.0
/NOTICE      the prior art this engine owes, which is most of it
```

`EXPOSE 8787`. The default command is `--help`.

## Where it is built

The image is built and pushed by the public repository that holds its source, from exactly
the tree published there, on the release tag. Nothing is compiled from source you cannot
read.

## Source, issues and documentation

https://github.com/lucheeseng827/dagpane

`ARCHITECTURE.md` explains the engine, the four architecture decision records cover the
choices that constrain it, and `SECURITY.md` covers the authentication gap above in full.

Apache-2.0.
