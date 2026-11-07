/**
 * Where the page's device token comes from. The page ships only
 * [`memoryTokenStore`]: it presents no token, so the daemon answers `Welcome`
 * and then `DevicePairFailed`.
 */
export interface TokenStore {
  get(): Promise<string | undefined>;
}

/**
 * A store that answers `initial` forever: nothing is persisted, and a phone
 * that has never paired lands in the connection's `notApproved` state.
 */
export function memoryTokenStore(initial?: string): TokenStore {
  return { get: () => Promise.resolve(initial) };
}
