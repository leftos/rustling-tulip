import type { ChatState } from "./chat-model";
import type { ComposerModel } from "./composer";
import { connectionBar } from "./connection-bar";
import type { ConnectionStateName } from "./connection-machine";
import { displayLabel } from "./home-screen";
import type { HomeModel } from "./home-model";
import { KEY_TRAY, type KeyTrayEntry } from "./keys";
import { parseMarkdown, type MdBlock } from "./markdown";
import {
  SHEET_KEYS,
  isVisible,
  sheetFor,
  type SheetAction,
  type SheetButton,
  type SheetState,
} from "./prompt-sheet";
import { quickRepliesFor, type QuickReplyGroup } from "./quick";
import type { ChatEntry, ChatScreen, SessionMode } from "./protocol";

/**
 * What the session screen draws: the chat rows, the composer, the dock one of
 * its buttons opens, and the prompt sheet over them. A pure view model over
 * the chat, composer and sheet models, so the texts and the visible parts are
 * testable without a DOM. `session-view.ts` only renders it.
 */

/** Where the page is: Home, or one session's screen. */
export type Route = { readonly kind: "home" } | { readonly kind: "session"; readonly id: string };

/** The text a page opened at a session that is no longer listed shows. */
export const ENDED_TEXT = "This session has ended.";

/** The Back button's label. */
export const BACK = "‹ Back";

/** The composer's button labels. */
export const SEND = "Send";
export const KEYS = "Keys";
export const QUICK = "Quick";

/** The header toggle's labels: it shows the chat's screen as `Terminal`, the terminal's as `Chat`. */
export const TERMINAL = "Terminal";
export const CHAT = "Chat";

/** The key bar's Ctrl button. */
export const CTRL = "Ctrl";

/** The box's placeholder, which names no agent: the session's agent is its own. */
export const MESSAGE_PLACEHOLDER = "Message";

/** The sheet's dismiss button. */
export const HIDE = "Hide";

/** What a send the connection never carried says under the box, or in the sheet. */
export const OFFLINE_NOT_SENT = "Offline: not sent";

/** One row of the chat: a bubble, a tool's collapsed card, or a muted line. */
export interface ChatRow {
  readonly id: string;
  readonly kind: "user" | "answer" | "tool" | "muted";
  /** The row's line; a tool row is its name and summary. */
  readonly text: string;
  /** A tool row's expanded text: its input and what it returned; null when both are empty. */
  readonly detail: string | null;
  /** A user row whose text has not reached the transcript yet. */
  readonly pending: boolean;
  /** An answer still arriving. */
  readonly live: boolean;
  /** An answer row's text as Markdown blocks; absent on every other row. */
  readonly markdown?: readonly MdBlock[];
}

/** The composer as the view paints it. */
export interface ComposerView {
  readonly draft: string;
  readonly placeholder: string;
  readonly send: string;
  /** False while the box is blank or a send is out. */
  readonly sendEnabled: boolean;
  readonly keys: string;
  readonly quick: string;
  /** The daemon's refusal of the last send, verbatim; null when it refused none. */
  readonly error: string | null;
}

/** The bar under the terminal: the Keys tray, the Ctrl button and the Quick dock's button. */
export interface KeyBarView {
  readonly keys: readonly KeyTrayEntry[];
  readonly ctrl: string;
  readonly ctrlArmed: boolean;
  readonly quick: string;
}

/** Which dock the composer has open. */
export type DockKind = "keys" | "quick";

/** The one dock open above the composer. */
export interface DockView {
  readonly kind: DockKind;
  readonly keys: readonly KeyTrayEntry[];
  readonly groups: readonly QuickReplyGroup[];
}

/** One button of the prompt sheet: an option's key-cap, its label and what a tap sends. */
export interface SheetButtonView {
  readonly number: string | null;
  readonly label: string;
  readonly selected: boolean;
  readonly action: SheetAction;
}

/** The prompt sheet over the chat and composer. */
export interface SheetView {
  /** The prompt's id, so the view clears its box when another prompt shows. */
  readonly id: string;
  readonly title: string;
  readonly buttons: readonly SheetButtonView[];
  /** The keys the sheet offers, in order. */
  readonly keys: readonly string[];
  /** Whether the box the answers that carry words type into is shown. */
  readonly typing: boolean;
  /** The daemon's refusal of the last answer, verbatim; null when it refused none. */
  readonly error: string | null;
  readonly hide: string;
}

