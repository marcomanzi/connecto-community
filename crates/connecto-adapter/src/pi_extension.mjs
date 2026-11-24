// Connecto's extension for Pi. Connecto writes this file into the terminal's
// run folder and loads it with `pi -e`. A line `const CONFIG = {...};` is put
// in front of it at launch:
//
//   hook      absolute path of Connecto's hook helper
//   inbox     whether notices from other terminals are handed to the model
//   lmstudio  base URL of a local LM Studio server, or null
//   bridge    the bridge text, how to use the `connecto` command (absent when
//             the bridge is off for the terminal)
//   recap     the built-in package's end-of-task status recap (absent when
//             the project does not have the package)
//
// It does two things. It tells Connecto when a session starts, a prompt is
// accepted, a turn settles and Pi waits for the user, by running the helper
// with the same payloads Claude Code's hooks send. And it offers the models a
// local LM Studio server has, under the provider `lmstudio`, and loads the one
// in use before Pi calls it, with a context Pi fits in: left to itself LM
// Studio picks the context from free memory, and can pick one too small.
//
// Nothing here may disturb Pi: every failure is swallowed.

import { spawn } from "node:child_process";
import { closeSync, openSync, statSync, unlinkSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";

const HELPER_LIMIT_MS = 8000;
// The prompt sections the bridge text and the package recap go in, in this
// order.
const BRIDGE_SECTION = "connecto-bridge";
const RECAP_SECTION = "connecto-status-recap";
const MODELS_LIMIT_MS = 3000;
// Pi's own instructions and tools take more than this many tokens.
const MIN_CONTEXT = 8192;
// The context a local model is loaded with, when it can take that much, and
// the smaller one tried when that does not fit in memory. LM Studio refuses
// to serve Pi below the smaller one.
const LOAD_CONTEXT = 32768;
const LOAD_CONTEXT_SMALL = 16384;
// Loading a large model takes a while; another terminal's load is waited for
// this long before this one tries.
const LOAD_LIMIT_MS = 300000;

/** Run the helper with `payload` on stdin. Resolves with what it printed. */
function helper(event, payload, context) {
	return new Promise((resolve) => {
		let out = "";
		let done = false;
		const finish = () => {
			if (!done) {
				done = true;
				resolve(out);
			}
		};
		try {
			const args = ["pi", "--event", event];
			if (context) args.push("--context");
			const child = spawn(CONFIG.hook, args, { stdio: ["pipe", "pipe", "ignore"], windowsHide: true });
			const timer = setTimeout(() => {
				child.kill();
				finish();
			}, HELPER_LIMIT_MS);
			child.stdout.on("data", (chunk) => {
				out += chunk;
			});
			child.on("error", () => {
				clearTimeout(timer);
				finish();
			});
			child.on("close", () => {
				clearTimeout(timer);
				finish();
			});
			child.stdin.on("error", () => {});
			child.stdin.end(JSON.stringify({ hook_event_name: event, ...payload }));
		} catch {
			finish();
		}
	});
}

// Reports leave in the order they were made.
let queue = Promise.resolve();
function report(event, payload) {
	queue = queue.then(() => helper(event, payload, false)).catch(() => {});
	return queue;
}

/** The text of a message's content: a string, or its text parts joined. */
function textOf(content) {
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.filter((part) => part && part.type === "text" && typeof part.text === "string")
		.map((part) => part.text)
		.join("\n");
}

/**
 * The notices in what the helper printed. It prints them the way Claude
 * Code's prompt hook wants them, as JSON around the text.
 */
function inboxText(printed) {
	const text = String(printed).trim();
	if (!text) return "";
	try {
		const context = JSON.parse(text)?.hookSpecificOutput?.additionalContext;
		return typeof context === "string" ? context.trim() : "";
	} catch {
		return text;
	}
}

function session(ctx) {
	const found = {};
	try {
		const id = ctx.sessionManager.getSessionId?.();
		const file = ctx.sessionManager.getSessionFile?.();
		if (id) found.session_id = id;
		if (file) found.transcript_path = file;
	} catch {
		// An ephemeral session has neither.
	}
	return found;
}

async function lmStudioModels(base, signal) {
	// The native listing says which models are chat models and how much
	// context each takes; the OpenAI one only names them.
	try {
		const response = await fetch(`${base}/api/v0/models`, { signal });
		if (response.ok) {
			const { data } = await response.json();
			if (Array.isArray(data)) {
				return data
					.filter((model) => model && typeof model.id === "string" && model.type !== "embeddings")
					.map((model) => ({
						id: model.id,
						// What the model can take, not what it happens to be loaded with.
						context: model.max_context_length ?? model.loaded_context_length,
						// What Pi will have: the context it is loaded with, or the one
						// Connecto will load it with.
						window:
							model.state === "loaded" && model.loaded_context_length > 0
								? model.loaded_context_length
								: loadContext(model.max_context_length),
						loaded: model.state === "loaded",
						images: model.type === "vlm",
						helper: model.publisher === "model-extension-pack",
					}))
					// LM Studio's own helper models (speech and the like) are listed as
					// chat models and are not; nor is one too small to hold Pi's prompt.
					.filter((model) => !model.helper && !(model.context > 0 && model.context < MIN_CONTEXT))
					// The roomiest first: Pi starts on the first one it is given.
					.sort((a, b) => (b.context ?? 0) - (a.context ?? 0));
			}
		}
	} catch {
		// An older server, or another OpenAI-compatible one: ask the plain way.
	}
	const response = await fetch(`${base}/v1/models`, { signal });
	const { data } = await response.json();
	return (Array.isArray(data) ? data : [])
		.filter((model) => model && typeof model.id === "string" && !/embed/i.test(model.id))
		.map((model) => ({ id: model.id }));
}

/** The context to load a model with that can take `max` tokens. */
function loadContext(max) {
	return Number.isFinite(max) && max > 0 ? Math.min(max, LOAD_CONTEXT) : LOAD_CONTEXT;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** The model `id` as the server lists it now, or undefined. */
async function lmStudioModel(base, id) {
	const response = await fetch(`${base}/api/v0/models`, { signal: AbortSignal.timeout(MODELS_LIMIT_MS) });
	if (!response.ok) return undefined;
	const { data } = await response.json();
	return Array.isArray(data) ? data.find((model) => model && model.id === id) : undefined;
}

/** Ask the server to load `id` with `context` tokens. Resolves with an error text, or "" when loaded. */
async function loadOnce(base, id, context) {
	try {
		const response = await fetch(`${base}/api/v1/models/load`, {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ model: id, context_length: context }),
			signal: AbortSignal.timeout(LOAD_LIMIT_MS),
		});
		if (response.ok) return "";
		const text = await response.text();
		try {
			const body = JSON.parse(text);
			return String(body?.error?.message ?? body?.message ?? body?.error ?? text);
		} catch {
			return text || `LM Studio answered ${response.status}`;
		}
	} catch (err) {
		return err instanceof Error ? err.message : String(err);
	}
}

