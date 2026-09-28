# What one more viewer costs

Two rigs and one question, split the way an operator meets it: a viewer who is **holding** a
socket and a viewer who is **using** it are two different costs, and only the first of them was
ever measured here.

```sh
./benches/sessions/run.sh     # held:  what it costs to have a viewer
./benches/sessions/busy.sh    # busy:  what it costs while they drag a slider
```

`run.sh` is the original rig and `ROADMAP.md` §7's first item. `busy.sh` is the exclusion that
rig wrote into its own method — *"an interaction allocates transiently and would be measured as
noise"* — collected rather than left as a footnote.

Both are bare Node with no npm dependency, and both are Linux only: every reading comes from
`/proc`, which is the kernel's own accounting rather than a userspace guess. The instrument and
the estimator live in `lib.mjs`, shared, so the two answers can be put side by side.

---

## Part one — a viewer who is holding

`ROADMAP.md` §7 named the gap precisely:

> `Arc::ptr_eq` on an untouched source across two sessions is asserted by a test rather than
> inferred from an RSS reading.

So the sharing this project's memory story rests on was proven structurally and never weighed.

### The method

One app, one `dagpane run` process, sessions opened and then left **idle**. Ramped 0, 1, 2, 4 …
512, and the answer is the **slope**, by least squares over the whole range — not a subtraction
between two rungs, because a process's RSS includes an allocator that grows in steps and does
not give pages back, so one subtraction at one N measures the step. `smaps_rollup` rather than
`ps`, for `Rss` and `Pss` both. Five samples a rung after a 700 ms settle, median taken.

### The experiment, which is the point

Three ramps, and the comparison between them is the result rather than any one of them:

1. the bundled 600-row example;
2. **the same pipeline over 200 000 rows**;
3. **the same 200 000 rows, aggregates only** — no cell downstream of the source keeps a row.

Two against three is the whole thing. Same data, same process, same sockets, same number of
open connections. The only difference is whether anything downstream holds onto rows.

| app | source | per session | ratio to source |
|---|---:|---:|---:|
| 600 rows, keeps rows | 0.02 MB | 179 kB | 8.46× |
| 200 000 rows, keeps rows | 7.6 MB | **7 248 kB** | 0.935× |
| 200 000 rows, aggregates only | 7.6 MB | **145 kB** | 0.019× |

**The source is shared, and this is the measurement that says so.** If a session copied the
sources, row 3 would cost about 7.6 MB and it costs 145 kB. `Arc::ptr_eq` was telling the truth.

**The derived cells are not shared, and cannot be.** Every viewer sets their own controls, so
every viewer holds their own computed frames — and a cell that *keeps rows* holds about a
table's worth. That is not a leak and not a bug: two viewers filtering differently must have two
answers. It is simply the cost, and it had never been priced.

**≈145 kB is the floor** — a socket's buffers and a slot vector, with nothing materialised
behind them. The 600-row app's 179 kB is that floor plus its small frames.

---

## Part two — a viewer who is using it

The held rig excluded interaction deliberately and left "not zero" where a number should be.
"Not zero" is not something an operator can size against, and the shape of the answer was not
obvious either: a pass could cost a frame, or a whole pipeline, or nothing at all if the engine
computed in place.

### The instrument, which is why this is measurable

A pass over the bundled example takes about a third of a millisecond. **A sampler cannot see
it.** Read RSS every 50 ms and you will observe an interaction costing nothing, roughly a
hundred and fifty times in a hundred and fifty-one, and publish it.

So the peak here is not sampled. `VmHWM` in `/proc/PID/status` is a high-water mark the kernel
maintains on every page fault, and writing `5` to `/proc/PID/clear_refs` resets it to the
process's current RSS. Each window therefore reports its true peak however briefly it was
touched, and each window's peak is its own rather than the run's.

### The method, and the three things it got wrong first

One process, one app, a **fixed** number of viewers held open — so the held cost is in the
baseline and out of the answer — and a ramp in how many of them are dragging a slider as fast as
the server will answer. Closed loop, deliberately: an arrival rate is a second parameter whose
value would have to be defended, and the ceiling is what an operator has to have room for.

Then three corrections, each of which is a finding about the thing being measured:

