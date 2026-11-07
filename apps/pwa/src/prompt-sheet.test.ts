import { expect, test } from "vitest";

import {
  NO_SHEET,
  SHEET_KEYS,
  answerFor,
  foldReply,
  isVisible,
  resolveAction,
  sheetFor,
  sheetKey,
  swipe,
  syncSheet,
  type Sheet,
  type SheetButton,
} from "./prompt-sheet";
import type {
  ChatAnswerKind,
  ChatOption,
  ChatPrompt,
  ChatPromptKind,
  ChatScreen,
  ClientMessage,
} from "./protocol";

const TYPED = "my text";

function option(number: number, label: string, selected = false): ChatOption {
  return { number, label, selected };
}

function prompt(
  kind: ChatPromptKind,
  options: readonly ChatOption[] = [],
  extra: Partial<ChatPrompt> = {},
): ChatPrompt {
  return {
    id: "p1",
    kind,
    subject: "The subject",
    body: "The body",
    options: [...options],
    ...extra,
  };
}

function screen(p: ChatPrompt | null): ChatScreen {
  return { composer: { state: "ready" }, prompt: p };
}

function buttons(sheet: Sheet | null): readonly SheetButton[] {
  if (sheet === null) {
    throw new Error("expected a sheet");
  }
  return sheet.buttons;
}

/** The answer each button sends when the sheet's box holds {@link TYPED}. */
function answers(sheet: Sheet | null): readonly ChatAnswerKind[] {
  return buttons(sheet).map((button) => {
    const answer = resolveAction(button.action, TYPED);
    if (answer === null) {
      throw new Error(`button ${button.label} sent nothing`);
    }
    return answer;
  });
}

/** A generator giving `req-1`, `req-2`, … in the order it is called. */
function counter(): () => string {
  let next = 0;
  return () => {
    next += 1;
    return `req-${next}`;
  };
}

/** A generator no answer may call: it sends nothing. */
function unused(): () => string {
  return () => {
    throw new Error("a request id was generated for an answer that sends nothing");
  };
}

/** The text a `send_input` message types. */
function typedBytes(message: ClientMessage): string {
  if (message.type !== "send_input") {
    throw new Error(`expected send_input, saw ${message.type}`);
  }
  const binary = atob(message.data_b64);
  return new TextDecoder().decode(Uint8Array.from(binary, (c) => c.charCodeAt(0)));
}

test("sheet_no_prompt_draws_nothing", () => {
  expect(sheetFor(null)).toBeNull();
  expect(sheetFor(screen(null))).toBeNull();
});

test("sheet_titles_from_the_subject_and_falls_back_to_the_body", () => {
  expect(sheetFor(screen(prompt("trust", [option(1, "Trust")])))?.title).toBe("The subject");

  const untitled = prompt("trust", [option(1, "Trust")], { subject: "  " });
  expect(sheetFor(screen(untitled))?.title).toBe("The body");
});

test("sheet_permission_offers_each_option_and_the_text_answers", () => {
  const p = prompt(
    "permission",
    [option(1, "Allow once"), option(2, "Always allow"), option(3, "Deny")],
    { can_amend: true },
  );
  const sheet = sheetFor(screen(p));

  expect(sheet?.id).toBe("p1");
  expect(sheet?.kind).toBe("permission");
  expect(buttons(sheet).map((button) => [button.number, button.label, button.selected])).toEqual([
    [1, "Allow once", false],
    [2, "Always allow", false],
    [3, "Deny", false],
    [null, "Allow with text", false],
    [null, "Deny with text", false],
  ]);
  expect(answers(sheet)).toEqual([
    { kind: "option", number: 1 },
    { kind: "option", number: 2 },
    { kind: "option", number: 3 },
    { kind: "allow_with_text", text: TYPED },
    { kind: "deny_with_text", text: TYPED },
  ]);
});