/** Everything the session screen draws. */
export interface SessionScreen {
  readonly back: string;
  readonly title: string;
  /** The connection bar under the header, as Home draws it; null when the connection is fine. */
  readonly bar: string | null;
  /** The whole screen's text when the session is gone; null otherwise. */
  readonly endText: string | null;
  /** Whether the terminal pane is shown: the toggle is on, or the chat was refused. */
  readonly terminal: boolean;
  /** The header toggle's label; null when there is nothing to toggle. */
  readonly toggle: string | null;
  /** The key bar under the terminal; null when the terminal is not shown. */
  readonly keyBar: KeyBarView | null;
  /** `{n} earlier messages`; null when the display cap dropped none. */
  readonly hiddenLine: string | null;
  readonly rows: readonly ChatRow[];
  /** The mirror's refusal, verbatim; null otherwise. */
  readonly notice: string | null;
  /** The Keys shown with a refusal, always open; empty otherwise. */
  readonly keys: readonly KeyTrayEntry[];
  /** The composer; null when the chat was refused or the session ended. */
  readonly composer: ComposerView | null;
  /** The dock one of the composer's buttons opens; null when none is. */
  readonly dock: DockView | null;
  readonly sheet: SheetView | null;
}

/** What the screen is drawn from. */
export interface SessionScreenInput {
  readonly title: string;
  /** The session is gone from Home's list, so nothing of it is drawn. */
  readonly ended: boolean;
  /** The session runs an agent, which picks the Quick replies' groups. */
  readonly isAgent: boolean;
  readonly chat: ChatState;
  readonly composer: ComposerModel;
  readonly sheet: SheetState;
  readonly dock: DockKind | null;
  /** `Offline: not sent` for a dropped Key or Quick tap; null otherwise. */
  readonly offline: string | null;
  /** The user's Terminal toggle is on, so the terminal replaces the chat. */
  readonly terminal: boolean;
  /** The terminal's Ctrl is armed for the next key. */
  readonly ctrlArmed: boolean;
  /** The chat mirror refused this session once while the screen was mounted. */
  readonly mirrorRefused: boolean;
  readonly status: ConnectionStateName;
  /** A send has been refused since the last `ready`. */
  readonly sendRefused: boolean;
  /** The page has had a ready, paired connection in this visit (the latch Home shares). */
  readonly connectedOnce: boolean;
  /** The view's parse cache for answer rows, which each build refreshes to the chat's answers. */
  readonly answers: AnswerCache;
}

/** What the screen knows of its session from Home's model. */
export interface SessionLook {
  /** The session's display name; empty while Home does not list it. */
  readonly title: string;
  /** The session runs an agent, which picks the Quick replies' groups. */
  readonly isAgent: boolean;
  /** Home's model has loaded and no longer lists the session. */
  readonly ended: boolean;
}

/**
 * The route `pathname` names: `/s/{id}` for one session, Home for anything
 * else, a trailing segment included.
 */
export function parseRoute(pathname: string): Route {
  const match = /^\/s\/([^/]+)$/.exec(pathname);
  const raw = match?.[1];
  if (raw === undefined) {
    return { kind: "home" };
  }
  try {
    return { kind: "session", id: decodeURIComponent(raw) };
  } catch {
    // A malformed escape names no session this page can open.
    return { kind: "home" };
  }
}

/**
 * The session's look in Home's model: its name and whether it runs an agent,
 * and whether it is gone once the model has loaded its session list.
 */
export function sessionLook(model: HomeModel, sessionId: string): SessionLook {
  const session = model.sessions.find((known) => known.id === sessionId) ?? null;
  if (session === null) {
    return { title: "", isAgent: false, ended: model.loaded };
  }
  return { title: displayLabel(session), isAgent: session.agent !== null, ended: false };
}

/**
 * Whether a session of `mode` opens on the terminal instead of the chat: only
 * a plain shell does, since an agent typed into a shell still has a chat. A
 * mode this build does not know opens on the chat too.
 */
export function opensOnTerminal(mode: SessionMode): boolean {
  return mode === "plain_shell";
}

/**
 * The Terminal toggle once the session's snapshot arrives: the user's own
 * choice when they already pressed it, else the view its mode opens on.
 */
export function terminalWithSnapshot(choice: boolean | null, mode: SessionMode): boolean {
  return choice ?? opensOnTerminal(mode);
}