1. **A fresh process per rung.** The first version walked one process up the ladder, and the
   first thing it found is that *after a burst, resident memory does not come back*. Every later
   rung therefore started from the previous rung's peak and measured how much **more** than that
   it needed — printing a jumping sequence of numbers as if it were the cost of a rising amount
   of work.
2. **The same burst twice.** Memory that does not come back has two explanations with nothing in
   common but the shape of one reading. A leak grows every time the work is done; a plateau does
   not. The second identical burst is the test.
3. **Three processes a rung, medianed.** `peak − idle` is how much *new* memory a burst has to
   fault in, so it depends on what the allocator was already holding free when the burst began.
   On an app whose sessions materialise little, two runs of one rung came back 12 MB and 0 kB —
   both true of their own process. The spread is printed beside the median rather than hidden
   by it.

Two controls are built into the table rather than argued for. The **K = 0** rung drives nobody,
so a peak above its idle reading would be the instrument measuring something other than the
experiment. And every rung holds the same viewers on a fresh process, so an **idle column that
drifts** across rungs is a rung that was not independent after all.

One viewer is held back from the ramp and never drags anything. It asks the server for the state
it already has every 50 ms and times the answer. That is the half of this cost which is not paid
by the viewer who is busy.

### What it found

**A pass costs resident memory, and the process does not give it back.** Not a leak — the same
burst fired a second time is nearly free, so the process reached a working set rather than
losing memory. What it means operationally is that a viewer who has *ever* touched a control
costs more, from then on, than one who never has.

| app | to hold a viewer | one pass in flight | 64 dragging at once | still resident once quiet |
|---|---:|---:|---:|---:|
| 600 rows, keeps rows | 179 kB | *see below* | 1 368 kB | 1 368 kB |
| 200 000 rows, keeps rows | 7 248 kB | **16 636 kB** | 129 084 kB | 126 240 kB |
| 200 000 rows, aggregates only | 145 kB | **0 kB** | 25 812 kB | 25 812 kB |

Read the last two columns together: **"still resident" is the same number as "64 dragging"** at
twenty of the twenty-four rungs across the three ramps. The burst's peak becomes the floor.
Four rungs give something back, and the amounts sort them: 0.07% and 0.2% are rounding, 2.2% at
the row-keeping app's top rung is small, and **23%** at that app's two-dragging rung is the one
place in the set where a real fraction returns. An earlier run of the same ramp returned 18% and
13% at two *different* rungs, so which rung gives memory back is not stable; that a few do, and
that most do not, is.

**One figure in these tables is not a measurement and is now labelled as one.** The bundled
example's *one pass in flight* has come out at **164, 244, 188, 92 and 92 kB** across five
independent runs of the same rung, and the three processes inside the last of those spread 128 kB
around a median of 92. The spread is larger than the number. That app's pass allocates a few tens
of kilobytes — a couple of dozen pages — so the reading is dominated by whatever the allocator
happened to round to, and no amount of repetition fixes a quantity that small against a
page-granular instrument.

Everything else in the same ramps is stable: that app's **ceiling** over the same five runs was
1 316, 1 324, 1 324, 1 368 and 1 368 kB, and the 200 000-row app's one-pass figure was 19 956,
17 532, 19 720 and 16 636 kB. The rule is the obvious one and worth stating because this rig
invites the mistake: **a transient of a few tens of kB is at the floor of what `VmHWM` can
resolve.** Read the small app's row for its shape, not its value.

**The charge is per viewer who has interacted, not per interaction.** Quadrupling the window — so
every viewer does four passes instead of one — leaves the ceiling where it was. Firing the
identical burst again costs about a **tenth** of the first in all three ramps, and a window ten
times longer — so ten times the passes — raises the ceiling by about a **quarter** rather than by
ten times. Fit a line to the upper half of each ramp and its slope is what one more *interacting*
viewer costs for good:

| app | held, per viewer | extra, once they interact |
|---|---:|---:|
| 600 rows, keeps rows | 179 kB | ≈ 12 kB |
| 200 000 rows, keeps rows | 7 248 kB | **≈ 1 449 kB** |
| 200 000 rows, aggregates only | 145 kB | **flat** — see below |