test("sheet_permission_without_an_amendment_offers_the_denial_text_only", () => {
  const p = prompt("permission", [option(1, "Allow once"), option(2, "Deny")]);
  const sheet = sheetFor(screen(p));

  expect(buttons(sheet).map((button) => button.label)).toEqual([
    "Allow once",
    "Deny",
    "Deny with text",
  ]);
});

test("sheet_question_picks_one_option_and_types_its_own_text", () => {
  const p = prompt("question", [
    option(1, "Sub"),
    option(2, "Div", true),
    option(3, "Type something"),
  ]);
  const sheet = sheetFor(screen(p));

  expect(buttons(sheet).map((button) => [button.number, button.selected])).toEqual([
    [1, false],
    [2, true],
    [3, false],
  ]);
  expect(answers(sheet)).toEqual([
    { kind: "option", number: 1 },
    { kind: "option", number: 2 },
    { kind: "type_something", text: TYPED },
  ]);
});

test("sheet_question_multi_select_toggles_and_submits", () => {
  const p = prompt("question", [option(1, "Sub", true), option(2, "Div")], {
    multi: true,
    questions: [
      { label: "Q1", answered: true },
      { label: "Q2", current: true },
    ],
  });
  const sheet = sheetFor(screen(p));

  expect(buttons(sheet).map((button) => [button.number, button.label, button.selected])).toEqual([
    [1, "Sub", true],
    [2, "Div", false],
    [null, "Back", false],
    [null, "Skip", false],
    [null, "Submit", false],
  ]);
  expect(answers(sheet)).toEqual([
    { kind: "toggle", number: 1 },
    { kind: "toggle", number: 2 },
    { kind: "back" },
    { kind: "skip" },
    { kind: "submit" },
  ]);
});

test("sheet_question_multi_select_shows_next_before_the_last_question", () => {
  const p = prompt("question", [option(1, "Sub")], {
    multi: true,
    questions: [{ label: "Q1", current: true }, { label: "Q2" }],
  });

  expect(buttons(sheetFor(screen(p))).map((button) => button.label)).toEqual([
    "Sub",
    "Back",
    "Skip",
    "Next",
  ]);
});

test("sheet_question_with_one_question_offers_no_back_or_skip", () => {
  const p = prompt("question", [option(1, "Sub")], {
    multi: true,
    questions: [{ label: "Q1", current: true }],
  });

  expect(buttons(sheetFor(screen(p))).map((button) => button.label)).toEqual(["Sub", "Submit"]);
});

test("sheet_review_picks_submit_or_cancel", () => {
  const p = prompt("review", [option(1, "Submit"), option(2, "Cancel")]);

  expect(answers(sheetFor(screen(p)))).toEqual([
    { kind: "option", number: 1 },
    { kind: "option", number: 2 },
  ]);
});

test("sheet_plan_approves_or_takes_the_change_text", () => {
  const p = prompt("plan", [
    option(1, "Approve (auto)"),
    option(2, "Approve (manual edits)"),
    option(3, "Tell the agent what to change"),
  ]);

  expect(answers(sheetFor(screen(p)))).toEqual([
    { kind: "option", number: 1 },
    { kind: "option", number: 2 },
    { kind: "type_something", text: TYPED },
  ]);
});

test("sheet_trust_picks_an_option", () => {
  const p = prompt("trust", [option(1, "Trust this folder"), option(2, "Exit")]);

  expect(answers(sheetFor(screen(p)))).toEqual([
    { kind: "option", number: 1 },
    { kind: "option", number: 2 },
  ]);
});

test("sheet_heads_up_offers_nothing_to_answer", () => {
  expect(buttons(sheetFor(screen(prompt("heads_up"))))).toEqual([]);
});

test("sheet_action_with_blank_text_sends_nothing", () => {
  const p = prompt("question", [option(1, "Type something")]);
  const [typed] = buttons(sheetFor(screen(p)));
  if (typed === undefined) {
    throw new Error("expected a typed-text button");
  }

  expect(resolveAction(typed.action, "  \n")).toBeNull();
  expect(resolveAction({ kind: "answer", answer: { kind: "skip" } }, "")).toEqual({ kind: "skip" });
});