/** The session screen for one input. */
export function sessionScreen(input: SessionScreenInput): SessionScreen {
  // Before the first connect nothing is coming back, so a cold deep link shows no bar.
  const bar = connectionBar(input.status, input.connectedOnce, input.sendRefused);
  if (input.ended) {
    return {
      back: BACK,
      title: input.title,
      bar,
      endText: ENDED_TEXT,
      terminal: false,
      toggle: null,
      keyBar: null,
      hiddenLine: null,
      rows: [],
      notice: null,
      keys: [],
      composer: null,
      dock: null,
      sheet: null,
    };
  }
  const chat = input.chat;
  const notice = chat.phase === "refused" ? chat.notice : null;
  // A refusal seen once keeps the terminal as the main view while the chat
  // reloads after a reconnect or a hide.
  const refused = input.mirrorRefused || chat.phase === "refused";
  const terminal = input.terminal || refused;
  const toggle = refused ? null : input.terminal ? CHAT : TERMINAL;
  if (terminal) {
    return {
      back: BACK,
      title: input.title,
      bar,
      endText: null,
      terminal: true,
      toggle,
      keyBar: keyBarView(input.ctrlArmed),
      hiddenLine: null,
      rows: [],
      // No composer draws the offline line here, so it sits above the terminal.
      notice: notice ?? input.offline,
      keys: [],
      composer: null,
      dock: dockView(input.dock === "quick" ? "quick" : null, input.isAgent),
      sheet: null,
    };
  }
  return {
    back: BACK,
    title: input.title,
    bar,
    endText: null,
    terminal: false,
    toggle,
    keyBar: null,
    hiddenLine: chat.hidden > 0 ? `${chat.hidden} earlier messages` : null,
    rows: chatRows(chat.entries, input.answers),
    notice,
    keys: [],
    composer: composerView(input.composer, input.offline),
    dock: dockView(input.dock, input.isAgent),
    sheet: sheetView(input.sheet, chat.screen),
  };
}

/** Whether two screens are equal, so painting one over the other would show the same. */
export function sameScreen(left: SessionScreen, right: SessionScreen): boolean {
  return sameValue(left, right);
}

/** Whether two row lists are equal, so the chat need not be rebuilt. */
export function sameRows(
  left: readonly ChatRow[] | null,
  right: readonly ChatRow[] | null,
): boolean {
  return sameValue(left, right);
}

/** Whether two rows draw the same thing, so the node drawn for one can be reused. */
export function sameRow(left: ChatRow, right: ChatRow): boolean {
  return sameValue(left, right);
}

/** Whether two of the screen's parts are equal, so that part need not be rebuilt. */
export function samePart(left: unknown, right: unknown): boolean {
  return sameValue(left, right);
}

/** An answer's text and the blocks parsed from it. */
export interface ParsedAnswer {
  readonly text: string;
  readonly blocks: readonly MdBlock[];
}

/**
 * Each answer's parsed blocks by entry id, with the text they were parsed
 * from. The live answer repaints on every chunk, so an answer is parsed again
 * only when its text changed; each build keeps only the ids still in the chat.
 */
export type AnswerCache = Map<string, ParsedAnswer>;

/** The chat's entries as rows, refreshing the answers' parse cache to the chat's ids. */
function chatRows(entries: readonly ChatEntry[], answers: AnswerCache): ChatRow[] {
  const kept: AnswerCache = new Map();
  const rows = entries.map((entry) => rowFor(entry, answers, kept));
  answers.clear();
  for (const [id, parsed] of kept) {
    answers.set(id, parsed);
  }
  return rows;
}

/** An answer's blocks, from the cache when its text is unchanged. */
function answerBlocks(
  id: string,
  text: string,
  answers: AnswerCache,
  kept: AnswerCache,
): readonly MdBlock[] {
  const known = kept.get(id) ?? answers.get(id);
  const blocks = known !== undefined && known.text === text ? known.blocks : parseMarkdown(text);
  kept.set(id, { text, blocks });
  return blocks;
}

/** One entry as the row the chat draws for it. */
function rowFor(entry: ChatEntry, answers: AnswerCache, kept: AnswerCache): ChatRow {
  switch (entry.kind) {
    case "user":
      return {
        id: entry.id,
        kind: "user",
        text: entry.text,
        detail: null,
        pending: entry.pending === true,
        live: false,
      };
    case "answer":
      return {
        id: entry.id,
        kind: "answer",
        text: entry.text,
        detail: null,
        pending: false,
        live: entry.live === true,
        markdown: answerBlocks(entry.id, entry.text, answers, kept),
      };
    case "tool":
      return {
        id: entry.id,
        kind: "tool",
        text: [entry.name.trim(), entry.summary.trim()].filter((part) => part !== "").join(" "),
        detail: toolDetail(entry),
        pending: false,
        live: false,
      };
    case "receipt":
      return mutedRow(entry.id, `${entry.question} — ${entry.answer}`);
    case "thinking":
      return mutedRow(entry.id, thinkingLabel(entry.secs ?? null));
    case "system":
      return mutedRow(entry.id, entry.text);
    case "turn_end":
      return mutedRow(entry.id, turnLabel(entry.duration_ms ?? null));
    default:
      // A kind this build does not draw: the daemon knows more than it does.
      return mutedRow(unknownId(entry), unknownKind(entry));
  }
}

