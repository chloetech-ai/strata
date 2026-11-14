# Workbench, rebuilt around what you came to do

This is the information architecture for the next Workbench UI: the pages,
the steps on each page, the one thing to press on each step, and where
every screen of today's build goes. It also covers how the dense views read
at scale and how it all works on a phone. The clickable mocks are in
[`ui/dev/journeys.html`](ui/dev/journeys.html) (see "The mocks" at the
end). Screenshots of them, desktop and phone, are in
`/mnt/project-files/workbench/journeys/`, listed in its `ready.txt`.

## Why change it

Today's build opens on a grid of terminals. Around it:

- a left rail of 11 screens: Mission, Decide, Map, Story, Preview, Flow,
  Away, Health, Ship, Library, Trace
- five level pills: Intent, Plan, Story, Changes, Terminal
- Search, Decisions and a Guide
- a time bar
- a Needs-you rail

Each screen is good at one thing, but together they are rooms with no
hallway. Five simulated users, each with a clear goal, said so:

- "11 rail icons + 5 zoom pills + Search + Decisions = a lot of doors
  before I know which room I want" (Priya, 07:14).
- "The 11 rail names meant nothing to me on first open" (Maya, 07:12). Lee
  (07:27): "Nothing tells me which repository it is looking at".
- Reading a repository needed an agent first: Search, Trace, Map and
  Health said "repo is required" (Priya 07:15, Lee 07:28, Sam 07:15).
- The pills did not say where you were: on Health the pill lit "Plan", and
  on Map it lit "Changes" (Sam 07:15, Lee 07:29).
- "What needs me" had four numbers: the top pill, the Decide badge, the
  board's column and the rail. They disagreed (Dev 07:19), and none of
  them counted Claude Code's trust prompt, which was the first thing every
  user had to answer (all five).
- What people came for was split across screens:
  - Dev made his merge call from Away's one sentence and his own shell,
    not from Ship (07:34).
  - Sam found persona tests only inside Preview (07:21).
  - Maya could not add a rule without a question open (07:23).
- Single-key shortcuts typed into terminals: `z`, `n` and `Esc` (Maya, Dev
  and Sam, each more than once).
- Yes / Always / No relabelled Claude Code's own options. One user's
  "Always" switched an agent into auto mode for the session (Dev 07:22).

What worked is kept:

- the live mini terminals
- Trace's timeline (Priya: "delightful")
- Map's collisions (Dev: "the screen I came for")
- Ship's "decisions about these lines" (Maya: "the moment it helped most")
- the persona test (Sam: "the best thing in the product")
- steering an idle agent from Story
- Health's SQL alert rules
- Flow's "36% of the fleet's time was waiting on you"

## The rule

A person always knows **where they are** and **what to do next**:

1. Every page says in plain words what it is for, and it has **one primary
   button**. Everything else on the page is secondary (ghost buttons,
   links).
2. Pages are reached from **a goal**, not from a list of screens:
   - No page is named after an internal concept: no "Mission", "Decide",
     "Flow", "Intent" or "Plan" as names.
   - Where a word needs explaining (worktree, ChloeCtx, rule), it is
     explained in place, once.
3. **The terminal is one click away, never the default.** Every agent has
   a Terminal tab, and no page opens on one.
4. **There is one count of what needs you**, the same everywhere. It
   counts an agent blocked on you (a question, a permission, the trust
   prompt) and an alert that asks you. An agent that finished its turn is
   "ready", not "needs you".
5. A question shows **exactly what will run**, and **exactly the options
   Claude Code offers**, numbered as Claude Code numbers them.
6. **Dense data opens calm.** Each view leads with one sentence and one
   small picture, then shows the exceptions, with the rest one click away.
   See "Dense views".
7. **It works on a phone.** Every page is one column there, and nothing
   needs hover. See "Phone".

## The goals

Start asks "What do you want to do?" and offers four goals. This follows
the user's mockup: their three use cases, plus Review and ship, which the
evidence split out.

| # | goal | the person says | from |
|---|---|---|---|
| A | **Start a new project** | "I have an empty folder, or an idea." | asked |
| B | **Continue a project** | "A repository I have, or work I started here: what changed, what needs me, let me ask about it, change it." | asked ("existing project" and "continue from Workbench", one goal) |
| C | **Review and ship** | "Is this ready to merge? What does it reach? In which order?" | added (see below) |
| D | **Debug context and project** | "What did the agent know at that turn, what did it miss, and where did it go wrong? What is failing?" | asked ("debug a context + project"); "keep the project healthy" is merged in |

Why this shape:

- **"Existing project" and "continue from Workbench" are one goal.** Both
  start from a repository, and both first need "since you left". With no
  agents yet, that section says so, and the page offers Understand.
  Priya (07:31) and Dev (07:34) both read "what happened", then "what
  needs me", then "the agents", in that order.
- **Review and ship is a goal of its own.**
  - Every diary ended at the merge.
  - Three users (Maya, Dev and Priya) left Workbench to decide it.
  - Priya came only to review.
  - It has checks no other step has: tests on the branch, merges cleanly,
    merge order, decisions to confirm.
  - Continue and the agent page both link to it.
- **Keeping the project healthy is part of Debug.** Sam's whole session
  was debugging with a watch left behind: a flaky test, an alert rule,
  hotspots, proving a fix stable. "What is failing" is one of Debug's
  ways in, and an alert that asks you lands in Needs you.

### Where this departs from the mockup, and why

- **Allow and Deny map to Claude Code's own numbered options.**
  - On Home, "Allow once" is option 1, and "Deny" is the option Claude
    Code numbers "No" (often 3).
  - "All 3 options" opens the rest, with their numbers, in place.
  - The reason: Yes/Always/No relabelled them, and "Always" switched
    agents into auto mode (Dev 07:22).
- **Reading needs no agent.** Continue › Understand asks about the code
  and shows the map before any agent exists (Lee, Priya).
- **Merge is primary in Review, with "Open a pull request instead"
  beside it.** The mockup put the PR draft first. Dev and Maya merged
  locally; the PR draft is one click away, under Details.