test("sheet_answer_carries_the_prompt_id_and_the_answer", () => {
  const p = prompt("permission", [option(1, "Allow once")]);
  const result = answerFor(NO_SHEET, "session-1", p, { kind: "option", number: 1 }, counter());

  expect(result.message).toEqual({
    type: "chat_answer",
    session_id: "session-1",
    prompt_id: "p1",
    answer: { kind: "option", number: 1 },
    request_id: "req-1",
  });
  expect(result.state.pending).toEqual({ promptId: "p1", requestId: "req-1" });
});

test("sheet_second_answer_waits_while_one_is_pending", () => {
  const p = prompt("permission", [option(1, "Allow once")]);
  const first = answerFor(NO_SHEET, "session-1", p, { kind: "option", number: 1 }, counter());
  const second = answerFor(first.state, "session-1", p, { kind: "option", number: 1 }, unused());

  expect(second.message).toBeNull();
  expect(second.state).toBe(first.state);
});

test("sheet_refusal_clears_the_answer_and_shows_the_message", () => {
  const p = prompt("permission", [option(1, "Allow once")]);
  const answered = answerFor(NO_SHEET, "session-1", p, { kind: "option", number: 1 }, counter());
  const state = foldReply(answered.state, {
    type: "error",
    request_id: "req-1",
    code: "chat_refused",
    message: "The prompt is no longer up.",
  });

  expect(state.pending).toBeNull();
  expect(state.error).toEqual({ promptId: "p1", text: "The prompt is no longer up." });
});

test("sheet_refusal_for_another_request_is_ignored", () => {
  const p = prompt("permission", [option(1, "Allow once")]);
  const answered = answerFor(NO_SHEET, "session-1", p, { kind: "option", number: 1 }, counter());

  expect(
    foldReply(answered.state, { type: "error", request_id: "req-9", message: "Another failed." }),
  ).toBe(answered.state);
});

test("sheet_new_prompt_clears_the_pending_answer_and_error", () => {
  const p = prompt("permission", [option(1, "Allow once")]);
  const answered = answerFor(NO_SHEET, "session-1", p, { kind: "option", number: 1 }, counter());
  const refused = foldReply(answered.state, {
    type: "error",
    request_id: "req-1",
    message: "The prompt is no longer up.",
  });
  const same = screen({ ...prompt("permission"), id: "p1" });
  const next = screen({ ...prompt("permission"), id: "p2" });

  expect(syncSheet(refused, same)).toBe(refused);
  expect(syncSheet(refused, next)).toEqual({ dismissed: null, pending: null, error: null });
  expect(syncSheet(NO_SHEET, next)).toBe(NO_SHEET);
});

test("sheet_keys_type_escape_up_and_down", () => {
  expect(SHEET_KEYS.map((entry) => entry.label)).toEqual(["Esc", "↑", "↓"]);
  expect(typedBytes(sheetKey("session-1", "escape"))).toBe("\x1b");
  expect(typedBytes(sheetKey("session-1", "up"))).toBe("\x1b[A");
  expect(typedBytes(sheetKey("session-1", "down"))).toBe("\x1b[B");
});

test("sheet_swipe_dismisses_only_its_prompt", () => {
  const first = screen(prompt("permission", [option(1, "Allow once")]));
  const swiped = swipe(NO_SHEET, first);

  expect(swiped.dismissed).toBe("p1");
  expect(isVisible(NO_SHEET, first)).toBe(true);
  expect(isVisible(swiped, first)).toBe(false);
  expect(isVisible(swiped, screen({ ...prompt("permission"), id: "p2" }))).toBe(true);
  expect(isVisible(swiped, null)).toBe(false);
  expect(swipe(NO_SHEET, null)).toBe(NO_SHEET);
});
