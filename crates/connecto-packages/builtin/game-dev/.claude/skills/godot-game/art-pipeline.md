# Art from Blender into Godot

For whoever makes or integrates 3D art. The rules for sharing this work between sessions (exclusive folders, the manifest, the naming contract) are in `team.md` of the `game-design` skill.

## The tools

- **Check them first:** `command -v blender ffmpeg` (PowerShell: `Get-Command blender, ffmpeg`); if Blender is not on the PATH, try the places below. Blender and FFmpeg became required in version 3 of this package, so a project that turned it on earlier may lack them. If one is missing, tell the user to install it (the project's status in Project Settings says how on their system; Connecto finds it after a restart) and stop that step.
- **Blender (4.2 or later) and FFmpeg (4.4 or later)** are checked by Connecto, with Godot, before this package is turned on, so once found, use them without asking. Find Blender the way you found Godot: `blender` on the PATH (Homebrew links it there), `/Applications/Blender.app/Contents/MacOS/Blender` on macOS, `flatpak run org.blender.Blender` on Linux, `C:\Program Files\Blender Foundation\Blender <version>\blender.exe` on Windows. Check it with `blender --version` and write the path in the project's agent notes. Write scripts for that version: `bpy` names change between versions (render engine ids, material socket names), so read them from the installed Blender (`blender -b --python-expr "..."`) instead of from memory.
- **Downloads need the user's OK.** Anything fetched for the art (an animation pack, models, textures, fonts), even under CC0, is asked for first: say what, from where and under which licence.
- **Python for image work** (finishing textures with Pillow or numpy) is not installed by the package. Use a virtual environment in the project: `python3 -m venv .venv && .venv/bin/pip install pillow numpy`, then run `.venv/bin/python`. On Windows: `py -m venv .venv`, then `.\.venv\Scripts\python.exe -m pip install pillow numpy`, and run `.\.venv\Scripts\python.exe`. Add `.venv/` to `.gitignore` (Godot skips folders whose name starts with a dot). Do not `pip install` globally: Homebrew's Python refuses it. Scripts that run inside Blender use Blender's own Python and need none of this.
- **Generated images** (texture and icon masters, concept sheets) come from Codex's built-in image generation, which no package can install. Run `connecto agents`: if Codex is `ready`, start a Codex terminal and ask it for the masters, with exact sizes, file names and the folder to write to. If it is not, use placeholder art (flat colours, simple shapes, procedural textures) and tell the user that generated art needs Codex. Placeholder art is enough for the milestones either way.

## Where things live

- Godot imports only the exported `.glb` files, under `assets/`.
- Sources stay out of the importer: `.blend` files, generator scripts, downloaded packs and previews go in a folder with an empty `.gdignore` (for example `art_src/`). Set `import/blender/enabled=false` under `[filesystem]` in `project.godot`, or Godot tries to start Blender for every `.blend` it finds.
- A folder of assets gets a `MANIFEST.md`: files, sizes, triangle counts, materials, colliders, clips with their timing marks, conventions, how to rebuild.

## Kits built by script

Environment sets (walls, floors, props, rocks, plants) come out well from a Blender Python script run without the interface: `blender -b -P art_src/gen_kit.py -- <piece>|all`. The script is the source: a change to a convention is one edit and a rebuild, and the manifest's numbers can be generated with it.

- Fix the conventions first and write them at the top of the manifest: 1 unit = 1 metre, which axis is the front, pivot at the centre of the base, the module size pieces snap to.
- Set a triangle budget per piece and for a whole room, and count. The budget goes when every stone, tile and plank is its own bevelled object; model the surface as one piece and keep separate objects for what reads as a silhouette.
- Do not use the imported flat materials as they are. Map each material **name** to a game shader or material in one place in code, so the look of every model changes together.

## Check the import, then look

1. Import the new `.glb` files in a throwaway Godot project outside the game first (`godot --headless --path <dir> --import`), and load each one in a script: meshes, materials, vertex colours, colliders, skeleton and clips present. A bad export then never breaks the game other sessions are working in.
2. Render every asset in the engine, from the game's camera angle, in the game's lighting, next to something of known size. A Blender render says little about how it reads in the game.
3. Look at the pictures before reporting the asset as done.

## Importer quirks

- **Vertex colours ignored on the first surface.** Seen in Godot 4.7.2: on an imported glTF mesh the first surface's material comes in without "use vertex colour as albedo", the others with it. If a model's first material looks flat, either set the materials in code after loading (which the name mapping above does anyway) or export a tiny hidden dummy triangle as the first surface.
- **Colliders by name.** An object whose name ends in `-colonly` or `-convcolonly` becomes a static body with that shape and no visible mesh. Use boxes and cylinders sized to the footprint, not the visual mesh. Where gameplay authors its own colliders, strip the imported ones when instancing.
- **Loop flags** often do not survive export. Set looping for idle, walk and run in code or in the import settings.

## Characters

- **Do not key humanoid animation by script.** It comes out stiff however long you tune it. Use animation made by people: a library with a clear licence (CC0 packs are the simplest), downloaded without a browser once the user agrees, with the licence file stored next to the source and the pack, URL and date in the manifest. Retarget it to your skeleton by script, then trim, time-warp and cross-fade clips to the lengths gameplay asks for.
- **Do not deform a realistic head into a stylised one.** The proportions fight back: it comes out gaunt, then puffy. Build a stylised head from scratch as simple smooth forms and paint the face (eyes, brows, mouth, soft shading) as a texture.
- **Expect rounds.** A face is judged by taste. Show the user comparison sheets early, in the same views each time, beside the concept art.

Defects to look for yourself, frame by frame, before anyone else sees the character. Each was caught by eye on a model that had been reported as finished:

- the head reads as detached from the neck (separate objects, or a hard shadow line where they meet): weld them into one surface and smooth the shading across the join;
- clothing layers passing through each other at the shoulders, elbows and hips when the limbs move;
- holes you can see through, where hidden faces were deleted or cloth is one-sided;
- long hair, scarves or straps passing through the body in a run or a roll;
- a shield or tool on the wrong side of the arm or hand, or turned the wrong way;
- props that pop in and out between clips, instead of staying or changing during a covered moment;
- feet sliding at the game's real movement speed.

## Judging animation

A still frame hides most animation defects.

- Record the clips in the game with `--write-movie` and `--fixed-fps`, extract **consecutive** frames with ffmpeg, and lay them out as a contact sheet: `ffmpeg -i reel.avi -vf "select='between(n\,0\,35)',scale=320:-1,tile=6x6" -frames:v 1 sheet.png`.
- Have the recording script print the movie frame at each event (`attack start`, `hit lands`, `clip change`), so you know which frames to pull.
- To find a pop at a blend between clips, log the world position of a few joints (hand, foot, head) every frame and look for jumps in the numbers. The eye sees that something is wrong; the log says which frame and which bone.
