import type { NewRequestId } from "./home-actions";
import { NAMED_KEY_BYTES, keyInput, type NamedKey } from "./keys";
import type {
  ChatAnswerKind,
  ChatOption,
  ChatPrompt,
  ChatPromptKind,
  ChatScreen,
  ClientMessage,
  DaemonMessage,
} from "./protocol";

/** The label an `AskUserQuestion` prompt gives its own-text option. */
const TYPE_SOMETHING = "Type something";

/** Which option of a plan prompt takes the user's own words instead of picking. */
const PLAN_TYPED_INDEX = 2;

/** One prompt the sheet answers, and the answer still out for it. */
export interface PendingAnswer {
  readonly promptId: string;
  readonly requestId: string;
}

/** A refusal of an answer: the prompt it answered, and the daemon's message verbatim. */
export interface SheetError {
  readonly promptId: string;
  readonly text: string;
}

/**
 * Where the sheet stands: the prompt id a swipe put away, the answer still
 * out, and the daemon's refusal of the last answer.
 */
export interface SheetState {
  /** The dismissed prompt's id, while it is the prompt up; null otherwise. */
  readonly dismissed: string | null;
  /** The answer still out; a second answer for its prompt sends nothing. */
  readonly pending: PendingAnswer | null;
  /** The refusal of the last answer, while its prompt is up; null otherwise. */
  readonly error: SheetError | null;
}

/** The state before any swipe or answer: nothing put away, nothing out. */
export const NO_SHEET: SheetState = { dismissed: null, pending: null, error: null };

/** The answer a text button sends once the sheet's box holds the user's words. */
export type TypedAnswer = "type_something" | "allow_with_text" | "deny_with_text";

/** What pressing a sheet button sends. */
export type SheetAction =
  /** This answer as it stands. */
  | { readonly kind: "answer"; readonly answer: ChatAnswerKind }
  /** The text the sheet's box holds, as one of the answers that carry it. */
  | { readonly kind: "text"; readonly as: TypedAnswer };

/** One button of the sheet: an option's number, its label and what it sends. */
export interface SheetButton {
  /** The option's number; null for an action button such as Submit, Back or Skip. */
  readonly number: number | null;
  readonly label: string;
  /** Whether a multi-select option is ticked. */
  readonly selected: boolean;
  readonly action: SheetAction;
}

/** The bottom sheet a prompt raises over the chat and composer. */
export interface Sheet {
  /** The prompt's id, the one an answer carries. */
  readonly id: string;
  readonly kind: ChatPromptKind;
  /** The prompt's subject, or its body when it has none. */
  readonly title: string;
  readonly buttons: readonly SheetButton[];
}

/** What an answer produces: the message to send, if any, and the sheet after it. */
export interface Answered {
  /** The message to send; null while an answer for the prompt is still out. */
  readonly message: ClientMessage | null;
  readonly state: SheetState;
}

/** The Esc, ↑ and ↓ keys the sheet offers, in the order it shows them. */
export const SHEET_KEYS = [
  { key: "escape", label: "Esc" },
  { key: "up", label: "↑" },
  { key: "down", label: "↓" },
] as const;

/** A key {@link SHEET_KEYS} names. */
export type SheetKey = (typeof SHEET_KEYS)[number]["key"];

const SHEET_KEY_BYTES: Record<SheetKey, NamedKey> = {
  escape: "Escape",
  up: "Up",
  down: "Down",
};

/**
 * The sheet the prompt up on `screen` raises, or null when no prompt is up:
 * its title, its kind and one button per option, in the prompt's own order,
 * plus the action buttons its kind offers.
 */
export function sheetFor(screen: ChatScreen | null): Sheet | null {
  const prompt = screen?.prompt ?? null;
  if (prompt === null) {
    return null;
  }
  return {
    id: prompt.id,
    kind: prompt.kind,
    title: prompt.subject.trim() === "" ? prompt.body : prompt.subject,
    buttons: buttonsFor(prompt),
  };
}

/**
 * Where the sheet stands after `screen` shows: a swipe put away the prompt
 * with `state.dismissed`'s id, and a different prompt id draws the sheet
 * again. Pure; a screen the state already fits returns it.
 */
export function swipe(state: SheetState, screen: ChatScreen | null): SheetState {
  const id = screen?.prompt?.id ?? null;
  if (id === null || id === state.dismissed) {
    return state;
  }
  return { ...state, dismissed: id };
}

/** Whether the sheet for `screen` is drawn: a prompt is up and was not swiped away. */
export function isVisible(state: SheetState, screen: ChatScreen | null): boolean {
  const id = screen?.prompt?.id ?? null;
  return id !== null && id !== state.dismissed;
}

/**
 * Where the sheet stands after `screen` shows: an answer still out and a
 * refusal belong to the prompt they answered, so a different prompt id clears
 * both. Pure; a screen the state already fits returns it.
 */
export function syncSheet(state: SheetState, screen: ChatScreen | null): SheetState {
  const id = screen?.prompt?.id ?? null;
  const pendingGone = state.pending !== null && state.pending.promptId !== id;
  const errorGone = state.error !== null && state.error.promptId !== id;
  if (!pendingGone && !errorGone) {
    return state;
  }
  return {
    ...state,
    pending: pendingGone ? null : state.pending,
    error: errorGone ? null : state.error,
  };
}

/**
 * The answer `action` sends with `text` in the sheet's box: null for a text
 * action while the text is blank, since the daemon takes no empty answer.
 */