The middle row is the one to plan against, and the third is why: **an app holding 200 000 rows
adds nothing measurable per interacting viewer, provided nothing downstream of its source keeps
a row.** On the aggregating ramp a line is the wrong model in the first place — that ramp does
not rise at all past four dragging viewers, and a least-squares fit to it came out slightly
*negative* last run and slightly *positive* (≈ 4 kB a viewer) this one. A slope whose sign is
decided by which run you took is the honest way of saying flat.

**The spread is real and the rig prints it.** The three processes behind that 129 084 kB wanted
123, 126 and 130 MB — ±3%, where the same rung on the rig's earlier shape spread ±30%. The
*shape* barely wavers either way: `retained` tracked `transient` in every repetition of every
rung of every ramp bar the four noted above.

### The two 200 000-row apps again, and they part company differently this time

The held measurement put these two 50× apart on identical data. Under load they are not 50×
apart, and more importantly they are not the same *shape*:

| viewers dragging | keeps rows | aggregates only |
|---:|---:|---:|
| 1 | 16 636 kB | **0 kB** |
| 4 | 37 316 kB | 20 764 kB |
| 8 | 49 136 kB | 25 708 kB |
| 16 | 57 604 kB | 25 544 kB |
| 32 | 82 728 kB | 26 624 kB |
| 64 | **129 084 kB** | **25 812 kB** |

**The aggregating app stops. The row-keeping one does not.** Past about four concurrent
viewers — the number of tokio workers on this machine — the aggregating app has reached its
ceiling and sixteen times as many viewers do not move it, because the thing a pass needs there
is a *scratch buffer* and only as many are needed as there are passes running at once. The
row-keeping app keeps climbing, because every viewer that passes ends up holding a *different*
allocation from the one it held before, and that is per viewer rather than per worker.

It is the same materialise-or-not distinction the held rig found, arriving as a shape rather
than as a magnitude. So the advice that came out of the held measurement survives, and for a
sharper reason than it was given: aggregating upstream does not make a pass **free** — `filter`
builds all 200 000 rows before `group_by` throws them away — **it makes the cost stop growing
with the number of viewers.**

The two passes are not the same price, and an earlier draft of this paragraph said they were.
One viewer dragging costs **≈ 16 MB** on the row-keeping pipeline, which ends the pass holding
a fresh allocation of everything it materialised, against **0 MB** on the aggregating one,
whose roughly 6 MB scratch buffer was already resident. The 6 MB is what a *second* concurrent
pass has to fault in, and that is what the ceiling is made of — not what the first one costs.

One more reading worth keeping, because it is the most surprising cell in the table: the
aggregating app's **first** dragging viewer costs **0 kB**, reproducibly, in all three
processes. Its pass needs a 200 000-row scratch buffer and one is already resident and free —
sixty-five sessions opened, each of which ran that same pipeline once at connect, and the
allocator kept the pages. Nothing new has to be faulted in until a *second* pass wants a buffer
at the same time.

### And the half that is not paid by the viewer who is busy

`apply` calls `session.commit()` inline in the connection's `async fn`. A pass therefore
occupies a tokio worker for its whole duration, and there are as many workers as the affinity
mask allows — four here. So the viewer held back from the ramp, asking only for the state it
already holds, waits behind everyone else's arithmetic:

| viewers dragging | 600 rows<br>(pass 0.5 ms) | 200k aggregating<br>(pass 25.6 ms) | 200k keeps rows<br>(pass 147.9 ms) |
|---:|---:|---:|---:|
| 0 | 0.4 ms | 0.3 ms | 0.4 ms |
| 16 | 1.8 ms | 75.7 ms | 497.9 ms |
| 32 | 4.1 ms | 195.7 ms | 919.9 ms |
| 64 | **9.0 ms** | **418.1 ms** | **2 248.5 ms** |

Every cell is a median over the probes that **came back**, and the ones that did not are
counted rather than folded in. That distinction is the whole of the next paragraph, and an
earlier version of this table got it wrong in a way worth printing.

The K = 0 row is the control that matters: all sixty-five sockets are open there too, so what
the rows below measure is the work and not the connections.

**The last version of this table published 431.6 ms in the bottom-right cell, and the reason it
was wrong is the most useful thing in this file.** A probe still owed an answer when the window
shuts is recorded *censored*, at the time on the clock when the window shut — known to be too
small, by an unknown amount. The rig folded those into the same median as the real observations.
Where nothing is censored that changes nothing; at the worst rung of the worst app, where 3 of 11
probes never returned, it dragged the median from 2 248.5 ms down to 431.6 ms — *below* the same
app's thirty-two-viewer rung. Nothing in a server gets faster when you double the load, and that
impossible row is what the mistake looked like from outside.

