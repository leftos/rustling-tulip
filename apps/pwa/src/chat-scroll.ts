/** A scroll box's position, measured where the chat's rows are drawn. */
export interface ChatScroll {
  readonly top: number;
  readonly atBottom: boolean;
}

/** How close to the bottom counts as at the bottom, in pixels. */
export const AT_BOTTOM_PX = 8;

/** Measures a scroll box's position and whether it sits at its bottom. */
export function measureScroll(el: {
  readonly scrollTop: number;
  readonly scrollHeight: number;
  readonly clientHeight: number;
}): ChatScroll {
  return {
    top: el.scrollTop,
    atBottom: el.scrollHeight - el.scrollTop - el.clientHeight < AT_BOTTOM_PX,
  };
}

/**
 * Where a rebuilt chat scrolls to: `kept` is the position saved when the chat
 * hid, `before` the one measured just before the rows were replaced, and
 * `scrollHeight` the rebuilt box's height. Null leaves the scroll alone.
 */
export function scrollAfterPaint(
  kept: ChatScroll | null,
  before: ChatScroll,
  scrollHeight: number,
): number | null {
  if (kept !== null) {
    return kept.atBottom ? scrollHeight : kept.top;
  }
  return before.atBottom ? scrollHeight : null;
}

/**
 * Where a chat scrolls to once its box may have changed height (a dock, the
 * key bar, the composer or the keyboard came or went): `wasAtBottom` and
 * `beforeHeight` are whether it sat at its bottom and its `clientHeight` just
 * before. A chat at the bottom whose box changed height stays at the bottom;
 * null, for no write, when it was scrolled up or its box kept its height.
 */
export function scrollAfterResize(
  wasAtBottom: boolean,
  beforeHeight: number,
  scrollHeight: number,
  clientHeight: number,
): number | null {
  if (!wasAtBottom || clientHeight === beforeHeight) {
    return null;
  }
  return Math.max(0, scrollHeight - clientHeight);
}