export function resolveAction(action: SheetAction, text: string): ChatAnswerKind | null {
  if (action.kind === "answer") {
    return action.answer;
  }
  if (text.trim() === "") {
    return null;
  }
  switch (action.as) {
    case "type_something":
      return { kind: "type_something", text };
    case "allow_with_text":
      return { kind: "allow_with_text", text };
    case "deny_with_text":
      return { kind: "deny_with_text", text };
  }
}

/**
 * Answer `prompt` with `answer` under a fresh request id: the `chat_answer`
 * carrying the prompt's id. A second answer waits while one for the same
 * prompt is still out, since the daemon has no success reply to settle it.
 */
export function answerFor(
  state: SheetState,
  sessionId: string,
  prompt: ChatPrompt,
  answer: ChatAnswerKind,
  newRequestId: NewRequestId,
): Answered {
  if (state.pending !== null && state.pending.promptId === prompt.id) {
    return { message: null, state };
  }
  const requestId = newRequestId();
  return {
    message: {
      type: "chat_answer",
      session_id: sessionId,
      prompt_id: prompt.id,
      answer,
      request_id: requestId,
    },
    state: { ...state, pending: { promptId: prompt.id, requestId }, error: null },
  };
}

/**
 * One daemon message folded into the sheet's answer: a refusal echoing the
 * answer's request id ends it and keeps the daemon's text verbatim, for its
 * prompt. Pure; a message the sheet does not read returns the state itself.
 */
export function foldReply(state: SheetState, message: DaemonMessage): SheetState {
  if (message.type !== "error" || state.pending === null) {
    return state;
  }
  if ((message.request_id ?? null) !== state.pending.requestId) {
    return state;
  }
  return {
    ...state,
    pending: null,
    error: { promptId: state.pending.promptId, text: message.message },
  };
}

/** The `send_input` the sheet's `key` types into `sessionId`. */
export function sheetKey(sessionId: string, key: SheetKey): ClientMessage {
  return keyInput(sessionId, NAMED_KEY_BYTES[SHEET_KEY_BYTES[key]]);
}

/** The buttons `prompt`'s kind offers, in the order the sheet draws them. */
function buttonsFor(prompt: ChatPrompt): SheetButton[] {
  switch (prompt.kind) {
    case "permission":
      return permissionButtons(prompt);
    case "question":
      return questionButtons(prompt);
    case "plan":
      return planButtons(prompt);
    case "review":
    case "trust":
    case "heads_up":
      return (prompt.options ?? []).map((choice) =>
        optionButton(choice, optionAction(choice.number)),
      );
    default:
      return [];
  }
}

/** Allow once, Always allow (when offered) and Deny, then the answers that carry words. */
function permissionButtons(prompt: ChatPrompt): SheetButton[] {
  const buttons = (prompt.options ?? []).map((choice) =>
    optionButton(choice, optionAction(choice.number)),
  );
  if (prompt.can_amend === true) {
    buttons.push(textButton("Allow with text", "allow_with_text"));
  }
  buttons.push(textButton("Deny with text", "deny_with_text"));
  return buttons;
}

/** A plan's pickup buttons, its "tell the agent what to change" taking the user's words. */
function planButtons(prompt: ChatPrompt): SheetButton[] {
  return (prompt.options ?? []).map((choice, index) =>
    index === PLAN_TYPED_INDEX
      ? optionButton(choice, { kind: "text", as: "type_something" })
      : optionButton(choice, optionAction(choice.number)),
  );
}

/**
 * A question's options, picked or toggled on a multi-select, its own-text
 * option, Back and Skip with several questions, and Submit on a multi-select
 * (Next before the last question).
 */
function questionButtons(prompt: ChatPrompt): SheetButton[] {
  const buttons = (prompt.options ?? []).map((choice) => {
    if (choice.label === TYPE_SOMETHING) {
      return optionButton(choice, { kind: "text", as: "type_something" });
    }
    const answer: ChatAnswerKind =
      prompt.multi === true
        ? { kind: "toggle", number: choice.number }
        : { kind: "option", number: choice.number };
    return optionButton(choice, { kind: "answer", answer });
  });
  const questions = prompt.questions ?? [];
  if (questions.length > 1) {
    buttons.push(namedButton("Back", { kind: "back" }));
    buttons.push(namedButton("Skip", { kind: "skip" }));
  }
  if (prompt.multi === true) {
    const last = questions.at(-1);
    const label = last === undefined || last.current === true ? "Submit" : "Next";
    buttons.push(namedButton(label, { kind: "submit" }));
  }
  return buttons;
}

/** One button for `choice`, labelled and ticked as the prompt has it. */
function optionButton(choice: ChatOption, action: SheetAction): SheetButton {
  return {
    number: choice.number,
    label: choice.label,
    selected: choice.selected === true,
    action,
  };
}

/** The action that picks the numbered option. */
function optionAction(number: number): SheetAction {
  return { kind: "answer", answer: { kind: "option", number } };
}

/** An action button with no option behind it, such as Submit, Back or Skip. */
function namedButton(label: string, answer: ChatAnswerKind): SheetButton {
  return { number: null, label, selected: false, action: { kind: "answer", answer } };
}

/** A button whose answer is the words the sheet's box holds. */
function textButton(label: string, as: TypedAnswer): SheetButton {
  return { number: null, label, selected: false, action: { kind: "text", as } };
}