**The diagnosis published alongside it was wrong too, and in the more interesting direction.**
The inversion was blamed on the three-second *window* being too narrow to see the rung, and the
thirty-second measurement was presented as what rescued the rule. It was not the window. Over
completed probes the three-second ramp agrees with the rule at **every rung of all three apps**,
that rung included, and agrees with the thirty-second measurement of the same rung to within 4%.
The aperture was never the error; the aggregation was. `busy-session.mjs` now aggregates the two
apart, keeping the mixed figures beside them under `_with_censored`, and prints the count of what
never came back as the rung's health warning.

**Divide each cell by that app's own pass and the three columns become one number.** The
bystander waits, in passes:

| viewers dragging | 600 rows | 200k aggregating | 200k keeps rows | viewers ÷ workers |
|---:|---:|---:|---:|---:|
| 16 | 3.9 | 3.0 | 3.5 | 4 |
| 32 | 8.7 | 7.6 | 6.3 | 8 |
| 64 | 18.8 | 16.3 | 15.2 | 16 |

Each cell divides that rung's own pass cost, not the column header's: the pass drifts a little
across a ramp (143.8 to 147.9 ms on the row-keeping app) and dividing every rung by one figure
would put that drift into the ratio. The thirty-second re-measurement of the bottom-right rung
divides out to **17.6** — 2 343.1 ms over that run's own 133.5 ms pass — against 15.2 here, which
is the agreement the previous paragraph is about.

Three apps spanning **more than 300× in pass cost** agree, which is worth more than any of the
absolute figures above it:

> **A viewer who is doing nothing waits about (busy viewers ÷ worker threads) passes.**

**An idle viewer inherits the pass cost of the busiest viewer on the process**, and pass cost
is a property of the manifest. The bundled app's pass is under a millisecond and sixty-four
people hammering it cost a bystander nine; the 200 000-row app's pass is 148 ms and the same
sixty-four cost a bystander a little over two seconds.

This had been reasoned from `crates/serve` for two rounds — `apply` is an ordinary `fn` and
there is no `spawn_blocking` anywhere near it — and was never a number.

**The bystander figure at the top rung rests on five completed probes across three processes,
and that is a fact about the rig rather than about the server.** A wait longer than the window
cannot be observed at all: the rig records the outstanding probe as *censored* and reports a
lower bound. At this rung each process gets two or three probes away and has one of them cut
short, so the rung's whole claim rests on what is left.

**The raw samples are what showed the aggregation was wrong**, and they are the reason this
directory carries them. Over completed probes the three processes waited **1 240, 1 315 and
2 310 ms**; over the mixed sample the rig used to publish, the same three read **432, 324 and
2 310 ms**. Two of the three moved by a factor of four, in the direction that flatters, purely
because a censored wait entered the median at the clock time the window shut.

**Run the identical rung at thirty seconds and the same three processes report 2 254, 2 310 and
2 547 ms**, on thirteen or fourteen completed probes each. So the spread was the sample size and
not the server: with enough probes, three processes that looked unrelated land within 6% of one
another and within a tenth of what the rule predicts. That is the lesson below, arrived at from
the direction of the raw samples rather than from the direction of the conclusion.

**So the conclusion drawn from it — that a 25 s keepalive has an order of magnitude of margin —
was not supported, and is withdrawn.** A three-second window cannot produce a number above
about three seconds whatever the truth is, so it could not have falsified the claim it was used
to support. That is the worse error of the two: not a wrong number, but a measurement asked a
question it was incapable of answering.

What the rule itself says is the part worth keeping, and it is not reassuring: the wait is the
app's pass times viewers over workers, so an app with a slower pass or a host with fewer
workers walks toward a keepalive interval without anything else changing.

### The same app, one core against four

`DAGPANE_CORES` runs the server under `taskset`. tokio sizes its worker pool from the affinity
mask, so this moves the **denominator** of everything above rather than the numerator — which is
the only way to tell a rule from a coincidence. Same aggregating app, same ramp.

