/**
 * Whether the page runs from the Home Screen, and what that changes on the
 * page's shell. The push panel and the intro line both ask the one check.
 */

/** Whether the page runs from the Home Screen: iOS reports `standalone`, other browsers the display mode. */
export function isInstalled(where: Navigator): boolean {
  const ios = where as Navigator & { readonly standalone?: boolean };
  return ios.standalone === true || matchMedia("(display-mode: standalone)").matches;
}

/**
 * Whether the intro's "Open this page from your Home Screen to finish setting
 * up." line shows: it is the install instruction, so only a browser tab shows
 * it. Home hides the whole intro header once it is up.
 */
export function introLineShown(installed: boolean): boolean {
  return !installed;
}
