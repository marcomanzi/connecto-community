# Building a game with several sessions

For a coordinator who hands the game out to parallel sessions: gameplay coders, art sessions, reviewers. One session working alone needs only the two contracts below.

## Order of work

1. Gameplay first, on placeholders, one milestone at a time, each closed by a playtest.
2. Art in parallel, **as files only**: an art session produces models, textures or UI art in its own folder and does not touch scenes or scripts.
3. Integration as a separate step, after the gameplay milestone passed: one session swaps placeholders for the finished assets and checks the result in the running game.

Keeping these apart is what lets a dozen sessions work at once. An art session that also edits scenes collides with the coder who owns them.

## Ownership

- Give every session folders it alone writes (`assets/models/enemies/`, `scripts/combat/`...). Write the list into the project's agent notes, so that a session started later can read it.
- Name the shared files nobody owns alone (`project.godot`, the main scenes, autoloads, the agent notes) and give them one rule: re-read the file just before editing it, make small additive edits, and tell the owner what you changed.
- Sessions that depend on each other talk directly (`connecto send <id> --ask`), not through the coordinator. Give each one the other's id.
- Before handing a session more work, check its context (the CONTEXT column of `connecto list`). Review fixes and refinements of its current task stay with it, but past about 60% have it write a handoff note and continue in a fresh session. Anything else (a new task, an unrelated bug report, a queued follow-up, extra scope) goes to it only at 30% or less, an unknown figure (`-`) counting as over; otherwise start a fresh session, and never stop a session in the middle of its task.
- Tell every reviewer what is out of scope: which folders are another session's work in progress. Otherwise it reports their half-done work as your coder's bug.

## Commit at every passed milestone

Without commits, one session's unfinished edit fails another session's tests, and nobody can tell whose change broke what. Commit when a milestone passes its playtest, so there is a known good state to compare with and go back to. For work that touches the same files as someone else's, use a git worktree per session and merge. Ask the user before the first commit if the project has none.

## The contract between gameplay and art

Agree on it before either side starts, and write it down next to the code:

- **Names**: the animation clips (`idle`, `walk`, `attack_light`, `hit`, `death`...), the attachment points, the material names.
- **A small visual interface** that every character or enemy model offers, for example `play(name, speed)` and optionally `flash(color, seconds)`. Gameplay calls it and never reads an animation back.
- **Timings belong to gameplay.** How long a wind-up lasts and when a hit lands live in data files the designer tunes. The model fits its clips to those numbers (the art marks the frame where the blade arrives; the visual stretches the clip so that frame falls on the hit). If the clip length drove the rules instead, every art change would change the balance.

With this, a coloured capsule and the finished rigged model are interchangeable, and neither side waits for the other.

## A manifest in every asset folder

Each folder of produced assets carries a `MANIFEST.md` written by the session that made it. It is what the integrating session reads instead of asking. It lists:

- every file, with its size in the world, its triangle count, its materials and whether it has colliders;
- the conventions: units, which way is front, where the pivot is, how pieces snap together;
- for animated models, each clip with its length, whether it loops, and its timing marks;
- how to rebuild it (the generator script and its command), and where the previews are;
- for anything not made in the project, the source, its URL, its licence and the date it was downloaded, with the licence file stored next to the source.

Also note what is known to be wrong or unused. The next session cannot tell a leftover from a part.

## Which model for which art job

- Work that is made of code goes to the strongest coding model: shaders, procedural effects, lighting, models built by a Blender script, UI built in code.
- Work that is a picture goes to a model that makes images: painted textures, icons, UI art.
- Whichever made it, judge the result inside the engine, from the game camera, in the game's lighting.

For an asset the user cares about and that keeps coming back from review (a face is the usual one), have a second model make a competing version and let the user choose. It costs little compared with more blind rounds.

## The user's eye

For anything judged by taste, show the user early and often instead of polishing alone. Send comparison sheets: before and after, candidate beside the concept art, the same views every time. Expect several rounds on faces and characters, and plan for them.