- **Debug has two more ways in besides "a session and a turn".** They are
  "why the code is like this" (Trace, Priya's "delightful" moment) and
  "what is failing" (Health and Hotspots, Sam's session). Each one ends
  at the same Fix step.

## Global elements

On a desktop, the top bar of every page holds the breadcrumb and four
controls:

```
◼ Workbench   Home › shop › Review and ship › cents     [⌕ Search shop /]  [● 2 need you]  [◉◉◉◉ 4 agents ▾]  [⋯]
```

- **Breadcrumb** (left): Home › project › step › agent. It says where you
  are, and each part is a link. It replaces the left rail and the level
  pills.
- **Search** (`/`): today's quick search (`quicksearch.js`) over code,
  docs and decisions, scoped to the project in the breadcrumb.
  - `⇧ Enter` on a line asks "why is this line like this" (Debug).
  - With no project yet, the first line of the results is the folder box,
    never "repo is required".
- **Needs you** (`.`): the one count. It is a button that opens a drawer
  of cards, oldest first.
  - Each card shows the agent, how long it has waited, and what it asks
    in full: the whole command, the edit as a diff, or the trust prompt.
  - Under that are Claude Code's own options, with Claude Code's numbers,
    and "Open agent".
  - Answering never types into a prompt that is already gone.
  - Alerts that ask you come after the agents' questions.
  - Zero shows as a quiet "Nothing needs you", not as a hidden pill.
- **Agents** (`a`): the agent switcher. It shows the running agents'
  avatars and a count.
  - It opens a list grouped as Needs you, Working, Ready to review and
    Finished.
  - Each row has the agent's name, project and model, its state in words,
    and its one live sentence.
  - The row opens the agent's page, and the terminal icon opens its
    Terminal tab. `1`…`9` pick a row.
  - Finished agents have **Resume**.
- **More** (`⋯`): what is useful but is nobody's first step:
  - All terminals (today's grid)
  - Where the time went (Flow)
  - Decisions timeline
  - Library docs
  - Rules
  - Guide
  - Keys
  - Setup check

There is no left rail, no level switch, no global time bar and no
permanent right rail. On a phone, the same controls move to a bottom tab
bar (see "Phone").

## Start (Home)

`#/` · CLI: `chloedb-workbench serve` (prints the URL, as today)

```
What do you want to do?
Good morning. You last looked 11 min ago, at 7:20 AM. 6 things happened in shop since.

┌ Start a new project ┐ ┌ Continue a project ─────────────┐ ┌ Review and ship ┐ ┌ Debug context and project ┐
│ From an empty folder│ │ A repository you have, or work  │ │ The diff, the   │ │ What an agent knew at a   │
│ or an idea …        │ │ started here …                  │ │ checks …        │ │ turn, what it missed …    │
│                     │ │ shop, since 7:20: cents         │ │ 2 ready: cents, │ │ freeship taxed shipping   │
│ [Start]             │ │ finished, 2 need you            │ │ flakehunt       │ │ at turn 2                 │
│                     │ │ [Continue shop]  (primary)      │ │ [Review]        │ │ [Debug]                   │
└─────────────────────┘ └─────────────────────────────────┘ └─────────────────┘ └───────────────────────────┘

Your projects  (most active first)                     Needs you now  2
shop      ~/code/shop     ◉◉◉◉  2 need you · 2 ready ·   paybutton wants to run · 9 min        Open
                          4 agents · active now          python3 - <<'EOF' … (the whole command)
          [Continue] [Review 2]                          Copies the page's script to /tmp and asks node …
invoicer  ~/src/invoicer  no agents · 3 days ago         [Allow once 1] [Deny 3]   All 3 options
          [Continue]                                     ▲ Alert · test_promo_window failed twice in an hour
harbor    ~/src/harbor    2 branches not merged          [Open in Debug] [Resolve]
          [Continue] [Review 2]                          Everything since you left →
```

- There are four goal cards. Each one has a sentence saying what it is
  for, and where there is one, a live fact.
- **Primary action**: the card that fits now, filled in accent:
  - Continue, when something happened since you last looked
  - Review, when something is ready and nothing needs you
  - Start, on the first run
- **Your projects** come from `GET /api/repos`, most recently active
  first. Each one has **Continue** and **Review N**.
- **Needs you now** is the same list as the drawer, answered in place.

### First run

`#/` with no project known: two cards and a setup line.

- "Continue a project you have" has the folder box inline (`repos.js`):
  - completion, with git checkouts first and the whole path in view
  - no agent needed to read
- "Start a new project".
- **Setup** (`setup.js`) is one line: ChloeCtx, Claude Code signed in, and
  git. It is red only when something is missing, and then it gives the
  one command that fixes it.
- A note: once a project is open, this page also offers Review and ship,
  and Debug.

## A. Start a new project

Entry: Start › Start a new project · CLI `chloedb-workbench new [DIR]`
(makes the folder, runs `git init`, and opens `#/new/what?dir=DIR`) ·
routes `#/new`, `#/new/what`, `#/new/plan`.

A stepper of four steps: **Folder › What to build › Who builds it › Plan
it**. **Nothing runs until Start on the plan.**

| step | on screen | reuses | primary |
|---|---|---|---|
| 1 **Folder** | A folder box ("an empty folder, or a new one"), with completion. "Workbench runs `git init` and makes a first commit." | `repos.js` | **Next** |
| 2 **What to build** | A large box for the idea in your own words, and a **Done when** line. Chips: "has a done check", and the stack ("Python, standard library; you can change it"). The intent check (`complete.js`) warns when there is no done check. | `complete.js` | (step 3 is on the same page) |
| 3 **Who builds it** | **A lead agent**: it reads the idea, splits it into parts, and starts one agent per part after you approve. **A single agent**: one branch, cheaper. "The lead only plans in this step: about 2 minutes, haiku." | | **Plan it** |
| 4 **Plan it** | The lead's draft, **editable**: the parts in order, each with its done check, what it waits on, and its model. Parts can be reordered, removed or added, or you can tell the lead what to change. Beside it, **What each agent starts with** (below). | Mission's goal staffing (`POST /api/goals` with `plan`), `route-hint.js` | **Start 4 agents** |

What each agent starts with, shown before anything runs:

- **Memory**: ◆ ChloeCtx wired. Its hooks and memory tools go in each
  session's own settings; yours are not touched.
- **Told**: your idea, its part, the done check, and the project's rules.
- **Models**: the lead's and each part's, as set on the part.
- **Branch**: its own worktree on `wb/<part>` from main. Main is never
  edited.
- **Without asking**: edit files on its branch and run the tests. It asks
  for anything else, and it never runs `git push` or `pip install`.
- **At once**: at most 3 agents, and a part that waits on another waits.

After Start, you land on the new project's Continue page, with the agents
working and the trust prompt already answered.

## B. Continue a project

Entry: Start › Continue, or a project row · CLI
`chloedb-workbench serve ~/code/shop` (opens that project and adds it if
new) · `chloedb-workbench open --continue` · routes `#/p/<project>`
(today's `#/catchup` redirects here), `#/p/<project>/understand` and
`#/p/<project>/change`; `#/p` opens a folder.

The project's row of steps sits under the breadcrumb: **Since you left ·
Understand · Start a change**, then **Review and ship (2 ready) →**. These
are places, not an order: you can click any of them.

### B1. Since you left (`#/p/shop`)

```
Continue shop                                                     [I'm stepping away]
Since you left at 7:20 AM, 11 min ago: cents finished its fix, flakehunt made the flaky
test stable, paybutton waits on you, freeship is working.
[ ⌕ Ask about this project: why does the total round half up now?          Ask ]

AGENTS (5)                                                   What changed
┌ paybutton · Waiting for you · 9 min ─────────────┐           wb/cents  money.py, 2 tests  +18 −4
│ Run this command?  Bash, 7 lines                  │           wb/flakehunt  test_coupons.py  +6 −4
│ python3 - <<'EOF' … (in full)                     │         Decisions made
│ [Allow once 1] [Deny 3] [2 Yes, and don't ask …]  │           ◆ Customers win: half up (you, 7:23)
└───────────────────────────────────────────────────┘           ◆ "test_promo_window is flaky, ignore it"
┌ cents · Ready to review · [Review and ship] +18 −4 ┐             (flakehunt's own call) [Keep] [Forget]
┌ freeship · Working · Adding Cart.shipping() …      ┐         Open threads (3)
┌ explain · Finished 3 days ago · [Resume] [Restart] ┐           finance wants banker's rounding … [Decide it]
```

- **Primary**: the first answer while anything needs you, and after that
  **Review and ship**.
- The header says when you last looked, in your own time ("since 7:20"),
  never "24 h". Answers given in a terminal count as yours (Sam 07:35).
- **Ask about this project** opens Debug's Trace on the question.
- **Agents**: Needs you first, open, with the full command and Allow and
  Deny. Then Working, then Ready, then Finished.
- **What changed**: by branch, with +/− as bars.
- **Decisions made**: an agent's own call says so, with **Keep** and
  **Forget**. Dev (07:31): agents' choices were filed as "the user's".
- **Open threads**: conflicts and questions nobody answered.
- **I'm stepping away** marks the moment (`POST /api/seen`). Where the
  browser allows, it also notifies you when something needs you (Priya
  07:16, 07:31).

### B2. Understand (`#/p/shop/understand`)

Reuses `map.js` (without the time bar), `quicksearch.js` and `library.js`
(code search), `trace.js` (history) and `rules.js`. **Nothing on this page
starts an agent.**

- **Primary: Ask.** An answer shows the code hits, with the line's Why,
  and, for a "when" or "why" question, Trace's dossier.
- **The map**, full width: files sized by lines, each agent's files
  tinted, ◆ decisions pinned, and collisions hatched.
- **What is known**, conflicts first: "finance wants banker's rounding,
  receipts round half up", with **Decide it**. This adds a rule without a
  question open (Maya 07:23).
- **Agents are told**: the rules, with Edit.
- **History**: "money.py changed 3 times", with Trace it.

### B3. Start a change (`#/p/shop/change`)

Reuses `complete.js`, `route-hint.js`, Mission's staffing, the guarded
list (`change-requests.js`) and Map's collisions.

- **The composer**:
  - What should change
  - Done when
  - chips: "has a done check", "◆ rule applies", "⚠ paybutton is changing
    web/index.html now"
  - How: one agent, or split into parts
  - **May do without asking**, the permission profile:
    - allowed: edits on its branch, the tests, `serve.py`
    - asks: anything else
    - never: `git push`, `rm -rf`, `pip install`
  - Model, with a suggestion only when there is history
  - Name
- **Primary: Start agent.**
- Flow's suggestions ("you approved `pytest` 9 times") appear as one-click
  ticks in "May do without asking". Flow measured 36% of the time spent
  waiting on you; Dev answered "~22 permission answers".
- **Work in progress**: one card per agent, with its state in words and
  its live sentence. A waiting card shows the question in full.

### The agent page (`#/a/<agent>[/changes|/preview|/terminal]`)

Reuses `story.js` (Progress), the Changes tab with `blast-summary.js` and
`test-chip.js`, `preview.js` with `preview-lab.js`, and the xterm
terminal.

- The header holds the name, its state in words, branch, model, start
  time, +/− and the task. Stop is a menu with Stop, Restart with the same
  task, Resume and Stop its dev server (Maya 07:29).
- **Its question comes first** when it waits: the whole command, and
  Claude Code's own options with their numbers.
- Tabs:
  - **Progress**: the story, one chapter per turn, with what ChloeCtx gave
    it.
  - **Changes**: the diff. **Why?** appears on the line under the cursor
    (`y`), not on every line (Maya counted 91).
  - **Preview**: only when the agent set one. The page is usable by
    default, and "Ask for a change" is a mode you turn on.
  - **Terminal**: with a bar that says "Keys go to paybutton. Ctrl-b then
    Esc to leave."
- **Primary**:
  - the answer, when it waits
  - **Review and ship →**, when its turn is over and it changed files
  - otherwise "Tell paybutton…"

## C. Review and ship

Entry: Start › Review and ship · Continue's "Review and ship" · a project
row's "Review N" · an agent page's primary when it is done · CLI
`chloedb-workbench open --review` · routes `#/p/shop/ship` and
`#/p/shop/ship/<agent>`.

| step | on screen | reuses | primary |
|---|---|---|---|
| 1 **What is ready** | One row per ready branch: what it does (the outcome, not the prompt), +/−, **tests on the branch** (by name), **merges cleanly** (with main and with each other ready branch: `git merge-tree`), **decisions to confirm**, and a verdict. Then **merge order**: "cents, then flakehunt; freeship will need rebasing on cents", with **Ship both together**. Below that, Not ready yet. | `ship.js` data (`/blast`, `/pr/stack`), Map collisions, `test-chip.js` | **Check it** (the first ready one) |
| 2 **Check it** | **Blast radius** first (below). Then **the pieces of work**, each with the hunk that matters. Then **the decision to confirm** ("Customers win: half up", which replaces banker's rounding: **Still right** / **Change it**). Then **Details**, folded: today's `ship.js` (every function, caller, test and decision, and the pull request draft). | `blast.rs` via `ship.js`, `blast-summary.js` | (step 3) |
| 3 **Merge or send back** | **Merge into main**, **Send back with a note** (to the same agent, which resumes with your note), or **Open a pull request instead** (the draft from Details). | `ship.js`'s PR tab, steer | **Merge into main** |

**The blast radius, before Merge**, is one sentence and a compact
picture, never a graph of every edge:

```
● Low risk: changes how customer totals round (to_cents); every caller is tested and passes on wb/cents.
  (rings)  1 function changed   to_cents()                    EXCEPTIONS 3
           6 callers            Cart.add, Cart.total, +4      ● freeship also changes tests/test_cart.py: merge cents first
           2 public names       the checkout total, receipt   ◆ Replaces banker's rounding (b72f105, Nov 9)
                                                              ● 12 pass on wb/cents · merges cleanly into main
```

- The **rings**, from the inside out:
  1. the functions it changes
  2. who calls them
  3. the public names a user sees
- The **exceptions** list only what you must know:
  - an untested function
  - another agent in the same file
  - a decision this change replaces
  - the tests, as one line
- Dev's "purple hairball of 11 decisions I can't read" becomes "2 that
  matter, 9 routine folded".

The verdicts (risk, tests, collisions) must be the ones fixed in the sixth
build (SPEC.md, "Ship and Trace tell the truth"):

- one risk per diff
- the branch it lands on
- stacked branches not counted as collisions

### Built (C)

`ui/goals/review.js` (`#/p/<project>/ship[/<agent>][/merge]`) over
`src/review.rs`:

- `GET /api/review?repo=` lists ready branches. Each row has its tests by
  name, whether it merges cleanly into main and into each other ready
  branch, the decisions to confirm, the risk and a gate.
- `GET /api/agents/:id/review` returns:
  - the verdict and the three rings (functions changed, callers reached,
    things users see), with only the exceptions;
  - the pieces of work, riskiest first, each with +/− bars and its key
    hunk;
  - the mechanical changes folded into "Not shown: …";
  - the decisions that matter, with the routine ones folded;
  - the checks: tests, merge, commits and order.
- The merge order is computed and says why, naming the first pair that
  conflicts ("merge X first, then send Y back to rebase"). **Merge both,
  in this order** is the primary when every ready branch is clean.
- Merge is gated (`gate()`), and the gate decides which of Merge and Send
  back is the primary:
  - A decision that replaces an earlier one must be confirmed first
    (`POST /api/review/confirm`, with **Still right** / **Change it**).
  - Failing tests or a conflict make Send back the primary. Merge stays
    possible through **Merge anyway…**, which asks you to type the branch
    name.
- Merging (`POST /api/review/merge`) first shows a confirm sheet: from,
  into, how, what is not done, what Undo does, and the commits.
  - The merge is a `--no-ff` merge in the main checkout.
  - It refuses with a plain sentence and a 409 code when the gate does
    not allow it (`unconfirmed`, `gated`), when there are uncommitted
    changes or a dirty checkout (`uncommitted`, `dirty`), or when the
    branches conflict (`conflict`).
- After a merge, a banner offers **Undo** (`POST /api/review/undo`):
  - Undo puts main back while main is still at the merge and nothing has
    been pushed.
  - Otherwise it explains why not (`pushed`, `moved`) and gives the
    `git revert -m 1` command.
- **Send back** sends a prefilled note through steer, or restarts the
  agent with the note when steer refuses.
- **Open a pull request instead** opens Details, which is today's
  `ship.js` with the draft.
- Empty project and failing endpoint each show one plain sentence.
- On a phone there is a sticky action bar and the exceptions come before
  the rings.
- Tests: `review_lists_what_is_ready_checks_one_and_merges_it_with_undo`
  and `review_holds_merge_for_failing_tests_and_an_unconfirmed_decision`.

Left out:

- "Ship both together" as one squashed change. The primary merges the
  branches one after another in the computed order, which keeps Undo per
  merge.
- "Public names" are the changed entry points (routes and pages) that the
  blast already finds. There is no separate public-API analysis.
- The CLI `open --review` entry. Entries and redirects belong to the
  router (R1).

## D. Debug context and project

Entry: Start › Debug · an alert in Needs you ("Open in Debug") · "Ask
about this project" · "Why?" on a line · CLI
`chloedb-workbench open --debug "when did rounding change?"` · routes
`#/debug`, `#/debug/<agent>/<turn>`, `#/debug/<agent>/<turn>/wrong`,
`#/debug/code` and `#/debug/failing`. Today's `#/why`, `#/trace` and
`#/health` redirect here.

A stepper: **Session and turn › What it knew › Where it went wrong ›
Fix**. Above it, "Start from" offers three ways in: **an agent's turn**,
**why the code is like this**, and **what is failing**.

| step | on screen | reuses | primary |
|---|---|---|---|
| 1 **Session and turn** | The project's sessions, with the one you flagged marked. That session's turns: title, prompt, files, tokens, ◆ injected and missed counts. The bad turn is red. | `GET /api/agents/:id/story` | **Open turn 2** |
| 2 **What it knew** | **Prompt**. **Files** it read and edited. **Context**: a "41% full" meter, the memory tokens and the threshold. **ChloeCtx memory injected**: counts by kind, then the records, each with its score. **What it missed**: a record that existed but was not injected, with its score against the threshold ("Shipping is not taxed", 0.58 < 0.60), and records rightly left out. | `story.js`'s "What it knew", the new turn-context API | **Where it went wrong** |
| 3 **Where it went wrong** | **The bad turn's blast radius** as a ranked list (below). Then the turn's diff, with the lines that matter marked, and the arithmetic that proves it ("4748 + 392 tax + 499 shipping = 5639; the test pins 5680"). "Rewind its terminal". | `blast.rs`, the commit of the turn, Rewind | (step 4, same page) |
| 4 **Fix** | Ticks: **Pin the record to cart.py** (always injected when an agent edits it), **Lower the threshold for your decisions** (0.60 → 0.50, "3 more records would have been injected today, +41 tokens"), **Make it a rule**. **Send freeship back**, with a prefilled note. | `rules.js`, steer, the new `pin` and `threshold` actions | **Apply and send to freeship** |

**The blast radius of a bad turn**, worst first:

```
● Turn 2 changed total(): 9 callers reached, 2 tests caught it, 0 reached main.
  2  tests failed after it         test_receipt_total, test_total_shown
  9  callers reached               receipt, ledger export, coupons, +6
  1  function changed              Cart.total()
  0  runtime errors blamed on it   none in the preview's log
  0  reached main                  wb/freeship is not merged
```

Other ways in:

- **Why the code is like this** (`#/debug/code`): Trace, as today.
  - The question as reading chips, the one-line answer, and the timeline:
    commits on every branch, edits, ◆ decisions, your answers and rules.
  - Then the code now, and **Explain**, which calls a model and says so.
- **What is failing** (`#/debug/failing`): today's Health with Hotspots,
  reordered around the question:
  1. failing tests, by test name, never by command line (Sam 07:19)
  2. flaky ones, found per test
  3. the alerts that fired
  4. hotspots
  - It shows "No test runs recorded yet" rather than a green "All quiet"
    on an empty project (Sam 07:15).
  - Alert rules are behind a link.
- **What agents remember** (`#/debug/records`, new): ChloeCtx's records
  about the project.
  - Filters: kind, **who said it** (you, or which agent), branch, and
    superseded.
  - Actions: **Correct**, **Forget**, **Pin** and **Make it a rule**.
  - It answers the trust problems the diaries found:
    - a one-task instruction stored as a standing preference (Maya 07:23)
    - an agent's words filed as "the user's" (Dev 07:20, 07:31)
    - another branch's records told as fact (Maya 07:23, Sam 07:30,
      Lee 07:39)

### Built (D)

`ui/goals/debug.js` (`#/debug`, `#/debug/<agent>[/<n>[/wrong|/fix]]`,
`#/debug/code`, `#/debug/failing`, `#/debug/records`) over `src/turns.rs`.

**Step 1, Session and turn**:

- `GET /api/debug/sessions` and `GET /api/agents/:id/turns` give each
  session's turns.
- The page shows the sessions, then the turn strip with phases on long
  sessions.
- "Turns that stand out" lists failed tests after a turn, a missed note
  and a flagged turn.
- Each turn has **This went wrong**
  (`POST /api/agents/:id/turns/:n/flag`, 404 for a turn that does not
  exist).

**Step 2, What it knew** (`GET /api/agents/:id/turns/:n/context`):

- the prompt, the context meter, and what Workbench reminded it of, as
  counts with the top note;
- **what it missed**, in plain words ("58% match; notes under 60% are left
  out"), and what was rightly left out;
- the nearby turns and the turn's diff.

**Step 3, Where it went wrong** (`GET /api/agents/:id/turns/:n/blast`):

- the verdict and the ranked list, worst first, with icon and text (never
  colour alone);
- the diff with the first changed line of each function marked;
- **Replay its terminal at turn N (read only)**, through the existing
  rewind.

**Step 4, Fix** is kept apart from Send back:

- One recommended fix is ticked: **Always remind agents editing <file>**
  (pin, `PATCH /api/records/:id`).
- **Remind agents of more of your decisions (looser match)**:
  - it is labelled as a project setting and previews what else it would
    have given at that turn, with the tokens;
  - it is applied with `PUT /api/repos/:repo/threshold` and has its own
    **Undo**.
- **Make it a rule**.
- With no missed note, the fix is a rule you write
  (`POST /api/rules`).
- **Send <agent> back** is a separate box with a prefilled note.

The other ways in:

- `#/debug/code` mounts Trace, saying "How it works now: the code on main".
- `#/debug/failing` gives a summary sentence from `/api/tests` ("No test
  runs recorded yet in X" when empty), then mounts Health.
- `#/debug/records` (`GET /api/records`, `PATCH` and `DELETE
  /api/records/:id`):
  - it lists ChloeCtx's notes with **who said it** (you, an agent, or
    Workbench) and the branch;
  - its filters are kind, who, branch and **Show replaced notes**, all
    kept in the URL;
  - its actions are **Correct**, **Always remind…**, **Make it a rule**
    and **Forget** (Forget asks first);
  - notes for one task only, or tied to a branch, are marked as such.

Every page shows one plain sentence when there is no data or the server
fails.

Tests:

- `debug_reads_turns_flags_one_and_traces_what_it_changed`
- `records_are_listed_with_who_said_them_corrected_pinned_and_forgotten`
- `the_threshold_is_set_for_the_project_and_undone`

Left out:

- **The arithmetic that proves it** ("4748 + 392 + 499 = 5639"). Nothing
  in the turn's data supports it without a model, so the page shows the
  failing tests by name and the marked lines instead.
- **A branch picker on `#/debug/code`**. Trace reads the main checkout,
  so the page says that the code is main's.
- **A threshold per kind or per author.** ChloeCtx has one value per
  project (`knowledge.min_cos` in the store's `config.json`), so "your
  decisions" is worded as a project-wide setting.
- **One "Apply and send" primary.** Following the UX critique, Apply fixes
  and Send back are kept separate.
- **The CLI `open --debug` entry and the `#/why`, `#/trace` and `#/health`
  redirects.** They belong to the router (R1).

## Dense views

The user asked: "think about places where we will see tens of lines of
changes and numbers and things, it can get overwhelming, look for those
places and think about a better UI for those". Every one of these places
is tested against the overwhelming case (`#/dense/*` in the mock):

- one agent's change of **38 files and 186 turns** (checkout-v2)
- a project with **9 agents**

| place | what it shows today | the new default |
|---|---|---|
| Diffs and the Changes tab | Every file, every hunk, in path order; a Why? button on every line (Maya counted 91) | One sentence for the whole change and one +/− bar. **The pieces of work the agent did**, riskiest first. Each piece has a sentence in words, one bar, and **the hunk that matters**, with "All hunks of this piece" behind a click. **Mechanical changes** (renames, imports, lockfiles, formatting-only hunks, generated files) are folded into one "skipped" line. Why? only on the cursor line. |
| Ship's file list | Every file with its functions, in a list | Rings (functions, callers, public names), each with its top 3 names and "+N more". |
| Ship's decisions | Every ◆ decision about the lines, as a purple cloud ("a hairball of 11") | Only the decisions that **change an earlier decision or add a rule** ("2 that matter"). Routine ones fold into a count ("9 routine"). |
| Ship's blast radius | A graph of every caller | **A one-line verdict**, three rings, and only the exceptions. |
| Health's tests table | A row per test run, by command line, and a radar | **"214 pass · 2 fail · 1 flaky · 2 untested"** on one line, with a strip of the last 20 runs and the delta ("+31 tests since yesterday"). Rows only for failing, flaky and untested. The radar goes to More. |
| Hotspots | A table of every file with five scores | The top 3 files with the reason in words ("agents corrected it 4 times"), then "N more". Heat on the map shows the rest. |
| Flow's numbers | Per agent: thinking, tools, tests and waiting, as stacked numbers | One bar, "Time waiting on you: 18%", with the one suggestion that saves the most ("12 of 14 waits were python3 commands you always allow: Allow them"). The rest stays under More. |
| Away's "what happened, in order" | Every event, in order, with "Replay the evening" | Continue's three groups: **Needs you** (open, with buttons), **Working** (one line each), **Finished** (folded). What changed **by piece of work**, with bars. Decisions that matter, with routine ones folded ("2 change something, 31 routine"). |
| Event logs | Every hook event | Folded into Progress as chapters; the raw log is behind "All events". |
| Decide's queue | Every question and every past answer | Needs you only, oldest first, each card complete. Past answers are in Continue's "Decisions made". |
| Trace's timeline | Every commit, edit and record on a time axis | The answer first, then the events that changed the answer ("3 changes to rounding"), then "N more events". |
| The PR draft | All of the branch's decisions, files and tests | The outcome, this branch's decisions that matter, and the tests by name, with failing ones first. It lives under Review › Details. |
| The capsule | The whole session's reasoning | Folded as "Attach the reasoning" in the PR draft. |
| Story's steps | Every step of every turn | **A strip of one tick per turn**, coloured by kind and split into named phases. **Only the turns that stand out** are listed ("6 of 186"). Tap a tick to name its turn. |
| Memory given | Every injected record | Counts by kind ("14 decisions, 2 rules, 9 facts, 3 fixes"), then **the top record** and **the missed record**. "All 28 records" is a click away. |
| Tokens | A number of tokens | A meter: **"41% full"**. |
| Map with many files | Every file as a tile with its label | Folders collapse beyond 40 tiles. Tiles of untouched files are muted, and labels show only on tiles large enough to read. Agents' files and collisions stay in colour. |

### Scale rules

Every dense view follows these rules. The same list is shown in the mock
at `#/dense/rules`.

1. **Test every screen against the overwhelming case**: a change of 38
   files and 186 turns, and a project with 9 agents.
2. **Lead with one sentence and one small picture.** The sentence answers
   the page's question. The picture is a ring, a bar, a strip or a meter.
3. **Ship opens with a one-line verdict.** Then come rings: functions,
   callers, public names (user-facing names only). Then come only the
   exceptions:
   - untested functions
   - another agent in the same file
   - a decision this change replaces
   - the tests passing, as one line
4. **Debug shows the blast radius of the bad turn**, e.g. "changed
   total(), 9 callers reached, 2 tests caught it, 0 reached main".
5. **Diffs are grouped into the pieces of work the agents did, riskiest
   first.**
   - Mechanical changes (renames, imports, lockfiles, formatting) are
     folded and skipped by default.
   - Each piece shows its one hunk that matters.
6. **+/− are short bars**, with one total per piece. The exact numbers
   show on hover on a desktop, and are printed beside the bar on a phone.
7. **Tests read "214 pass" on one line.** Only failing, flaky or untested
   tests get a row.
8. **Agents fall in three groups**: Needs you (open, with buttons),
   Working (one line each) and Finished (folded).
9. **A session is a strip**: one tick per turn, coloured by kind and split
   into named phases. The list shows only the turns that stand out (6 of
   186).
10. **Memory given shows counts by kind**, plus the missed record and the
    top record. The rest is one click away.
11. **Decisions show only those that change an earlier one or add a
    rule.** Routine ones fold into a count.
12. **Tokens show as a meter**: "41% full".
13. **Show the top 3, then "N more".** Show deltas ("+31 tests since
    yesterday"), not totals.

## Shared pieces

The goal pages are built from one set of pieces in `ui/levels/common.js`,
so Start, Continue, the drawer, the agent page, Review and Debug look and
behave the same. Import them from `/levels/common.js`. Their styles use the
tokens below only; a page's own `useStyle` CSS does too.

### Page frame and controls

| piece | what |
|---|---|
| `page({ kicker, title, sub, aside, steps, wide, cls, inspector }, ...body)` | A goal page: a mono kicker, a 28px title, one sentence, an aside on the right, then the body. `inspector` (a node or array) is the frosted side panel, `--insp-w` wide; at 1024px and below it moves under the body. `wide` drops the `--page-w` cap. |
| `stepper({ list: [{label, href, state: "done"\|"now"\|"next"}], after: {label, href} })` | The steps of a goal, `01 / 02 / 03` in mono; on a phone "Step 2 of 4 · What to build" over a bar of segments. |
| `btn(text, { primary, ghost, danger, sm, lg, href, onclick, kbd, icon, cls, disabled, title })` | A button (or a link when `href`) with its key shown as a keycap. **One `primary` per page: the page's goal.** Answer options are never primary. |
| `icon(name, cls = "i")` | A line icon: search, bell, more, chev, down, term, check, plus, folder, spark, rewind, branch, ship, why, new, map, stop, warn, pause, play, eye, x, back, home, grid, kbd, bug, clock, book, gear, send, agents. `"i sm"` is 13px. |
| `kc(key)` | A keycap (`<kbd class="kc">`). |
| `label(text, ...aside)` | A small-caps mono label (`.lbl`), with links or counts on the right. |
| `pane(title, aside, ...kids)` | A flat pane on a hairline with a label header; put padded content in `h("div", { class: "bd" }, …)`. |
| `bank(cols, ...tiles)` | Tiles separated by 1px hairlines (`.bank`); one column on a phone unless it has the class `keep`. |
| `stateDot(group, text)`, `GROUP_WORDS` | The dot and word of an agent's group: `needs`, `working`, `ready`, `finished`, `failed`. The same words everywhere. |
| `since(t)` | "now", "4 min", "2 h", "3 d". |
| `.field` | A text input or textarea (16px on a phone and on a coarse pointer). |
| `.tbl` | A table: mono small-caps heads, 44px rows, hairlines. |
| `.chip` | A mono chip (`.chip.add`, `.chip.del`). |

### The question card

`questionCard(q, { agent, onAnswer(i, option), onGrant(i, option), onText(words), big, agentLink })`
is the only way a question is shown: on Start, Continue, the drawer, the
agent page and the phone. `q` is a row of `/api/questions` or the
`question` of a `/api/needs` item. It shows:

- the command in full (over 25 lines: the first 25 and "Show all N lines",
  and the options are disabled, with the reason, until it is shown);
- "Claude's summary, not checked:" with the model's description, under the
  command, never instead of it;
- Claude Code's options in Claude Code's order, with its own labels and
  numbers (`optionsOf(q)`), all equal weight and none filled;
- an option that grants more than one call (the server's `grants`) on its
  own, apart from the others, with what it grants on a second line. It
  calls `onGrant`, which opens the grant sheet (the exact rule from
  `grantOf(q, option, agent)`, the file it is written to, what else it
  would allow, and the narrower choice) before anything is typed;
- keys while the card has the focus: a digit picks that option, `Enter`
  sends it, `Esc` drops the pick. Digits outside a card do nothing.

### Dense pieces (the scale rules)

| piece | rule | what |
|---|---|---|
| `churnBar(add, del, max = 600)` | 6 | +/− as one short bar, the numbers printed beside it (never hover-only). |
| `churnSquares(add, del)` | 6 | Five squares, the added share green and the removed red (Review's pieces table). |
| `riskLabel(risk)` | 5 | `HIGH` / `MED` / `LOW` in mono small caps. |
| `blastCompact({ verdict, tone, rings: [{n, label, names, code, of, unit, onMore}], exceptions: [{tone, text, sub}], tests, actions, title })` | 2, 3 | The verdict sentence in a pane, the actions, a bank of gauges (a big mono number, a scale of ticks, the top 3 names and "+N more"), then only the exceptions (`tone`: `crit`, `warn`, `ctx`, `info`; a failing test is `crit`). |
| `turnStrip(turns, phases, marks, notes, { href, onPick })` | 9 | One tick per turn (`read`, `edit`, `run`, `fail`, `you`, `ctx`), told apart by height as well as colour, under named phases `[{name, from, to}]`; a tap names the turn; `href(n)` links it. |
| `meter(pct, text)` | 12 | "41% full" and a bar (warn from 85%). |
| `testStrip("pppfp")` | 7 | The last runs as squares: `p` pass, `f` fail, `s` none. |

### Tokens

Every colour, font, size, radius, border and shadow is a custom property
on `:root` in `ui/style.css`, set again for dark mode. The default is
direction C, "Instrument": Geist and Geist Mono (`ui/fonts/`), hairlines,
a frosted toolbar and inspector, one indigo accent.

| group | tokens |
|---|---|
| type | `--sans`, `--mono`, `--sans-features`; sizes `--fs-xs` 11, `--fs-sm` 12, `--fs-body` 13 (15 on a phone), `--fs-md` 15, `--fs-lg` 20, `--fs-xl` 28, each with its `--lh-*`; `--track-xl`, `--track-md`, `--track-label`; weights `--w-text`, `--w-mid`, `--w-title` |
| space and size | `--s-1` 4 … `--s-7` 40; `--ctl-h` 30 (44 on a phone), `--ctl-h-lg` 36, `--touch` 44, `--kc-h`; `--top` (the bar), `--tabh` (the phone's tab bar), `--insp-w` 360, `--page-w`, `--sheet-w` |
| radius and line | `--r` 10 (panes), `--r-sm` 6 (controls), `--r-xs` 4 (chips, keycaps), `--r-pill`; `--hair` |
| surfaces | `--bg` (the canvas), `--surface` (a pane), `--surface-2`, `--surface-3`, `--sunken`, `--well` (a command), `--glass`, `--glass-in`, `--glass-blur`, `--scrim` |
| text and lines | `--text`, `--text-2`, `--muted`, `--text-3` (all at least 4.5:1 on `--surface` and `--bg`), `--line`, `--line-2` |
| accent | `--accent`, `--accent-2`, `--accent-ink`, `--accent-text`, `--accent-soft`, `--accent-line`, `--focus` |
| meaning | `--good`, `--good-ink`, `--good-soft`; `--warn`, `--warn-ink`, `--warn-soft`, `--warn-line`, `--on-warn`; `--crit`, `--crit-ink`, `--crit-soft`; `--ctx` (ChloeCtx) |
| diffs and terminal | `--add-bg`, `--add-gut`, `--add-ink`, `--del-bg`, `--del-gut`, `--del-ink`; `--term-bg`, `--term-fg`, `--term-line` |
| agents and lanes | `--lane-1` … `--lane-5`, `--lane-x`, `--av-ink` |
| depth | `--shadow-1` (none), `--shadow-2` (cards, the drawer), `--shadow-3` (sheets) |


## Phone

Every page works on a phone. The mocks are shot at 390x844, which is an
iPhone 14 at 2x, and the screenshots are named `*-phone-*.png`. The
layout is the same set of pages and routes, re-flowed; there is no
separate phone app.

**Breakpoints** (CSS, in `ui/dev/journeys/phone.js`; the rewrite moves
them into `style.css`):

| width | what changes |
|---|---|
| **> 1024px** | the desktop layout above |
| **≤ 1024px** (tablet, narrow window) | One column of cards where there were two. The four goals become 2 × 2. The top bar stays, and Search loses its fixed width. Ship's exceptions go under the rings. Debug's ranked list becomes 3 across. |
| **≤ 640px** (phone) | Everything in the rules below. |
| `pointer: coarse` (any width) | Touch targets of at least 44px. |

The phone rules:

1. **One column, no horizontal scroll.**
   - Every grid becomes one column.
   - Long names and code wrap (`overflow-wrap: anywhere`).
   - Only a box made to scroll does so: the project's row of steps, the
     agent page's tabs, the session strip and the terminal's key row.
   - The screenshot script reports any phone shot whose page scrolls sideways.
2. **The New project stepper becomes a compact indicator**: "Step 2 of 4 ·
   What to build" and a bar of segments. **A sticky bottom action bar**
   holds **Back** and **Next** (Next is "Plan it" on step 3 and "Start 4
   agents" on step 4). The inline primary buttons are hidden, so each
   step still has one primary. Review and Debug use the same compact
   indicator.
3. **Top navigation becomes a bottom tab bar**: **Start**, **Needs you**
   (with the count as a badge), **Projects** and **More**.
   - The top bar keeps a back link to the parent crumb and the page's
     name, plus Agents (a count) and a Search icon.
   - Needs you opens full screen between the two bars.
   - More, Agents and Search open as bottom sheets.
4. **Needs-you cards get full-width Allow and Deny buttons** (48px tall).
   - The command is shown in full, in a wrapping monospace block, **never
     truncated** (no max height, no ellipsis).
   - Claude Code's options stack as full-width buttons with their numbers.
5. **No information is hover-only; a tap opens it.**
   - The exact +/− numbers are printed beside each bar.
   - "All 3 options" is a tap-to-open list, not a tooltip.
   - Ship's exceptions are a tap-to-open list.
   - A tap on a tick of the session strip names its turn under the strip.
   - Keyboard hints (`1`…`9`) are hidden.
6. **Touch targets are at least 44px**: buttons, tabs, links that stand on
   their own and rows. **Inputs are 16px or more**, so iOS does not zoom
   on focus.
7. **Review and ship puts the verdict and the Merge or Send back buttons
   first.**
   - The rings sit beside their legend (96px), and the exceptions follow
     as a folded list with their count.
   - The action row at the bottom of the desktop page is hidden, so the
     buttons appear once.
8. **Debug's turn strip scrolls sideways inside its own box**, never the
   page. **The blast radius is a ranked list**, worst first, one row
   each, not a graph.
9. **A terminal on a phone is read-only by default**: "Read only. Nothing
   you tap reaches paybutton." **Type** opens the keyboard and an input
   bar.
   - The bar has the keys a phone keyboard lacks: Esc, Tab, ↑, ↓, 1, 2, 3
     and Ctrl-C.
   - The input has Send. **Done** closes it.
   - This keeps a stray tap from answering a prompt.
10. **Safe-area insets are respected.**
    - The page sets `viewport-fit=cover`.
    - The top bar pads with `env(safe-area-inset-top)`, and the tab bar
      with `env(safe-area-inset-bottom)`.
    - The page gutter is `max(16px, env(safe-area-inset-left/right))`.

Reused level modules (`map.js`, `story.js`, `ship.js`, `trace.js`) render
inside their hosts at phone width. The rewrite gives each its own
≤ 640px rules:

- Map: a list of files under the treemap, with the treemap at most 360px
  tall.
- Story: chapters as cards.
- Ship and Trace: one column.

## Every screen and feature: where it goes

| today | where it lives now | why |
|---|---|---|
| **Terminals grid** (home today) | More › All terminals; each agent's **Terminal** tab | the terminal is one click away, never the default |
| Zoom (`z`) and its Terminal / Changes / Events tabs | the agent page's tabs; Events folded into Progress | one page per agent |
| **Mission** (goal, outcomes in four columns, staffing three ways) | A's Who builds it and Plan it; B3's "Split it into parts"; the outcome cards as Start a change's work in progress | a goal is a way to start work, not a place |
| **Decide** (questions, Yes/Always/No) | the **Needs you** drawer, Start's Needs you now, Continue's agents, the agent page | questions belong to the agent and to "what needs me"; options keep Claude Code's numbers |
| Decide's Rules panel | B2's "Agents are told"; More › Rules | rules are what agents are told about a project |
| Decide's "Review today's" | Continue's "Decisions made" | it is part of what happened |
| Guarded commands, change requests | B3's and A's "May do without asking" (never: …); a proposal arrives in Needs you | it is part of what an agent may do |
| **Trace** | D › Why the code is like this; Continue's "Ask about this project"; B2's History | its question is "why" |
| **Map** (treemap, collisions, pins, Heat) | B2 Understand (the main visual); collisions in B3 and C1 | the map is how you understand a repository |
| **Story** (Intent, chapters, steps, what it knew, steer) | the agent page's Progress tab; "What it knew" is D step 2 | one agent's work is read on its page |
| **Preview** (iframe, overlay, Ask for a change) | the agent page's Preview tab, shown only when set; overlay off by default | the preview is that agent's |
| Preview lab (persona test, runtime errors) | Preview › "Try it as a customer"; C's checks; D's blast ("runtime errors blamed on it") | it is a check before shipping |
| **Ship** (blast radius, PR, capsule, Ship together) | C Review and ship; today's view under Review › Details | now a goal, with cross-branch checks and the verdict first |
| Changes tab's blast summary, test chip | the agent page's Changes tab; C1 rows | kept |
| **Away** (digest, replay) | B1 Since you left | "continue" is one page |
| Away's "Replay the evening" | **removed**; a moment's Rewind stays on each line | 20 identical permission tiles were noise (Dev 07:34) |
| **Health** (incidents, alert rules, tests) | D › What is failing; an alert that asks you is in Needs you | health is debugging with a watch left behind |
| **Hotspots** | D › What is failing; Heat on B2's map | same question: what keeps breaking |
| **Flow** (where the time went, coach) | **hidden under More** › Where the time went; its suggestions become ticks in "May do without asking" | a mirror worth a look, not a step |
| **Library** (code search, docs crawl) | code search: B2's Ask and global Search; docs: **More** › Library docs | reading code is part of understanding |
| Quick search (`/`) | kept, global, scoped to the breadcrumb's project | |
| **Decisions** (top-bar timeline, lanes per agent) | **More** › Decisions timeline; records at D › What agents remember | three ways to read one memory were two too many |
| **Time bar** (time travel above Mission and Map) | **removed** as a bar; "as it was then" stays as a link on Trace's cards | nobody in five sessions used it, and it took the top of two screens |
| Level pills (Intent, Plan, Story, Changes, Terminal), `[` `]` | **removed**; the breadcrumb and the steps say where you are | they lit the wrong level on half the screens |
| Left rail of 11 screens | **removed** | replaced by goals and the breadcrumb |
| Right rail, Needs you | the Needs-you button and drawer; Start's Needs you now; the phone's tab | one count, reachable anywhere |
| New agent dialog | B3's composer | starting an agent is describing a change |
| Autocomplete and intent check (`complete.js`) | A's and B3's composers; completions only from the repository's own text | Lee 07:32: an invented completion offered as the default |
| Model routing (`route-hint.js`) | A and B3: a suggestion with its reason, only with history | "0 of 1 like this finished" was noise (Maya 07:25) |
| Rewind, Why card (the side sheet) | kept everywhere as the side sheet; "Rewind its terminal" in D | |
| Guide (`guide.js`) | **More** › Guide, rewritten as these journeys | the first run replaces "the first ten minutes" |
| Setup check (`setup.js`) | the first run's setup line; **More** › Setup check | |
| Repository picker (`repos.js`) | `#/p`, the first-run card, Search's first line | reading needs no agent |
| Stop (confirm) | the agent page's Stop menu, with Restart, Resume, Stop its dev server | |
| Exited fold | the switcher's Finished group and Continue's agents, with Resume and Restart | Maya 07:14: no restart button |
| Keys dialog, `Ctrl-b` prefix | **More** › Keys; the prefix only in the Terminal tab | |
| Bare keys `z`, `c`, `t`, `d` | **removed** (see Keyboard) | they typed into terminals |

In short:

- **Removed outright**:
  - the left rail
  - the level pills and `[` `]`
  - the global time bar
  - "Replay the evening"
  - the bare keys `z c t d`
- **Hidden under More**:
  - All terminals (today's home)
  - Where the time went (Flow)
  - Decisions timeline
  - Library docs
  - Rules (also in Understand)
  - Guide
  - Keys
  - Setup check
- Everything else moved into a goal's step.

## URL routes

| route | page |
|---|---|
| `#/` | Start (the first run when no project is known) |
| `#/new`, `#/new/what`, `#/new/plan` | A. Start a new project |
| `#/p` | open a folder |
| `#/p/<project>` | B1 Since you left |
| `#/p/<project>/understand[?q=]` | B2 Understand |
| `#/p/<project>/change` | B3 Start a change |
| `#/p/<project>/ship`, `/ship/<agent>` | C Review and ship |
| `#/a/<agent>`, `/changes`, `/preview`, `/terminal` | the agent page and its tabs |
| `#/debug`, `#/debug/<agent>/<turn>`, `…/wrong` | D, steps 1 to 4 |
| `#/debug/code[?q=]`, `#/debug/failing`, `#/debug/records` | D's other ways in |
| `#/more/terminals`, `/time`, `/decisions`, `/library`, `/rules`, `/guide`, `/keys`, `/setup` | More |

`<project>` is a short name the server gives each repository: its
folder's name, made unique (`shop`, `shop-2`). A route never carries an
absolute path.

Today's routes redirect for a release:

| today | redirects to |
|---|---|
| `#/mission` | `#/p/<p>/change` |
| `#/decide`, `#/away`, `#/catchup` | `#/p/<p>` |
| `#/trace`, `#/why` | `#/debug/code` |
| `#/map` | `#/p/<p>/understand` |
| `#/story/<id>`, `#<agent id>` | `#/a/<id>` |
| `#/preview/<id>` | `#/a/<id>/preview` |
| `#/ship/<id>` | `#/p/<p>/ship/<id>` |
| `#/health[/…]` | `#/debug/failing` |
| `#/flow` | `#/more/time` |
| `#/library` | `#/more/library` |
| `#/grid` | `#/more/terminals` |
| `#/guide` | `#/more/guide` |

## Keyboard

Single keys act only when the focus is not in a text field or a terminal.
Inside the Terminal tab every key goes to the agent, and a bar says so.

| keys | what |
|---|---|
| `/` | Search (`⇧ Enter` on a line: why is this line like this) |
| `.` | Needs you |
| `a` | Agents (then `1`…`9` to open one) |
| `n` | in a project: Start a change (focus the composer) |
| `g s`, `g c`, `g r`, `g d` | go to Start, Continue, Review, Debug |
| `1`…`9` | in a question card: Claude Code's option with that number |
| `y` | in a diff: Why? for the line under the cursor |
| `j` `k`, `Enter` | move through the items of a page, open one |
| `Esc` | close the drawer or the sheet; else up one level of the breadcrumb |
| `?` | Keys |
| `Ctrl-b` then `Esc` | leave a terminal (`Ctrl-b` then `?` lists the rest) |

Removed: `z`, `c`, `t`, `d`, `[` and `]` as bare keys. They typed into
terminals, and they named concepts the user had not met.

## Rules and notifications

Two goal modules, served at `/goals/<file>` (`GOALS` in `src/server.rs`,
next to R1's list; when R1's list exists, its two entries move there).
They take the level ctx (`{ api, state, params, go }`) plus R1's
`{ project, crumbs }`, and use only `common.js`'s pieces and the tokens.

### What R1 calls

```js
import * as rules from "/goals/rules-suggest.js";
import * as notify from "/goals/notify.js";

// More › Rules (#/more/rules): mount it under Decide's text rules, with
// its own heading ("What agents may run"); both are about rules.
rules.mount(root, ctx);

// Start and Continue: a one-line card ("12 of 14 waits were pytest.
// Review a rule"); an empty node when there is nothing to suggest.
col.append(rules.card(ctx));

// Every question card: the wide option ("Yes, and don't ask again…")
// opens the grant sheet before anything is typed.
questionCard(q, { agent, onAnswer, onGrant: (i) => rules.grantSheet(ctx, { question: q, option: i, agent }) });

// Once, at start.
const n = notify.init(ctx);
topBar.append(n.control);        // on a phone: in More instead
stream.on("change", n.poke);     // re-read at once on the stream's changes
```

- `grantSheet(ctx, { question, option, agent })` answers the question
  itself (`POST /api/questions/:id/answer`) on Confirm, and resolves
  `{ choice: "confirm" | "narrow" | "cancel", pattern, result }`. Other
  callers pass `{ pattern, agent, file, command, repo, onConfirm }`.
- A notification's click calls `ctx.go("#/a/<agent>?q=<question>")`,
  fires `wb:focus-question` on `window` (`detail: { question, agent,
  href }`), then focuses `.qc[data-q="<question>"]` and adds the class
  `focus` once the card is on the page. The agent page should show that
  question's card when `?q=` names it.
- `n.open()` opens the Notifications sheet (a More entry on a phone);
  the digest opens by itself when there is one.

### The API

| route | what |
|---|---|
| `GET /api/rules/suggested?repo=` | Rules worth adding: the narrowest pattern its approvals support (`Bash(python3 -m pytest:*)`, never `Bash(python3:*)`), and for 7 and 30 days every command it would allow (risky first: migrations, databases, network, deletes, root, writes outside the worktree), the prompts it removes and the wait it saves. A rule that covers a command you said No to is split or dropped. |
| `POST /api/rules/suggested/:id/accept` | `{ repo, pattern }`: writes only when `pattern` is the suggestion's exact pattern (400 without, 409 when it changed). |
| `POST /api/rules/suggested/:id/dismiss` | Not suggested again for that project. |
| `GET /api/rules/check?repo=&pattern=&agent=&command=` | The grant sheet's data: the file, whether the rule is too wide and why, what it would allow, a narrower rule. |
| `GET /api/rules/grants?repo=` | Every grant: Workbench's, Claude Code's `.claude/settings.local.json` lines, session-wide answers; who, when, where, how often used, and whether Workbench can revoke it. |
| `POST /api/rules/grants` | `{ repo, pattern }`: the sheet's narrower choice; refuses a too-wide rule. |
| `DELETE /api/rules/grants/:id?repo=` | Revoke. A line in your own checkout's settings is not edited: 409, with what to remove. |
| `GET`/`PUT /api/notify/settings` | `{ default: {level, minutes}, projects: [...], desktop }`; PUT `{ repo?, level: "all" \| "blocked" \| "alerts", minutes, reset?, desktop? }`. |
| `POST /api/notify/quiet` | `{ minutes }` (25 by default in the UI; 0 ends it). Critical alerts still notify. |
| `GET /api/notify/pending?repo=` | `notify`, `later` (with `due_ms`), `held` (quiet), `silent`, `quiet`, `presence`, and `digest` when you come back from quiet or from 5 min without the heartbeat. |
| `POST /api/notify/seen` | `{ until_ms }`: the digest was read. |

`GET /api/rules` and `DELETE /api/rules/:id` already belong to Decide's
text rules, so the grants live under `/api/rules/grants`.

### Built

- An accepted rule goes to the mark `allow:<repo>`, which `spawn` merges
  (`flow::with_allowed`) into `permissions.allow` of the agent's own
  `<data>/sessions/<id>/settings.json`, where Flow's applied rules go.
  Agents started afterwards get it; running agents keep theirs. Nothing is
  written to `~/.claude` or the project's `.claude` files.
- Grants are rows of `grants` (`src/rulesuggest.rs`); settings, quiet and
  the digest's last read are marks (`src/notify.rs`).
- Approved means answered Yes on a card, or in the terminal followed by
  the call running; refused means a new prompt or a Stop came first.
- Notification permission is asked only from the "Notify me" switch. A
  notification shows the agent, what it asks, the command and Claude
  Code's options, and says that nothing is answered from it.
- The dev harness is `ui/dev/rules-notify.html`, over a fixture recorded
  from a real server by `ui/dev/rules-notify-shoot.cjs --record`.
- Not built: `Edit`, `WebFetch` and MCP prompts get a suggestion (a
  folder, a host, the tool) but no test, and the sheet's narrower choice
  is for Bash commands only; the away digest (no heartbeat for 5 min) is
  built like the quiet one but has no test of its own; notifications have
  no action buttons, by design.

## What the rewrite changes

### `ui/app.js`

- Replace `SCREENS`, `LEVEL_OF`, `goLevel`, the rail and pill wiring, and
  the grid-as-home start with a router over the routes above. Keep
  today's routes as redirects.
- Add one `needs()` that counts waiting agents, the trust prompt and
  alerts that ask you. The button, Start, Continue and the phone's tab
  badge all use it.
- The agent switcher (from `renderRail` and `renderExited`) is grouped
  Needs you / Working / Ready / Finished.
- The zoom view becomes the agent page's Terminal and Changes tabs.
- `levelCtx` gains `project` (name and path) and `crumbs(list)`.
- Key handling drops the bare `z c t d [ ]` keys and never acts while a
  terminal has focus.
- On a phone (`matchMedia("(max-width: 640px)")`), the terminal attaches
  read-only until Type.

### `ui/index.html`

- Drop `#lvnav`, `#levels`, the `.rail`, `#dec-btn` and the grid as the
  first view.
- The top bar becomes brand, breadcrumb, Search, Needs you, Agents and
  More.
- Add the bottom tab bar (hidden above 640px).
- Use `viewport-fit=cover` in the viewport meta.
- The New agent dialog moves into B3's composer.
- Keep the sheet, the gate, the confirm dialog and the toasts.

### `ui/` modules

- New, under `ui/levels/` (or `ui/pages/`), after the mock's files:
  - `home.js`
  - `newproject.js`
  - `project.js` (`#/p`, Understand, Start a change)
  - `continue.js`
  - `agent.js`
  - `review.js`
  - `debug.js`
  - `shell.js` (the top bar, the drawer, the switcher, More, the tab bar)
- Add `blastCompact`, `churnBar`, `turnStrip`, `meter` and `testStrip`
  (in the mock's `dense.js`) to `common.js`.
- Move `phone.js`'s rules into `style.css`.
- Existing modules keep `mount(root, ctx)`:
  - `map.js` exports its unwrapped mount, so Understand can show it
    without the time bar.
  - `story.js` takes a `tab` option and a way to hide its agent picker.
  - `health.js` and `hotspots.js` mount as one "What is failing".
  - Each reused module gets ≤ 640px rules.

### `src/server.rs`

- Serve the new modules: the `LEVELS` list of `include_str!`s.
- `GET /api/repos` returns each project's short name, agents, counts and
  last activity, for Start.
- `GET /api/needs` (or `GET /api/attention`) returns the one count, with
  the trust prompt and alerts in it.
- Questions carry the full command and Claude Code's options, numbered as
  Claude Code numbered them.
- `POST /api/repos` takes `trust: true`: Workbench answers the trust
  prompt for its worktrees.
- `POST /api/agents` takes a permission profile (`allow`, `ask`, `deny`),
  written into the session's settings.
- `POST /api/goals` with `plan: "draft"` returns the lead's plan without
  starting anything.
- `POST /api/agents/:id/restart` and `/resume` (`claude --resume`).
  `POST /api/agents/:id/steer` takes Send back's note.
- `GET /api/review?repo=` returns each ready branch: tests on it,
  `git merge-tree` against main and the others, merge order, and a
  blast-radius summary.
- `GET /api/agents/:id/turns/:n/context` returns the prompt, the files,
  the tokens, the injected records with their scores, and the records
  that scored under the threshold.
- `GET /api/agents/:id/turns/:n/blast` returns the callers, the tests
  that failed after it, runtime errors blamed on it, and whether it
  reached main.
- `GET /api/records?repo=` with `PATCH` and `DELETE` (correct, forget,
  pin, through `chloectx`). Each record says who said it and on which
  branch.
- `PUT /api/repos/:p/threshold`.
- `/guide.md` serves the rewritten guide.

### `src/main.rs`

- `serve [PATH]` opens that project.
- `new [DIR]` starts a new project there.
- `open [PATH] [--continue|--review|--debug Q]` prints the URL of the
  running server, with the right route.

## The mocks

`ui/dev/journeys.html` is a clickable mock of every page above, built on
the shop demo repository: Python (`money.py`, `cart.py`, `coupons.py`,
`serve.py`), a checkout page in `web/index.html`, and their tests.

- It runs in the browser on in-page fixtures
  (`ui/dev/journeys/fixtures.js`).
- It mounts the real `map.js`, `story.js`, `ship.js` and `trace.js` where
  a step reuses them.
- `#/dense/*` shows the dense views at scale (38 files, 186 turns, 9
  agents).
- `phone.js` holds the tablet and phone layouts.
- Query parameters:
  - `?theme=dark|light`
  - `?first=1` (the first run)
  - `?open=needs|agents|search|more`
  - `?type=1` (the phone terminal after Type)
  - `?at=<selector>` (scrolls that element to the top)

Any static server over `ui/` serves it, for instance the levels mock:

```bash
node chloedb-workbench/ui/dev/levels-mock.mjs --port 7790
# http://127.0.0.1:7790/dev/journeys.html#/
node chloedb-workbench/ui/dev/journeys/shoot.cjs /mnt/project-files/workbench/journeys            # desktop and phone, dark and light
node chloedb-workbench/ui/dev/journeys/shoot.cjs OUT --phone 01-home 10-review-check            # some shots, one device
```

`shoot.cjs` takes each shot at 1440x900 and at 390x844 (touch), both at
2x, in dark and light. It reports any phone shot whose page scrolls sideways,
and writes `ready.txt` with one line per screenshot.