/**
 * Make sure the local model `id` is loaded before Pi calls it. One terminal
 * loads it, others wait for that load: two loads at once would load it twice.
 * Resolves with an error text, or "" when it is loaded (or the server cannot
 * say, in which case Pi's own call reports what is wrong).
 */
async function ensureLoaded(base, id, status) {
	let model;
	try {
		model = await lmStudioModel(base, id);
	} catch {
		return "";
	}
	if (!model || model.state === "loaded" || model.state === undefined) return "";
	const lock = join(tmpdir(), `connecto-lmstudio-${Buffer.from(`${base} ${id}`).toString("hex").slice(0, 80)}.lock`);
	const deadline = Date.now() + LOAD_LIMIT_MS;
	let held = false;
	while (!held) {
		try {
			closeSync(openSync(lock, "wx"));
			held = true;
		} catch {
			// Another terminal loads it, unless its lock is stale.
			try {
				if (Date.now() - statSync(lock).mtimeMs > LOAD_LIMIT_MS) unlinkSync(lock);
			} catch {
				// Gone already.
			}
			if (Date.now() > deadline) return "";
			status(`Waiting for LM Studio to load ${id}…`);
			await sleep(1000);
			try {
				if ((await lmStudioModel(base, id))?.state === "loaded") return "";
			} catch {
				return "";
			}
		}
	}
	try {
		if ((await lmStudioModel(base, id))?.state === "loaded") return "";
		const first = loadContext(model.max_context_length);
		status(`Loading ${id} in LM Studio with ${first} tokens of context…`);
		let error = await loadOnce(base, id, first);
		if (error && first > LOAD_CONTEXT_SMALL) {
			status(`Loading ${id} in LM Studio with ${LOAD_CONTEXT_SMALL} tokens of context…`);
			error = await loadOnce(base, id, LOAD_CONTEXT_SMALL);
		}
		return error;
	} finally {
		try {
			unlinkSync(lock);
		} catch {
			// Removed by a waiter that took it for stale.
		}
	}
}

