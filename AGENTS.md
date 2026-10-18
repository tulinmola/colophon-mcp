# Colophon MCP — agent instructions

Colophon MCP gives an agent the machine: a server speaking the Model Context Protocol, through which an agent stands a machine up, runs it, stops it and reads it. The machine itself lives in the sibling repository `colophon-emulator`, under its own instructions; read those before touching anything that crosses into C. What a finding becomes is `colophon-archive`'s to say. `README.md` is the prologue.

## The rule everything follows from

The protocol changes; the machines do not. Nothing an agent finds may live only here: a finding leaves as something the archive can derive again — the copy it was read from, named by its hash, the build it ran on, and what was done to the machine and at which tick.

- No clock decides how far a machine runs. A machine advances only when a tool asks it to, and every tool that advances one says by how many ticks it did, so the same calls on the same copy and the same build give the same answers.
- A build is named for the emulator commit it was compiled from and for this repository's own, each marked when the files it is compiled from carry uncommitted changes, an untracked file among them, and the server answers with that name. A finding made on a marked build is made again on an unmarked one, at commits the published repositories hold, before it leaves.

## The machine

- No machine is the ordinary case. A tool that builds a machine is never given a default for which one, and a machine is named as the emulator's command line names it.
- The core allocates nothing, so every buffer a machine is given is Rust's, and none may move or be freed while the machine stands or a state of it is kept. A machine holds pointers to its buffers, and a CPC's also points into itself — its disc controller points at its own drives. A state is the machine's own bytes, pointers and all, with the buffers they point at as they stood, so a machine stays where it was built and a state is restored into the machine it was taken from.
- Machines share the code and nothing else, save one buffer the core keeps for itself: the empty socket a CPC reads as &FF when neither the upper ROM it selects nor ROM 0 is fitted, which each `cpc_init` fills. No CPC is built while another is being built or is running.
- A machine stopped mid-instruction has a PC that belongs to no instruction anyone can name, so every tool that stops one leaves it between instructions.

## The server

- Several machines stand at once, each behind a handle the server gives it, because a game's releases are read side by side. A tool that acts on a machine names it by its handle.
- The server speaks over standard input and output, one client to a process. It answers in the revisions a test speaks, and a revision is supported the day a test speaks it, not before.
- Each machine is built on a thread of its own and never leaves it. A machine holds raw pointers, a CPC's into itself, and nothing claims it can be sent between threads.
- Rust holds the protocol, the handles, the threads and the encodings. Its types for the core's structs are generated from the headers by bindgen, and so is everything that reads them by name; none of it is written by hand, so a renamed field breaks the build and a new one arrives without anyone listing it.

## The bridge

- The core is compiled here from a checkout of the emulator standing beside this one, and between it and the server stands a bridge of this repository's own, in `emulator/`, as the player, the archive and the desk each keep theirs. They call theirs the host; here it is the bridge, because in the protocol's own terms the host is the application the agent runs in.
- Whatever happens once a tick happens in the bridge, and nothing crosses into Rust once a tick. It keeps no globals: every machine is a context of its own.
- The bridge is C, and the emulator's conventions hold in it: C99 with `-Wall -Wextra -Werror`, copies of its `.clang-format` and `.clang-tidy`, public identifiers wearing `mcp_` as the player's wear `player_`, and pins touched only through the accessors in the chip headers.

## Tools

- A tool is added the day someone needs it and not before. The first stand machines up, from power-on or from a snapshot, list them and put them away, run them by the frame and step them by the instruction, read and set anything they hold by its path, read and write their memory, stop them where they are told and gather what they do on the way, call their routines, show their picture and read their memory as one, disassemble it, type at them, give them their discs and tapes, and keep them as they stand to be taken up again; anything further waits for a colophon to ask for it.
- A tool's description is the reference for it, as a header is for the core, and a page never restates a schema.
- A tool speaks in the machine's terms. A field keeps the name its header gives it.
- A tool that succeeds answers with a JSON object, and the same JSON as text beside it: the older revisions take no other structured answer, and the protocol asks for the text. One that fails answers with text alone, saying what would have worked.
- What can be told as text is told as text. A picture is returned only for what only a picture shows, and comes with text saying which machine, at which tick, and at what size.
- An error says what would have worked.

## Evidence