| viewers dragging | 1 core · 2 threads |  | 4 cores · 5 threads |  |
|---:|---:|---:|---:|---:|
| | transient | bystander | transient | bystander |
| 0 | 0 kB | 0.3 ms | 0 kB | 0.3 ms |
| 1 | **0 kB** | 16.0 ms | 0 kB * | 16.1 ms |
| 4 | **0 kB** | 96.4 ms | 20 712 kB | 10.1 ms |
| 16 | **0 kB** | 423.2 ms | 23 928 kB | 92.2 ms |
| 64 | **0 kB** | 822.9 ms ‡ | 26 548 kB | 420.1 ms |

**On one core the transient is zero at every rung, and that is the mechanism stated as an
experiment.** What a burst has to fault in is the scratch buffer of the *second and later*
concurrent pass; the first one's buffer is already resident, left behind by the sixty-five
sessions that each ran the pipeline once at connect. With one worker there is never a second
concurrent pass, so nothing new is ever needed — however many viewers are dragging.

**And the bystander's wait tracks the worker count, not the machine.** Against the rule
(busy ÷ workers) × pass:

| viewers dragging | 1 core: predicted / measured | 4 cores: predicted / measured |
|---:|---:|---:|
| 4 | 89 ms / **96.4 ms** | 25 ms / 10.1 ms |
| 16 | 365 ms / **423.2 ms** | 101 ms / **92.2 ms** |
| 64 | 1 443 ms / 822.9 ms ‡ | 405 ms / **420.1 ms** |

‡ **Half that rung's probes never came back** — six of twelve on one core — so its median is
over the six that were quick enough to be seen, and it is the one cell here not worth reading.
It is left in rather than blanked because a rung that loses half its samples should be visible
as such; the sixteen-viewer row above it is the one that tests the rule on one core.

It is close wherever there are more viewers than workers and optimistic below that, for the
obvious reason: with four workers and four busy viewers a bystander often finds one free.

So the trade is explicit. **A core buys throughput and it buys everybody else's latency; it
costs peak memory.** Four cores here serve 146 passes/s against one core's 40, answer the
bystander 4.6× sooner at sixteen dragging viewers — the last rung on one core where enough
probes come back to say — and hold 26 MB more at the peak.

\* **That starred cell is a median hiding a coin flip, and a previous draft of this file got it
wrong in the confident direction.** At a single dragging viewer the three 4-core processes
wanted **0 kB, 11 596 kB and 0 kB**. An earlier two-repetition run had produced 0 and 11 596 and
was averaged into a meaningless 5 798 kB; a three-repetition run then produced 0 in all three,
and this file concluded that repetition had settled it. It had not — the same split is back, and
the same 11 596 kB with it.

The bimodality is the answer rather than the noise. One dragging viewer with four workers is
exactly the boundary where a *second* concurrent pass may or may not happen, depending on
whether the bystander's 50 ms probe lands inside the dragger's pass. Below that boundary (one
core) there can never be a second, and the transient is 0 kB in every process of every run.
Above it (four dragging viewers) there always is, and the transient is 20 MB. At the boundary
the process gets one or the other, which is what a median of three is least able to tell you —
so the spread column, not the median, is what this rung is reporting.

### Opening the aperture: what the three-second window could not see

Every rung above uses a three-second window, and a bystander's wait longer than the window is
unobservable by construction. At sixty-four dragging viewers on the worst app that window answers
**5 of its 11 first-burst probes**, so the figure it prints rests on five samples. It was on
exactly that basis that an earlier draft of this work declared the 25 s keepalive safe — a claim
the measurement could not have contradicted.

So the same rung was run again at **thirty seconds**, ten times the aperture. A probe still owed
an answer when the window closes is recorded **censored** — a lower bound, not an observation — so
the two bursts and the two kinds of sample are kept apart below, because pooling them is exactly
the mistake the previous section is about.

**What the wider window bought was not a different answer.** Three seconds, over the probes that
came back, put that rung at **2 248.5 ms**; thirty seconds puts it at **2 343.1 ms**. They agree
to 4%, and both sit within a tenth of what the rule predicts from their own pass costs. What
thirty seconds bought is the right to say so: five completed probes became thirty-eight, and the
three-second repeat burst on the same processes gave 159 ms from three samples, which is the same
statistic saying nothing at all. **A rung with five samples is not wrong, it is unfalsifiable**,
and that is the honest reason to widen the window rather than the one first given here.

