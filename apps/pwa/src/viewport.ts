/**
 * Where the session screen sits while the on-screen keyboard is open. iOS
 * Safari ignores the viewport meta's `interactive-widget=resizes-content`, so
 * the keyboard leaves the layout viewport as it was and pans it instead, and a
 * box fixed to the layout viewport slides its top off the screen. The visual
 * viewport is the part the user sees; the session screen is sized to it.
 */

/** What the session screen reads of `window.visualViewport`. */
export interface VisualViewportLike {
  readonly height: number;
  /** How far the visual viewport sits below the layout viewport's top. */
  readonly offsetTop: number;
}

/** The session screen's top and height, in whole CSS pixels, and whether the keyboard is up. */
export interface SessionBox {
  readonly top: number;
  readonly height: number;
  /**
   * The visual viewport is shorter than the layout viewport, so the column
   * ends at the keyboard and the home indicator's inset does not apply.
   */
  readonly keyboardOpen: boolean;
}

/**
 * The box that covers the visual viewport: its top at the visual viewport's
 * offset and its height down to the keyboard. Both edges round to the nearest
 * pixel and the height is the space between them, so the box never leaves a
 * gap at its foot or runs under the keyboard by a fraction. `layoutHeight` is
 * `innerHeight`; a visual viewport more than 1px shorter than it reads as the
 * keyboard being open, however far iOS has panned it.
 */
export function sessionBox(viewport: VisualViewportLike, layoutHeight: number): SessionBox {
  const top = Math.round(viewport.offsetTop);
  const end = viewport.offsetTop + viewport.height;
  const bottom = Math.round(end);
  return { top, height: bottom - top, keyboardOpen: viewport.height < layoutHeight - 1 };
}