- A tool exists when a test grades it, and a test arrives in the change that writes what it grades.
- Prefer evidence we did not write: a ROM disassembly someone else made, the firmware's own tables. A test built from our own understanding agrees with our own mistakes. The emulator's command line, run on the same copy, is a second program around the same core: it shows that the server reports what the machine did, never that the machine did it right.
- The protocol is graded by the specification's own sentences, quoted in the tests that hold the server to them, and by the protocol's own Inspector, a client this repository did not write.
- No firmware and no software is kept here. The tests write the few bytes they run, and anything heavier reads the emulator's own fetched copies.

## Crates

- A crate is brought in with the first thing that uses it, pinned to an exact version, and every result taken from it is fixed by a test, so an upgrade that changes one fails instead of drifting. A crate that cannot be held still that way is replaced by our own.
- The toolchain is pinned as well, because rustfmt and clippy change with it.

## Documentation

Documentation lives in `docs/` and is this repository's own: a page is a markdown file named `<name>.<language>.md`, and it becomes a folder holding `index.<language>.md` the day it carries artifacts, so that what a page cites sits beside the prose citing it. A page's title lives in its front matter and the layout that publishes it sets the heading, so a document has exactly one `<h1>` and the markdown starts at `##`; `order` places a page among its siblings, and two siblings may not claim the same one. `docs/index.en.md` carries no order at all, because where this documentation sits among the other projects' is not ours to say. A link between pages names the markdown file it points at, and a link to a sibling repository is a URL, never a relative path into a tree that is not there.

The Colophon Project publishes the documentation it gathers. Nothing about that site is written down here: it owns its own addresses, and a page that hardcodes one is wrong twice over.

A page describes what the server does today, and what it owes a reader is its limits in plain words: what is missing, and what its absence costs. A change to what the server does updates its pages in the same change; documentation left behind is a false claim in the project's own voice. What it is built toward belongs in `README.md`.

## Voice

- The register is a scribe's: plain, declarative, a little antique. Take the manuscript metaphor completely seriously and never wink at it — pointing at the bit kills it.
- Like an illuminated manuscript: mood at the openings, discipline in the middles. A section's first sentence may sing; commands, specs, and rules stay dry.
- Humour only as a byproduct of honesty. None in code comments, error messages or tool descriptions: comments pay rent in facts, error messages are read on bad days, and a tool description is taken at its word.
- Counts are words and measures are figures: two machines, eight rows, against 16K, 69888 T-states, `&C000`. A machine's sizes are what it is called, not how much of something there is.
- Write for 2036. Mood is timeless, jokes are timestamps; the prose meets the same bar as the code.

## Writing style

- One paragraph, one line: markdown is never hard-wrapped. Soft wrap does the work.
- Sources are cited at their point of use, in the code or doc that uses them: link, what we learned, what we changed. No link dumps.
- The README's `Sources` section is a view over those citations, never a collection of its own: a source is promoted there the day code starts citing it, one entry saying what it is and what it gives us.

## Code conventions

- rustfmt and clang-format decide formatting, and it is never a discussion; clippy, with its warnings denied, and clang-tidy decide the rest.
- Every comment is a lost battle. The survivors: hardware facts, cited sources, and constraints the code cannot express — an `unsafe` block's comment says what it relies on. Never narrate what the code already says; rename or restructure instead.
- Wrappers around the bridge mirror its C names mechanically: the prefix falls away, a machine's name becomes the type the function is a method of, `_t` falls away, and what remains takes Rust's case — `mcp_cpc_run` becomes `Cpc::run`, and `mcp_run_t` becomes `Run`. A wrapper that renames what it wraps hides the emulator from anyone reading both.
- Documented names for documented things: a register, a pin or a counter keeps the name its primary documentation gives it, as the core's headers already do. Every deviation from the documentation — an active level, a prime mark — is declared where it is defined.
- Invented names explain themselves. Abbreviations count as cryptic (`err`, `msg`, `buf`, `cur`): write the word. Abbreviations that are documented names (`RFSH`, `WZ`), a language's own (`int`, `argv`, `u8`, `Vec`, `fn`), or the name of a real thing in the repo (the `src/` folder) stay. Single letters only in scopes a few lines long.
- Of two options that work, the one with fewer concepts and fewer lines.

## Working

- `make check` is formatting, linting and the tests together; run it before handing work back. The Makefile is the list of targets and this file does not repeat it.
- Never commit, never push. The human reviews; the human commits.

## Unsettled

What a finding looks like on disk, which is the archive's to settle. Restoring a state into a machine other than the one it was taken from. Transports other than standard input and output.
