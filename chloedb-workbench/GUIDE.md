# Workbench: a guide

Workbench runs several Claude Code agents side by side, each in its own
terminal and most often its own git worktree, and shows you what they are
doing: what needs you, what happened since you left, what is ready to
review, and why the code is like it is. This page says what each place is
for and when to open it. The README beside this guide is the reference for
installing it and for its HTTP API; [JOURNEYS.md](JOURNEYS.md) is the
design.

> You can read a project without starting any agent. Open its folder
> (Start, or `#/p`; Search and Debug offer **Open a repository**), then
> **Understand** answers questions about it from
> memory, the code and git, and **Search** (<kbd>/</kbd>) finds code, docs
> and decisions.

## The first ten minutes

1. **Start the server and open its link.** `chloedb-workbench serve` prints
   a URL with a token. Open it. **Start** asks what you came to do.
2. **Open a project.** Type the folder of a git checkout (<kbd>Tab</kbd>
   completes folders, git checkouts come first) and press Open. You land on
   the project's page, **Since you left**. On a first visit it says so;
   later it counts from when you last looked.
3. **Understand it first, if you like.** **Understand** has a box: ask
   "why does the total round half up?" and the answer comes as cards, each
   with its source: what ChloeCtx remembers (who said it, when), the code
   (file and line), and the commits that changed it. No model is called.
4. **Start a change.** Say what should change and how you will know it is
   done (a command in backticks makes the agent run it). Before anything
   starts, Workbench checks your sentences against the project's rules,
   its guarded paths and the verbs that destroy (delete, drop, force-push):
   a sentence that matches is shown, and you remove it, start read only,
   or start anyway. The form also suggests a model from this project's past
   tasks, warns when the done check runs a flaky test, and keeps your draft
   if you leave the page. <kbd>⌘</kbd><kbd>↵</kbd> (<kbd>Ctrl</kbd><kbd>↵</kbd>
   off a Mac) from any field starts it. Tick **Split it into parts** and a
   lead drafts the parts first; you edit them, and each part's model,
   before anything runs.
5. **Answer what it asks.** A question appears as a card in **Needs you**
   (the top bar's button, and the project's page). Press a digit, then
   <kbd>↵</kbd>. A long command must be read first: <kbd>e</kbd> or
   <kbd>Space</kbd> shows it all. An answer that widens what the agent may
   do (“don't ask again for …”) opens a check of the exact rule first.
6. **Tell it something.** On the agent's page, **Tell** sends your words
   exactly as written: typed into its prompt when it waits there, or given
   at its next tool call when it works. When it is showing a question,
   your words are queued, and nothing is typed into that question. A
   sentence that begins "From now on", "Always" or "Never" can also become
   a rule for every agent of the project.
7. **Review and ship.** When an agent is done, **Review and ship** shows its
   changes, the decisions behind them and the merge.

## The places

### Start

What you came to do: continue a project, review what is ready, start a new
project, or find out why something went wrong.

### A project: Since you left, Understand, Start a change

`#/p/<project>` is **Since you left**: one sentence on what happened since
you last looked (agents that ended, branches merged, outcomes done), the
cards that need you, the agents by state, and the decisions made. A goal
split into parts says how many are merged ("3 parts, 1 merged, 2 left")
and that it is done once every part is. **Preview main** shows the app as
the main checkout serves it, after a merge, through the same preview as an
agent's. **I'm
stepping away** marks the moment; it asks the browser for notifications and
says plainly what will reach you (a desktop notification while the tab is
open; nothing by phone or email).

**Understand** answers questions about the project without starting an
agent, and shows who is changing which file.

**Start a change** is described above. **May do without asking** decides
the permissions: **Edit files on its branch and run the tests** lets it
edit with the Edit and Write tools and run reading commands, the tests and
local git, alone or chained; anything on the network, a delete or a guarded
command still asks. **Read only** starts it changing nothing.

### The agent page

`#/a/<agent>`: its task (once, folded: **Show the whole task**), the
question it asks you, its Progress (a chapter per turn), Changes, Preview
and Terminal. **Review and ship** and **Tell**
stay side by side in that order, so one never turns into the other under
the pointer.

### Review and ship

An agent's diff, its blast radius, the decisions behind it, and the merge.

### Debug: find out why

Pick an agent and a turn, and Debug walks to where it went wrong.
**Why the code is like this** (Trace) answers "when and why did this
change" from git on every branch, Workbench's events and ChloeCtx; its
example questions come from this project's own most-changed files and
commits; when a later commit undid one that gave a reason, it says the two
reasons conflict. **What is failing** lists the tests the agents ran, flaky ones
marked. **What agents remember** is ChloeCtx's records.

On a line of a diff, **Why?** says who wrote it and why: the agent's edit
(an edit made with a shell command counts too), its decision, or
`git blame` and the commit when no agent touched it.

### More

**Terminals** (every agent's live terminal), **Where the time went**,
**Decisions** (a timeline per agent), **Library** (code and docs search),
**Rules** (what every agent is told, what they may run without asking,
every grant with Revoke, and the guarded commands and paths), **Guide**
(this page), **Keys** and **Setup**.

Old addresses still work: `#/decide` opens More › Rules, `#/trace` opens
Why the code is like this, `#/mission` opens Start a change.

## The keyboard

| keys | what |
|---|---|
| <kbd>/</kbd> | Search: <kbd>↑</kbd> <kbd>↓</kbd> move, <kbd>↵</kbd> opens, <kbd>⇧</kbd><kbd>↵</kbd> on code says why it is like this, <kbd>Tab</kbd> changes the scope, <kbd>Esc</kbd> closes |
| <kbd>.</kbd> | Needs you |
| <kbd>a</kbd> | Agents |
| <kbd>n</kbd> | Start a change in this project |
| <kbd>g</kbd> then <kbd>s</kbd> <kbd>c</kbd> <kbd>r</kbd> <kbd>d</kbd> | go to Start, Continue, Review, Debug |
| <kbd>j</kbd> <kbd>k</kbd> | the next or previous card, else item |
| <kbd>]</kbd> <kbd>[</kbd> | the next or previous card that needs you |
| <kbd>1</kbd>…<kbd>9</kbd> then <kbd>↵</kbd> | on a focused question card, Claude Code's option |
| <kbd>e</kbd> or <kbd>Space</kbd> | on a question card, show the whole command |
| <kbd>x</kbd>, then <kbd>⇧</kbd><kbd>1</kbd>… | select cards, then answer them alike (identical commands only) |
| <kbd>Esc</kbd> | close the sheet or drawer; else up one level |
| <kbd>?</kbd> | Keys |
| <kbd>Ctrl</kbd><kbd>b</kbd> then <kbd>Esc</kbd> | leave a terminal |

Single keys act only outside a text box and a terminal: inside a terminal
every key goes to the agent (Claude Code uses <kbd>Esc</kbd> to interrupt).

## Words

- **Agent**: one Claude Code session Workbench started, with a name.
- **Worktree**: a second checkout of the repository on its own branch
  (`wb/<name>`), so agents don't edit the same files on disk.
- **Goal** and **part**: what you want, and the pieces a lead split it into,
  each with an agent.
- **Rule**: something you decided once, told to every agent of the
  project.
- **Guarded path** or **command**: agents may read the path but not change
  it; the command is denied and comes to you as a request.
- **ChloeCtx**: Workbench's memory: what was decided, by whom and why,
  across sessions.