/** Where LM Studio says its server listens, asked of its own `lms` command. */
function lmStudioPort() {
	return new Promise((resolve) => {
		let out = "";
		let done = false;
		const finish = (port) => {
			if (!done) {
				done = true;
				resolve(port);
			}
		};
		try {
			const lms = join(homedir(), ".lmstudio", "bin", process.platform === "win32" ? "lms.exe" : "lms");
			const child = spawn(lms, ["server", "status", "--json"], {
				stdio: ["ignore", "pipe", "ignore"],
				windowsHide: true,
			});
			const timer = setTimeout(() => {
				child.kill();
				finish(undefined);
			}, MODELS_LIMIT_MS);
			child.stdout.on("data", (chunk) => {
				out += chunk;
			});
			child.on("error", () => {
				clearTimeout(timer);
				finish(undefined);
			});
			child.on("close", () => {
				clearTimeout(timer);
				try {
					const status = JSON.parse(out.trim().split("\n").pop());
					finish(status.running && Number.isInteger(status.port) ? status.port : undefined);
				} catch {
					finish(undefined);
				}
			});
		} catch {
			finish(undefined);
		}
	});
}

/**
 * The server to use and its models now. The address set in Connecto comes
 * first. When nothing answers there and it is a local address, LM Studio is
 * asked which port it really listens on (it moves off 1234 when that is taken).
 */
async function findLmStudio(configured) {
	try {
		return { base: configured, models: await lmStudioModels(configured, AbortSignal.timeout(MODELS_LIMIT_MS)) };
	} catch {
		// Not there.
	}
	let host;
	try {
		host = new URL(configured).hostname;
	} catch {
		return { base: configured, models: [] };
	}
	if (!["localhost", "127.0.0.1", "[::1]"].includes(host)) return { base: configured, models: [] };
	const port = await lmStudioPort();
	if (port === undefined) return { base: configured, models: [] };
	const base = `http://${host}:${port}`;
	if (base === configured) return { base, models: [] };
	try {
		return { base, models: await lmStudioModels(base, AbortSignal.timeout(MODELS_LIMIT_MS)) };
	} catch {
		return { base: configured, models: [] };
	}
}

async function registerLmStudio(pi) {
	const configured = String(CONFIG.lmstudio).replace(/\/+$/, "").replace(/\/v1$/, "");
	const describe = (model) => ({
		id: model.id,
		name: model.id,
		reasoning: false,
		input: model.images ? ["text", "image"] : ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: Number.isFinite(model.window) && model.window > 0 ? model.window : LOAD_CONTEXT,
		maxTokens: 8192,
	});
	// Asked once before Pi goes on, so the models are there when Pi reads
	// `--model lmstudio/<id>` at startup; a server that is not running costs
	// the time limit and no more. Later changes come through `refreshModels`.
	const found = await findLmStudio(configured);
	const base = found.base;
	const models = found.models.map(describe);
	pi.registerProvider("lmstudio", {
		name: "LM Studio",
		baseUrl: `${base}/v1`,
		apiKey: "lm-studio",
		api: "openai-completions",
		models,
		async refreshModels({ signal }) {
			try {
				const limit = AbortSignal.any([signal, AbortSignal.timeout(MODELS_LIMIT_MS)]);
				return (await lmStudioModels(base, limit)).map(describe);
			} catch {
				// The server is not running: no local models for now.
				return [];
			}
		},
	});
	if (CONFIG.silent) return;
	// Right before each call to a local model, the model is loaded if it is
	// not: LM Studio's own load on demand sizes the context from free memory
	// and can pick one Pi does not fit in.
	pi.on("before_provider_request", async (_event, ctx) => {
		const model = ctx.model;
		if (model?.provider !== "lmstudio" || typeof model.id !== "string") return undefined;
		const status = (text) => {
			try {
				ctx.ui?.setStatus?.("connecto-lmstudio", text);
			} catch {
				// No UI.
			}
		};
		try {
			const error = await ensureLoaded(base, model.id, status);
			if (error) {
				ctx.ui?.notify?.(
					`LM Studio could not load ${model.id}: ${error}. Free memory by unloading other models, or pick a smaller one with /model.`,
					"error",
				);
			}
		} catch {
			// Pi's own call says what is wrong.
		} finally {
			status(undefined);
		}
		return undefined;
	});
}

