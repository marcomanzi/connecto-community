# Godot gotchas

Each of these cost a real project time (a 3D action game on Godot 4.7.2). The cause is given so you can recognise relatives of the same mistake.

## Nodes and effects

- **An effect appears at the world origin or in the wrong place.** A `global_position` set before the node is in the tree is taken as a local position, and a node made `top_level` before it enters the tree sits at the origin plus its local offset. Add the node to the tree first, then set `global_position`. Effects that must not follow their actor (rings on the ground, particles, trails) are the usual victims: place them again every time they are shown, and look at each one in a screenshot from the game camera, away from the origin.
- **A test asserts on a node that was freed.** After `queue_free()` or a scene change the variable still exists. Check `is_instance_valid()` first, or read what you need before the object goes.
- **Shaders stutter the first time they are seen.** A new material compiles when it is first drawn, which is usually the first hit of the first fight. At level load, draw each enemy, effect and special material for a few frames behind the fade.

## Time

- **Hit-stop that lasts a different time at different frame rates.** A freeze timed in seconds while `Engine.time_scale` is near zero depends on how frames fall. Count it in physics ticks: `SceneTree.physics_frame` keeps firing at its real rate whatever the time scale is.
- **Anything a test or a bot waits for** is counted in physics ticks too, not in wall-clock time, so a run recorded with `--fixed-fps` replays the same on a slow machine.

## Physics and movement

- **Characters stand on each other.** A `CharacterBody` treats another body's capsule as floor, so an enemy that leaps lands on the player's head and stays. After `move_and_slide()`, if the floor collision is another character, keep gravity on and slide off sideways.
- **Attacks and lock-on go through walls.** Overlap tests know nothing about walls. Before a hit, a lock or an area effect counts, cast a ray on the world layer from attacker to target.
- **A large enemy stuck behind props.** A navigation mesh is baked for one agent radius. Bake it for the largest body that uses it (or one mesh per size), and test that every room stays connected at that radius.
- **Collision inherited from art.** A collider generated from a detailed model snags the player on decoration. Author simple boxes for what blocks movement and keep the walking lanes clear; let only the few solid props keep their own collider.

## Camera and rendering

- **Scenery hides the player** in a fixed or isometric camera. Fade whatever stands between camera and player. Test the sight line against the object's oriented bounds, or a few simplified volumes for an L-shaped building; a single axis-aligned box fades half the scene.
- **A water surface that vanishes in waves.** A surface moved by its vertex shader at the same height as its basin dips under the rim for half of each wave, and its bounding box no longer matches what is drawn. Keep the surface clear of the geometry around it, and give displaced meshes a larger `custom_aabb` or `extra_cull_margin`.
- **Flicker where two surfaces share a plane** (decals, floor tiles on a floor, paths on terrain). Lift one by a few millimetres or cut the hole.

## Project and import

- **A project theme breaks a clean import**: see "Check every change" in the skill. Set themes on UI roots.
- **Fonts**: without a font in the project the game uses Godot's built-in Open Sans SemiBold, which is the same everywhere but is not the game's own look, and has no CJK characters or emoji. Those characters are taken from the machine's system fonts, so they look different on every machine or are missing. Ship font files that cover every language and symbol the game shows, whose licence allows it, with the licence file next to them.
- **Text that overlaps or runs off** at another window size. Look at every screen at two or three resolutions and with the longest strings of the game's language.
- **Vertex colours missing on part of a model**: see `art-pipeline.md`.

## Saves

- Write to a temporary file, then rename it over the save. A crash mid-write otherwise leaves half a file.
- Validate the whole file before replacing any live state, and replace all of it or none. A save that is rejected must leave the running game exactly as it was.
- Store stable ids and a format version. Loading replaces state; it never merges into what is there, or items duplicate.
