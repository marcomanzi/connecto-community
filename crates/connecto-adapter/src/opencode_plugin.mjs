// Connecto's plugin for OpenCode. Connecto writes this file into the
// terminal's run folder and names it in `OPENCODE_CONFIG_CONTENT`, so it is
// loaded for that terminal only, beside the user's own plugins. A line
// `const CONFIG = {...};` is put in front of it at launch:
//
//   hook      absolute path of Connecto's hook helper
//   inbox     whether notices from other terminals are handed to the model
//   session   the OpenCode session the terminal resumes, or null
//   shellEnv  variables the agent's shell commands get back (an isolated
//             terminal moves OpenCode's folders with the XDG variables, and
//             the commands it runs must see the user's own), or null
//
// It tells Connecto when OpenCode is up, a prompt is accepted, a turn ends
// and OpenCode waits for the user, by running the helper with the same
// payloads Claude Code's hooks send.
//
// Nothing here may disturb OpenCode: every failure is swallowed.

import { spawn } from "node:child_process";

const HELPER_LIMIT_MS = 8000;

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
			const args = ["opencode", "--event", event];
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

/** The text the user wrote in a message's parts: not what a plugin added. */
function promptOf(parts) {
	return (Array.isArray(parts) ? parts : [])
		.filter((part) => part && part.type === "text" && !part.synthetic && typeof part.text === "string")
		.map((part) => part.text)
		.join("\n");
}

/** A short word for why a turn failed, as Claude's `StopFailure` names it. */
function failureWord(error) {
	if (error?.name === "MessageAbortedError") return "interrupted";
	const name = typeof error?.name === "string" ? error.name : "";
	return /^[A-Za-z0-9_-]{1,64}$/.test(name) ? name : "unknown";
}

export const ConnectoPlugin = async () => {
	if (!CONFIG.hook) return {};

	// Sessions a subagent (the task tool) runs in: they have a parent, and
	// their turns are not the terminal's.
	const children = new Set();
	// The session id Connecto was last told, and the one the running turn is in.
	let told = typeof CONFIG.session === "string" ? CONFIG.session : "";
	let current;
	let busy = false;
	// The newest assistant message of the turn, its text parts, and how it failed.
	let lastAssistant;
	let failure;
	const texts = new Map();

	// OpenCode makes a session only when the first prompt is sent, so it is
	// up before there is one: the terminal is ready now, and is told the id
	// with the prompt that made it (`SessionNamed`, which only records it).
	await report("SessionStart", { session_id: told });

	async function settle(sessionID) {
		if (!busy || sessionID !== current) return;
		busy = false;
		const parts = texts.get(lastAssistant);
		const reply = parts ? [...parts.values()].join("") : "";
		const failed = failure;
		lastAssistant = undefined;
		failure = undefined;
		texts.clear();
		if (failed) {
			await report("StopFailure", { session_id: current, error: failureWord(failed) });
			return;
		}
		const payload = { session_id: current };
		if (reply) payload.last_assistant_message = reply;
		await report("Stop", payload);
	}

	return {
		// A prompt counts when OpenCode takes it into the conversation.
		"chat.message": async (input, output) => {
			const sessionID = input?.sessionID;
			if (!sessionID || children.has(sessionID)) return;
			if (!busy) {
				lastAssistant = undefined;
				failure = undefined;
				texts.clear();
			}
			current = sessionID;
			busy = true;
			// A new session (the first prompt, or one after `/new`) is named at
			// once, so a restart in the middle of this turn can resume it.
			if (sessionID !== told) {
				told = sessionID;
				await report("SessionNamed", { session_id: sessionID });
			}
			const prompt = promptOf(output?.parts);
			await report("UserPromptSubmit", { session_id: sessionID, prompt });
			if (!CONFIG.inbox) return;
			// Notices Connecto holds for this terminal reach the model with the
			// prompt, as they do in the other agents: as a part OpenCode does
			// not draw. The prompt goes with the ask: Connecto may tell the
			// model about a project operation that matches it.
			const notices = inboxText(await helper("Inbox", { prompt }, true));
			const parts = output?.parts;
			const last = Array.isArray(parts) ? parts[parts.length - 1] : undefined;
			if (!notices || !last?.id) return;
			parts.push({
				id: `${last.id}c`,
				sessionID: last.sessionID,
				messageID: last.messageID,
				type: "text",
				text: notices,
				synthetic: true,
			});
		},

		event: async ({ event }) => {
			const type = event?.type;
			const props = event?.properties ?? {};
			switch (type) {
				case "session.created":
					if (props.info?.parentID && props.info?.id) children.add(props.info.id);
					return;
				case "message.updated": {
					const info = props.info;
					if (info?.role !== "assistant" || info.sessionID !== current) return;
					lastAssistant = info.id;
					if (info.error) failure = info.error;
					return;
				}
				case "message.part.updated": {
					const part = props.part;
					if (part?.type !== "text" || part.synthetic || part.sessionID !== current) return;
					let parts = texts.get(part.messageID);
					if (!parts) {
						parts = new Map();
						texts.set(part.messageID, parts);
					}
					parts.set(part.id, String(part.text ?? ""));
					return;
				}
				case "session.error":
					if (!props.sessionID || props.sessionID === current) failure = props.error ?? { name: "unknown" };
					return;
				case "session.status":
					if (props.status?.type === "idle") await settle(props.sessionID);
					return;
				case "session.idle":
					await settle(props.sessionID);
					return;
				// OpenCode asks the user something: a permission, or a question
				// its question tool put. A subagent's ask is the user's too.
				case "permission.asked":
				case "permission.updated":
				case "question.asked":
					await report("Notification", { notification_type: "permission_prompt" });
					return;
				default:
			}
		},

		// The agent's shell commands run with the user's own folders.
		"shell.env": async (_input, output) => {
			if (CONFIG.shellEnv && output?.env) Object.assign(output.env, CONFIG.shellEnv);
		},
	};
};