export default async function connecto(pi) {
	if (CONFIG.lmstudio) {
		try {
			await registerLmStudio(pi);
		} catch {
			// Pi goes on without the local provider.
		}
	}

	// A run that only asks Pi for its models has nothing to report.
	if (CONFIG.silent) return;

	// The bridge text, then the package recap, as sections of the prompt Pi
	// resolved itself: its APPEND_SYSTEM.md and the project's context stay as
	// they are (`--append-system-prompt` would replace that APPEND_SYSTEM.md).
	// Set before every run, a resumed session's included: Pi compares the
	// sections with the ones the transcript has and sends what changed.
	// A Pi without prompt sections (0.85 hands the handler the prompt it
	// resolved, APPEND_SYSTEM.md included, and takes a changed one back) gets
	// them added at the end of that prompt, in the same tags Pi 1.5 draws
	// sections in. Pi starts from its own prompt again before every run.
	const added = [
		[BRIDGE_SECTION, CONFIG.bridge],
		[RECAP_SECTION, CONFIG.recap],
	].filter(([, text]) => typeof text === "string" && text);
	if (added.length > 0) {
		pi.on("before_agent_start", async (event) => {
			const sections = event.systemPromptOptions?.sections;
			if (sections && typeof sections === "object") {
				for (const [name, text] of added) sections[name] = text;
				return undefined;
			}
			if (typeof event.systemPrompt !== "string") return undefined;
			const tagged = added.map(([name, text]) => `<${name}>\n${text}\n</${name}>`);
			return { systemPrompt: [event.systemPrompt, ...tagged].filter(Boolean).join("\n\n") };
		});
	}

	let lastReply;
	let lastFailed = false;

	pi.on("session_start", async (_event, ctx) => {
		await report("SessionStart", session(ctx));
	});

	// Notices Connecto holds for this terminal reach the model with the prompt
	// that starts a run, as they do in the other agents.
	pi.on("before_agent_start", async (event) => {
		if (!CONFIG.inbox || event.messageType !== "user") return undefined;
		// The prompt goes with it: Connecto may tell the model about a
		// project operation that matches it.
		const prompt = typeof event.prompt === "string" ? event.prompt : undefined;
		const notices = inboxText(await helper("Inbox", prompt === undefined ? {} : { prompt }, true));
		if (!notices) return undefined;
		// For the model, as in the other agents: not drawn in the transcript.
		return { message: { customType: "connecto", content: notices, display: false } };
	});

	// A prompt counts when it enters the conversation: at once for one typed at
	// the prompt, later for one queued while a turn ran. One taken back to the
	// editor never does.
	pi.on("message_start", async (event, ctx) => {
		if (event.message?.role !== "user") return;
		await report("UserPromptSubmit", { ...session(ctx), prompt: textOf(event.message.content) });
	});

	pi.on("agent_end", async (event) => {
		const messages = Array.isArray(event.messages) ? event.messages : [];
		for (let index = messages.length - 1; index >= 0; index--) {
			if (messages[index]?.role === "assistant") {
				lastReply = textOf(messages[index].content);
				// The provider refused or could not be reached: no reply.
				lastFailed = messages[index].stopReason === "error";
				return;
			}
		}
	});

	// Settled is the end Pi will not continue from by itself (a retry, a
	// compaction or a queued message would have). An aborted run reports no
	// end here: Connecto reads the interrupt from the session file.
	pi.on("agent_settled", async (event, ctx) => {
		const reply = lastReply;
		const failed = lastFailed;
		lastReply = undefined;
		lastFailed = false;
		if (event.aborted) return;
		if (failed) {
			await report("StopFailure", { ...session(ctx), error: "provider_error" });
			return;
		}
		const payload = session(ctx);
		if (reply) payload.last_assistant_message = reply;
		await report("Stop", payload);
	});

	pi.on("ui_prompt_start", async () => {
		await report("Notification", { notification_type: "ui_prompt" });
	});
}