| 64 dragging · 200 000 rows · three processes | first burst | the identical repeat |
|---|---:|---:|
| probes issued | 41 | 40 |
| answered inside the window | **38** | **37** |
| p50 of those | **2 343 ms** | **2 285 ms** |
| longest of those | 3 923 ms | **4 905 ms** |
| cut short at window close | 3 | 3 |

**The wait did not grow to fill the window.** Ten times the room did not move the middle: the
three processes put their first-burst completed medians at 2 254, 2 310 and 2 547 ms, and the
repeat burst agrees at 2 285 ms. That is a statement about the 75 probes that came back; the six
that did not are bounded below by their truncation and above by nothing, so they are consistent
with this and cannot confirm it.

**Three things about how this was first written down here were wrong, and all of them were
mine.** The table published two rounds ago gave a "worst observed" of 3 718 ms. That was not a
worst and not an observation of one — it was the **median across the three processes of each
process's own worst**, relabelled as a maximum it never was. It was also the first burst only:
then as now, the longest wait of the pair comes from the **repeat** burst — 4 905 ms here — so
"at worst under four seconds" was under four partly because half the data was not in it. And the
statistic it was drawn from mixed censored lower bounds into its own median, which is the
correction two sections up and the one that took longest to find.

**What the measurement supports, at its own size**: at this load, on the slowest app here, a
bystander's median wait is about 2 343 ms, and the longest wait recorded across all **75 answered
probes** is **4 905 ms** — about **5.1× under** the 25 s keepalive on the single worst thing seen.
A further 6 probes were cut short at window close with lower bounds of 0.46–2.5 s; those are
consistent with the answered ones, and they are also the reason this cannot be stated as a bound.
A wait that never finished inside the window is a wait this rig did not measure.

So the claim is that **this load did not starve the keepalive**, not that it cannot be starved.
The rule is the thing that scales, and the rule says the wait is the app's pass times viewers over
workers — a bystander's median reaches 25 s at roughly ten times this pass cost, and its worst at
about 5.1×. Neither is far: the core ramp above measures a single core at **3.0×** four cores'
bystander wait on the aggregating app, on the host alone. A ceiling a few times above the worst
thing you measured is a margin worth knowing and not a guarantee.

**One thing the wider window did move**: the memory ceiling, from 129 084 kB to 163 304 kB. Ten
times the window buys about a **quarter** more memory, not ten times more — so the charge is
mostly, but not purely, per viewer who has interacted rather than per interaction they make.

### The allocator, and the build that actually ships

Everything above is a glibc build. The `Dockerfile` produces a **static musl binary on
`scratch`**, and musl's allocator is not glibc's — so "the burst's peak becomes the floor" is
exactly the kind of result that might be a property of the allocator rather than of this
runtime. It is.

**A spot check, and labelled as one**: the 200 000-row row-keeping app only, two repetitions
rather than three, four rungs rather than eight. It is enough to say the glibc result does not
transfer and not enough to replace it.

| 65 sessions, 4 dragging | glibc | musl |
|---|---:|---:|
| transient | 37 316 kB | 15 162 kB |
| **retained once quiet** | **+37 316 kB** | **−15 638 kB** |
| pass p50 | 124.8 ms | **1 763.3 ms** |

**On musl the memory comes back, and then some.** `retained` is *negative*: the process ends the
burst smaller than it started it, by 15 MB at four dragging viewers and by 235 MB at sixty-four —
and by 4 MB with only one viewer working. The most likely reading is that musl returns freed pages
eagerly but does the returning during later allocator traffic, so the burst is what finally cleans
up what opening sixty-five sessions left behind. Either way the headline above is glibc's, not
dagpane's.

**And musl is much slower here, but only under contention** — which is a different claim from
"musl is slower", wants a different fix, and needs a rung the spot check did not originally
have. It has one now: the session count is held at sixty-five throughout and only the number
*working* moves, so concurrency is the one thing varying. With **one** dragging viewer a pass
costs 260.8 ms against glibc's 152.1 ms — **1.7×**, the ordinary price of a simpler allocator.
Put **four** of the same sixty-five to work and it is **1 763 ms against 125 ms**, more than
**fourteen times** worse, and at sixty-four it is fourteen times still. This runtime
allocates heavily per pass — `filter` builds a whole new table — and mallocng serialises where
glibc's per-thread arenas do not.