/** A row this build draws no special way. */
function mutedRow(id: string, text: string): ChatRow {
  return { id, kind: "muted", text, detail: null, pending: false, live: false };
}

/** A tool row's expanded text: what went in, then what came back. */
function toolDetail(entry: Extract<ChatEntry, { kind: "tool" }>): string | null {
  const parts = [entry.input.trim(), entry.result?.text.trim() ?? ""].filter((part) => part !== "");
  return parts.length === 0 ? null : parts.join("\n\n");
}

/** "Thought for 14s", or "Thought" while the length is unknown, as the desktop draws it. */
function thinkingLabel(secs: number | null): string {
  return secs === null ? "Thought" : `Thought for ${span(secs)}`;
}

/** "Worked for 12s", or "Worked" without a duration, as the desktop draws it. */
function turnLabel(durationMs: number | null): string {
  return durationMs === null ? "Worked" : `Worked for ${span(Math.floor(durationMs / 1000))}`;
}

/** A duration in seconds as the desktop writes it: `45s`, `2m 5s`, `1h 3m`. */
function span(secs: number): string {
  if (secs < 60) {
    return `${secs}s`;
  }
  if (secs < 3600) {
    return `${Math.floor(secs / 60)}m ${secs % 60}s`;
  }
  return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
}

/** The id of a row whose kind this build does not know. */
function unknownId(entry: never): string {
  const raw: unknown = entry;
  return idOf(raw) ?? "";
}

/** The kind name of a row whose kind this build does not know. */
function unknownKind(entry: never): string {
  const raw: unknown = entry;
  if (typeof raw !== "object" || raw === null || !("kind" in raw)) {
    return "unknown";
  }
  const kind: unknown = raw.kind;
  return typeof kind === "string" ? kind : "unknown";
}

function idOf(raw: unknown): string | null {
  if (typeof raw !== "object" || raw === null || !("id" in raw)) {
    return null;
  }
  const id: unknown = raw.id;
  return typeof id === "string" ? id : null;
}

/**
 * The composer as the view paints it; Send waits while a send is out or the
 * box is blank. A dropped Key or Quick tap shows in the same line as a
 * refusal, which wins while it stands.
 */
function composerView(state: ComposerModel, offline: string | null): ComposerView {
  return {
    draft: state.draft,
    placeholder: MESSAGE_PLACEHOLDER,
    send: SEND,
    sendEnabled: state.sending === null && state.draft.trim() !== "",
    keys: KEYS,
    quick: QUICK,
    error: state.error ?? offline,
  };
}

/** The key bar under the terminal, with `ctrlArmed` on its Ctrl button. */
function keyBarView(ctrlArmed: boolean): KeyBarView {
  return { keys: KEY_TRAY, ctrl: CTRL, ctrlArmed, quick: QUICK };
}

/** The dock `kind` opens, with the entries it holds; null when none is open. */
function dockView(kind: DockKind | null, isAgent: boolean): DockView | null {
  switch (kind) {
    case "keys":
      return { kind: "keys", keys: KEY_TRAY, groups: [] };
    case "quick":
      return { kind: "quick", keys: [], groups: quickRepliesFor(isAgent) };
    default:
      return null;
  }
}

/** The sheet the prompt up on `look` raises; null when no prompt is up or it was put away. */
function sheetView(state: SheetState, look: ChatScreen | null): SheetView | null {
  if (!isVisible(state, look)) {
    return null;
  }
  const sheet = sheetFor(look);
  if (sheet === null) {
    return null;
  }
  return {
    id: sheet.id,
    title: sheet.title,
    buttons: sheet.buttons.map(buttonView),
    keys: SHEET_KEYS.map((key) => key.label),
    typing: sheet.buttons.some((button) => button.action.kind === "text"),
    error: state.error !== null && state.error.promptId === sheet.id ? state.error.text : null,
    hide: HIDE,
  };
}

function buttonView(button: SheetButton): SheetButtonView {
  return {
    number: button.number === null ? null : String(button.number),
    label: button.label,
    selected: button.selected,
    action: button.action,
  };
}

/** Structural equality over the plain data a screen is made of. */
function sameValue(left: unknown, right: unknown): boolean {
  if (left === right) {
    return true;
  }
  if (Array.isArray(left) || Array.isArray(right)) {
    if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) {
      return false;
    }
    return left.every((item, at) => sameValue(item, right[at]));
  }
  if (typeof left !== "object" || typeof right !== "object" || left === null || right === null) {
    return false;
  }
  const a = left as Readonly<Record<string, unknown>>;
  const b = right as Readonly<Record<string, unknown>>;
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) {
    return false;
  }
  return keys.every((key) => key in b && sameValue(a[key], b[key]));
}
