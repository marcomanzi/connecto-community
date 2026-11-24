---
name: game-design
description: Turn a game idea into a one-page design a solo developer can finish, with milestones whose "done" can be seen in a screenshot or video. Use when the user has a game idea, asks for a GDD, a core loop, scope, or what to build first, or before starting a new Godot game.
---

# Game design, kept to solo size

The design is the contract for everything after it: the Godot work builds toward it and the playtests judge the game against it. Keep it to one page. A small game that is finished beats a big one that is not.

## 1. Ask first

Ask the user, in one message, only what you cannot guess:

- the genre and a game it should feel like;
- 2D or 3D;
- the target platforms (desktop, web, Android, iOS);
- the art style (shapes and colours to start with, pixel art, low-poly...), and any concept art or reference images they have;
- how much time they have (a weekend, a month...).

If the user brings a design that is already written (a long GDD, an art bible), do not shrink it or rewrite it. Condense it faithfully into `design/gdd.md`, name the source file at the top, and put your effort into choosing the first slice and its milestones.

## 2. Write `design/gdd.md`

One page, these headings:

- **Pitch**: one sentence.
- **Pillars**: two or three feelings the game must give. Every feature serves one, or it goes to the cut list.
- **Core loop**: what the player does every few seconds, and why they do it again.
- **Verbs**: the player's actions (move, jump, shoot...). Few.
- **Win and lose**: how a run or a level ends.
- **Content**: the levels, enemies, items for the first version, with numbers.
- **Cut list**: the good ideas that are not in the first version. Put things here freely.
- **Controls**: keyboard, gamepad, touch.

Two rules that save rework in any game where the player reacts to danger:

- **Put the warning on the thing that is dangerous**: a pose, a glow, a sound. Markers on the ground and other overlays are a difficulty assist; decide with the user whether they are on by default. A readable enemy works at every difficulty, while an overlay teaches the player to watch the floor.
- **Reward the skill a pillar names with a rule of its own**, such as a window of extra damage after a dodge or a parry, not with a global change to damage or health. Then show the player why a hit was weak or strong. Without this, mashing the attack button or trading hits and healing becomes the best way to play, and no number tuning fixes it.

## 3. Milestones

Add them to the GDD in this order. Each has acceptance criteria someone can check by looking at a screenshot, a short video or a log, never "it feels good":

1. **Graybox**: the verbs work with plain shapes. For example: "the player moves and jumps; a video shows a jump onto a platform".
2. **First playable**: the core loop from start to win or lose.
3. **Vertical slice**: one level as it will ship: art, sound, UI.
4. **Content**: the rest of the content list.
5. **Polish**: game feel, menus, settings, performance.
6. **Ship**: exports for the target platforms.

Work on one milestone at a time. When a request would add scope, say which pillar it serves and offer to put it on the cut list instead.

Write the criteria so a playtest can use them as they are:

- From the first playable on, give each milestone an **acceptance scenario**: the numbered steps a player takes from the start of the game to the end of the milestone ("new game, cut wood, sow, sleep, harvest, sell, buy..."). A playtest bot replays exactly this list. An in-game objective list that advances on real game events can be the same list, so the onboarding and the end-to-end test stay one thing.
- Where feel matters, give **a number with both bounds**: "the boss falls in 40 to 70 seconds to a player who parries". With only an upper bound, a fight that is over in five seconds passes.
- Keep "the level can be finished" apart from "the level is balanced". The first is a criterion a bot can check. The second is the user's to judge by playing; give them numbers to judge with, such as the damage each enemy dealt in a run.
- The loop comes before the art. Up to the first playable the game runs on placeholders; final art replaces them through an agreed contract (see `team.md`) once the gameplay milestone has passed its playtest. Art integrated into rules that are still changing gets redone.

## 4. With several agents

When more than one session builds the game (coders, artists, reviewers in parallel), read `team.md` in this skill's folder before handing out work: who owns which folders, the contract between gameplay and art, the manifest each asset folder needs, and which kind of model suits which art job.

## 5. Hand over

Tell the user the design is in `design/gdd.md`, then go on with the `godot-game` skill for the first milestone. After each milestone, use `game-playtest` to check its acceptance criteria.