**None of that is a benchmark note.** Every published figure in this file is a glibc figure and
the container is musl, so the one an operator runs is slower than the ones here under exactly
the conditions an operator cares about. Which allocator to ship is a decision this file does not
make; that the decision exists and was never taken deliberately is what the spot check found.

### What was done to the rig to see whether it was measuring anything

Three mutations, and the interesting one is the mutation that **failed to break anything**.

**The value cursor, frozen** — every viewer sends the value it is already holding. The engine
reuses, no pass runs, and the window measures an idle process. *Caught*: `patch for seq 2
carried no panes — this window measured nothing`, and the run exits non-zero. Without that guard
the rig would have published "an interaction is free" while reporting that it never caused one.
This is not hypothetical; a per-burst cursor did exactly this on the first real run.

**One process for the whole ladder** — the rig's original shape. *Caught*, by the `idle spread
across rungs` column: every rung starts from the previous rung's peak, the idle column climbs
across the ramp instead of standing still, and the transient sequence stops being monotone. That
is how the retention result was found in the first place.

**The peak reset, made a no-op** — `clear_refs` never written. **Not caught, and that is a fact
about the rig worth writing down.** The first burst's figure does not move: a fresh process grows
monotonically while its sessions open and settles at its own high-water, so `VmHWM` already
equals RSS at the moment the reset would have run. What moves is the *second* burst — `repeat`
at four dragging viewers went from 216 kB to 3 892 kB, because burst two is then being compared
against burst one's peak rather than against its own baseline.

So the reset is not a guard and no control catches its absence. It became **partly redundant the
moment the rig moved to a fresh process per rung**, and it still earns its place for the one
column that distinguishes a plateau from a leak. A reader who wants that column to mean what it
says should know it rests on one `writeFile` with nothing watching it.

---

## What neither of them says

* **One machine, shared, and cloud.** The ratios survive that; the absolutes are for ordering
  decisions, not for a cost model. `results/` records the host beside every run.
* **One pipeline shape at each end of the range.** "Keeps rows" and "keeps none" are the
  extremes; a real app is somewhere between and its own number is one `run.sh` away.
* **Not apps-per-core, and not a latency rig.** Those are `benches/loadgen/`, which is
  protocol-level with a measured floor, and `benches/fleet/`, which drives real browsers. The
  neighbour's wait above is the one latency question those cannot ask: what a viewer who is
  doing nothing experiences while their neighbours are busy.
* **Nothing about a fleet.** Every figure here is one `dagpane run` holding one app. What a busy
  app does to a *different* app in the same `dagpane host` process is not measured, and the
  mechanism above says it is unlikely to be nothing.

## Running them

```sh
./benches/sessions/run.sh
./benches/sessions/busy.sh
```

Each builds the binary if it is not there, generates the 200 000-row fixtures into a temporary
directory — `fixtures.sh`, shared, so both rigs weigh the same two apps — and writes a JSON per
ramp into `results/`.

**What is in a result file.** Every run records its own provenance: the argv it spawned with,
the binary's path, size and `binary_sha256`, the detected libc *and the evidence for it*, the git
revision, whether the tracked tree was dirty, the untracked file count, the CPU and kernel, and a
digest of the manifest and every CSV it read. That block exists because it caught things — a
static musl binary reported as glibc, and a `dirty` flag tripped by the rig's own untracked
output, which had a whole run claiming a modified tree.

**Two things to know about reading the committed set.**

The files are one continuous run but they do **not** all name the same revision. Each section was
committed as it landed — a two-hour run on a container that is reclaimed when idle is two hours
you can lose — so a later section records the commit carrying the earlier section's data. Nothing
but result files changed between them, and `binary_sha256` is one value across every glibc file
and one across every musl file, which is the thing to check if it matters.

And their `command` names a **concrete port**, while the rig now writes a `<port>` template plus
a `ports` array. Review found that field misleading — each repetition of each rung spawns its own
server on its own port, so one concrete port names one process out of dozens — and the fix landed
after these files were written. The port is the only field that differed, and no figure depends
on it.
